//! The neutral reader over authored files with independently computed samples.
mod common;
use common::fixtures::{CASES, Case};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{
    CodecId, ContainerReader, Event as Neutral, MediaKind, PacketInfo, RandomAccess, SeekMethod,
    SeekPolicy, SeekRequest, TrackId, TrackInfo,
};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceId, Step, Time,
    WorkBudget,
};
use triergon_isobmff::{
    BoxStep, BoxView, EntryLayout, Limits, Packet, Reader, ReaderEvent, ReaderLimits,
};

fn capabilities(file: &[u8]) -> SourceCapabilities {
    SourceCapabilities {
        length: Some(file.len() as u64),
        random_access: true,
    }
}

fn open(pool: &Budget, file: &[u8]) -> Reader {
    Reader::new(
        pool,
        SourceId(1),
        capabilities(file),
        ReaderLimits::default(),
        4096,
    )
    .unwrap()
}

fn supply(reader: &mut Reader, pool: &Budget, file: &[u8], request: InputRequest, most: usize) {
    let at = request.offset as usize;
    if at >= file.len() {
        reader.finish(request.source, request.token).unwrap();
    } else {
        let end = file.len().min(at + request.maximum.min(most));
        let bytes = Bytes::copy_from_slice(pool, &file[at..end]).unwrap();
        reader
            .provide(
                &InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
            )
            .unwrap();
    }
}

#[derive(Debug, Default)]
struct Reading {
    packets: Vec<Packet>,
    roots: Vec<[u8; 4]>,
    receipts: Vec<triergon_core::container::SeekReceipt>,
    opened: bool,
}

/// Run to the end with the given work per step and bytes per response.
fn read(
    reader: &mut Reader,
    pool: &Budget,
    file: &[u8],
    work: u64,
    most: usize,
) -> Result<Reading, Error> {
    let cancel = AtomicBool::new(false);
    let mut out = Reading::default();
    loop {
        match reader.step(&mut WorkBudget::new(work), &cancel)? {
            Step::Ready(Neutral::Packet(packet)) => out.packets.push(packet),
            Step::Ready(Neutral::Seek(receipt)) => out.receipts.push(receipt),
            Step::Ready(Neutral::Other(ReaderEvent::Box(header))) => out.roots.push(header.kind),
            Step::Ready(Neutral::Other(ReaderEvent::Movie)) => out.opened = true,
            Step::NeedInput(request) => supply(reader, pool, file, request, most),
            Step::Yield => {}
            Step::Backpressure(pressure) => panic!("{pressure:?}"),
            Step::End(_) => return Ok(out),
        }
    }
}

fn assert_samples(case: &Case, packets: &[Packet]) {
    // File order across tracks is the reader's; each track keeps its decode order.
    let mut tracks: Vec<_> = case.samples.iter().map(|s| s.track).collect();
    tracks.sort_unstable();
    tracks.dedup();
    for track in tracks {
        let expected: Vec<_> = case.samples.iter().filter(|s| s.track == track).collect();
        let actual: Vec<_> = packets
            .iter()
            .filter(|p| p.track() == TrackId(u64::from(track)))
            .collect();
        assert_eq!(expected.len(), actual.len(), "{}", case.name);
        for (want, got) in expected.iter().zip(actual) {
            assert_eq!(got.sample().ordinal, want.ordinal, "{}", case.name);
            assert_eq!(
                got.sample().description_index,
                want.description,
                "{}",
                case.name
            );
            assert_eq!(got.sample().range.start, want.offset, "{}", case.name);
            assert_eq!(got.dts(), Some(want.dts as i64), "{}", case.name);
            assert_eq!(got.pts(), Some(want.cts as i64), "{}", case.name);
            assert_eq!(got.duration(), Some(i64::from(want.delta)), "{}", case.name);
            assert_eq!(got.payload(), want.payload, "{}", case.name);
            assert_eq!(
                got.random_access() == RandomAccess::Sync,
                want.sync,
                "{}",
                case.name
            );
        }
    }
}

#[test]
fn packets_match_independent_sample_facts_for_any_window_and_work() {
    let mut read_cases = 0;
    for case in CASES.iter().filter(|c| !c.malformed && !c.gap) {
        for (work, most) in [(1 << 20, 1 << 16), (7, 1 << 16), (1 << 20, 1), (3, 5)] {
            let pool = Budget::new(16 << 20).unwrap();
            let mut reader = open(&pool, case.bytes);
            let reading = read(&mut reader, &pool, case.bytes, work, most)
                .unwrap_or_else(|e| panic!("{}: {e}", case.name));
            assert!(reading.opened, "{}", case.name);
            assert_samples(case, &reading.packets);
        }
        read_cases += 1;
    }
    assert!(read_cases > 10);
}

#[test]
fn a_malformed_file_is_an_error_and_stays_one() {
    let mut refused = 0;
    for case in CASES.iter().filter(|c| c.malformed) {
        let pool = Budget::new(16 << 20).unwrap();
        let mut reader = open(&pool, case.bytes);
        if let Err(error) = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16) {
            refused += 1;
            let cancel = AtomicBool::new(false);
            assert_eq!(
                reader.step(&mut WorkBudget::new(16), &cancel).unwrap_err(),
                error,
                "{}",
                case.name
            );
        }
    }
    assert!(refused > 0);
}

fn patched(name: &str, find: &[u8], at: usize, bytes: &[u8]) -> Vec<u8> {
    let mut file = CASES
        .iter()
        .find(|c| c.name == name)
        .unwrap()
        .bytes
        .to_vec();
    let start = file.windows(find.len()).position(|w| w == find).unwrap() + at;
    file[start..start + bytes.len()].copy_from_slice(bytes);
    file
}

#[test]
fn colour_from_first_visual_entry_preserves_codes_and_range_presence() {
    use triergon_core::container::{ColourRange, Origin};
    for (name, codes, range, origin) in [
        (
            "r-colour-nclx",
            (9, 16, 9),
            ColourRange::Full,
            Origin::Container,
        ),
        (
            "r-colour-nclc",
            (1, 1, 1),
            ColourRange::Unspecified,
            Origin::Absent,
        ),
    ] {
        let file = CASES.iter().find(|c| c.name == name).unwrap().bytes;
        let pool = Budget::new(16 << 20).unwrap();
        let mut reader = open(&pool, file);
        read(&mut reader, &pool, file, 3, 5).unwrap();
        let colour = reader.tracks()[0].colour().unwrap();
        assert_eq!(
            (
                colour.primaries.value,
                colour.transfer.value,
                colour.matrix.value
            ),
            codes,
        );
        assert_eq!((colour.range.value, colour.range.origin), (range, origin));
    }
}

#[test]
fn track_colour_does_not_take_values_from_a_later_description() {
    let file = include_bytes!("fixtures/r-colour-config-change.mov");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, file);
    let reading = read(&mut reader, &pool, file, 3, 5).unwrap();
    let selected: Vec<_> = reading
        .packets
        .iter()
        .map(|packet| packet.sample().description_index)
        .collect();
    assert_eq!(selected, [1, 1, 1, 2, 2, 2]);
    let track = &reader.tracks()[0];
    assert!(track.colour().is_none());
    assert_eq!(track.descriptions().len(), 2);
    assert!(
        track.descriptions()[1]
            .raw()
            .bytes()
            .windows(4)
            .any(|kind| kind == b"colr")
    );
    let description = track
        .descriptions()
        .iter()
        .find(|description| Some(&description.index) == selected.last())
        .unwrap();
    let mut children = description
        .extensions(EntryLayout::Visual, Limits::default())
        .unwrap();
    let mut colour = None;
    loop {
        match children.poll(&mut WorkBudget::new(1)).unwrap() {
            BoxStep::Box(child) => {
                if let Some(found) = child.colour_parameter() {
                    assert!(colour.replace(found).is_none());
                }
            }
            BoxStep::End => break,
            BoxStep::Yield => panic!("one unit should admit a child"),
        }
    }
    let colour = colour.unwrap();
    use triergon_core::container::{ColourRange, Origin};
    assert_eq!(
        (colour.primaries.value, colour.primaries.origin),
        (9, Origin::Container)
    );
    assert_eq!(
        (colour.transfer.value, colour.transfer.origin),
        (16, Origin::Container)
    );
    assert_eq!(
        (colour.matrix.value, colour.matrix.origin),
        (9, Origin::Container)
    );
    assert_eq!(
        (colour.range.value, colour.range.origin),
        (ColourRange::Unspecified, Origin::Absent)
    );
    assert!(colour.chroma_siting.is_none());
    assert!(colour.mastering.is_none());
    assert!(colour.content_light.is_none());
}

#[test]
fn colour_parameter_rejects_other_and_unsupported_boxes() {
    let pool = Budget::new(1024).unwrap();
    for bytes in [
        &b"\0\0\0\x12freenclc\0\x09\0\x10\0\x09"[..],
        &b"\0\0\0\x0ccolrprof"[..],
        &b"\0\0\0\x0bcolrncl"[..],
    ] {
        let view = BoxView::new(Bytes::copy_from_slice(&pool, bytes).unwrap(), 0).unwrap();
        assert!(view.colour_parameter().is_none());
    }
}

#[test]
fn absent_or_unsupported_colour_does_not_change_opening() {
    let original = CASES
        .iter()
        .find(|c| c.name == "r-colour-nclx")
        .unwrap()
        .bytes;
    let at = original.windows(4).position(|w| w == b"colr").unwrap();
    for mutation in 0..4 {
        let mut file = original.to_vec();
        match mutation {
            0 => file[at..at + 4].copy_from_slice(b"free"),
            1 => file[at + 14] |= 1, // reserved bits
            2 => file[at + 4..at + 8].copy_from_slice(b"prof"),
            _ => {
                file[at - 4..at + 4].copy_from_slice(b"\0\0\0\x08colr");
                file[at + 4..at + 12].copy_from_slice(b"\0\0\0\x0bcolr");
            }
        }
        let pool = Budget::new(16 << 20).unwrap();
        let mut reader = open(&pool, &file);
        read(&mut reader, &pool, &file, 3, 5).unwrap();
        assert!(reader.tracks()[0].colour().is_none(), "mutation {mutation}");
    }
}

#[test]
fn a_supported_colour_box_can_follow_an_unknown_colour_type() {
    let mut file = include_bytes!("fixtures/r-colour-nclx.mov").to_vec();
    let extra = *b"\0\0\0\x0ccolrzzzz";
    let added = u32::try_from(extra.len()).unwrap();
    let position = |tag: &[u8]| file.windows(4).position(|bytes| bytes == tag).unwrap();
    let colr = position(b"colr") - 4;
    let chunks = position(b"stco") + 12;
    let sizes = [
        b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"raw ",
    ]
    .map(|tag| position(tag) - 4);
    for size_at in sizes {
        let size = u32::from_be_bytes(file[size_at..size_at + 4].try_into().unwrap());
        file[size_at..size_at + 4].copy_from_slice(&(size + added).to_be_bytes());
    }
    let count = usize::try_from(u32::from_be_bytes(
        file[chunks - 4..chunks].try_into().unwrap(),
    ))
    .unwrap();
    for chunk in file[chunks..chunks + count * 4].chunks_exact_mut(4) {
        let offset = u32::from_be_bytes((*chunk).try_into().unwrap());
        chunk.copy_from_slice(&(offset + added).to_be_bytes());
    }
    file.splice(colr..colr, extra);
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    read(&mut reader, &pool, &file, 3, 5).unwrap();
    let description = reader.tracks()[0].colour().unwrap();
    assert_eq!(description.primaries.value, 9);
    assert_eq!(
        description.range.value,
        triergon_core::container::ColourRange::Full
    );
}

#[test]
fn a_named_track_reports_its_tag_record_and_clock() {
    let file = include_bytes!("fixtures/encoded-config-v3/creation-all.mp4");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, file);
    let reading = read(&mut reader, &pool, file, 1 << 20, 1 << 16).unwrap();
    assert_eq!(reader.major_brand(), Some(*b"isom"));
    let video = reader.track(0).unwrap();
    assert_eq!(video.id(), TrackId(1));
    assert_eq!(video.kind(), MediaKind::Video);
    assert_eq!(video.codec(), CodecId::H264);
    assert_eq!(video.codec_tag(), b"avc1");
    assert_eq!(video.configuration()[0], 1, "AVCDecoderConfigurationRecord");
    assert_eq!(video.language(), Some("und"));
    let audio = reader.track(1).unwrap();
    assert_eq!(audio.kind(), MediaKind::Audio);
    assert_eq!(audio.codec(), CodecId::Aac);
    assert_eq!(audio.codec_tag(), b"mp4a");
    assert_eq!(audio.object_type(), Some(0x40));
    assert_eq!(audio.configuration().len(), 5, "AudioSpecificConfig");
    let timescale = u64::from(audio.header().timescale);
    let packet = reading
        .packets
        .iter()
        .find(|p| p.track() == TrackId(2))
        .unwrap();
    assert_eq!(packet.time_base().denominator(), timescale);
}

#[test]
fn an_unknown_sample_entry_is_reported_and_its_samples_still_read() {
    let name = "r-front-stco";
    let case = CASES.iter().find(|c| c.name == name).unwrap();
    let file = patched(name, b"stsd", 16, b"xyz1");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    let reading = read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap();
    let track = reader.track(0).unwrap();
    assert_eq!(track.codec(), CodecId::Other(b"xyz1"));
    assert_eq!(track.codec_tag(), b"xyz1");
    assert_eq!(track.kind(), MediaKind::Video);
    assert_eq!(
        track.configuration(),
        track.descriptions()[0].raw().payload(),
        "an entry of unknown layout stands in whole"
    );
    assert_samples(case, &reading.packets);
}

#[test]
fn an_opus_entry_is_named_and_a_missing_record_stays_empty() {
    let mut file = patched("r-front-stco", b"stsd", 16, b"Opus");
    let handler = file.windows(4).position(|w| w == b"vide").unwrap();
    file[handler..handler + 4].copy_from_slice(b"soun");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap();
    let track = reader.track(0).unwrap();
    assert_eq!(track.kind(), MediaKind::Audio);
    assert_eq!(track.codec(), CodecId::Opus);
    assert_eq!(track.codec_tag(), b"Opus");
    assert!(track.configuration().is_empty());
}

#[test]
fn a_flac_entry_is_named_with_its_dfla_record() {
    let mut file = include_bytes!("fixtures/encoded-v1/control-ac3-front.mp4").to_vec();
    for (from, to) in [(b"ac-3", b"fLaC"), (b"dac3", b"dfLa")] {
        let at = file.windows(4).position(|w| w == from).unwrap();
        file[at..at + 4].copy_from_slice(to);
    }
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap();
    let track = reader.track(0).unwrap();
    assert_eq!(track.codec(), CodecId::Flac);
    assert_eq!(track.codec_tag(), b"fLaC");
    assert_eq!(
        track.configuration(),
        [0x10, 0x09, 0x40],
        "the dfLa payload"
    );
}

#[test]
fn an_mp3_entry_is_named_without_a_record() {
    let mut file = include_bytes!("fixtures/encoded-v1/control-ac3-front.mp4").to_vec();
    let at = file.windows(4).position(|w| w == b"ac-3").unwrap();
    file[at..at + 4].copy_from_slice(b".mp3");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap();
    let track = reader.track(0).unwrap();
    assert_eq!(track.codec(), CodecId::MpegAudio);
    assert_eq!(track.codec_tag(), b".mp3");
}

#[test]
fn an_unknown_handler_is_not_given_a_kind() {
    let file = patched("r-front-stco", b"vide", 0, b"xyz1");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap();
    let track = reader.track(0).unwrap();
    assert_eq!(track.kind(), MediaKind::Unknown);
    assert_eq!(track.codec(), CodecId::Other(track.codec_tag()));
}

#[test]
fn media_in_another_file_is_refused() {
    let file = patched("r-front-stco", b"url ", 7, &[0]);
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    assert_eq!(
        read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap_err(),
        Error::Unsupported("external source data reference")
    );
}

#[test]
fn a_time_beyond_the_neutral_range_is_an_error_not_a_sentinel() {
    let file = include_bytes!("fixtures/f-tfdt-above-i64.mp4");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, file);
    assert_eq!(
        read(&mut reader, &pool, file, 1 << 20, 1 << 16).unwrap_err(),
        Error::Overflow
    );
}

#[test]
fn edits_and_extra_entries_stay_on_the_track() {
    let file = include_bytes!("fixtures/r-edits-repeat.mov");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, file);
    let reading = read(&mut reader, &pool, file, 1 << 20, 1 << 16).unwrap();
    let track = reader.track(0).unwrap();
    assert!(track.edits().is_some());
    assert_eq!(reading.packets.len() as u64, track.table().sample_count());

    let file = include_bytes!("fixtures/r-description-change.mov");
    let mut reader = open(&pool, file);
    let reading = read(&mut reader, &pool, file, 1 << 20, 1 << 16).unwrap();
    assert!(reader.track(0).unwrap().descriptions().len() > 1);
    let first = reading.packets[0].sample().description_index;
    assert!(
        reading
            .packets
            .iter()
            .any(|p| p.sample().description_index != first)
    );
}

#[test]
fn a_fragment_without_a_decode_time_has_unknown_times() {
    let file = include_bytes!("fixtures/f-no-second-tfdt.mp4");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, file);
    let reading = read(&mut reader, &pool, file, 1 << 20, 1 << 16).unwrap();
    assert!(reading.packets[0].dts().is_some());
    let last = reading.packets.last().unwrap();
    assert_eq!((last.dts(), last.pts()), (None, None));
    assert!(last.duration().is_some());
}

#[test]
fn tracks_appear_only_once_the_movie_is_open() {
    let file = include_bytes!("fixtures/encoded-config-v3/creation-all.mp4");
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, file);
    let cancel = AtomicBool::new(false);
    loop {
        match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            Step::Ready(Neutral::Other(ReaderEvent::Movie)) => break,
            Step::NeedInput(request) => supply(&mut reader, &pool, file, request, 1 << 16),
            _ => assert_eq!(reader.track_count(), 0),
        }
    }
    assert_eq!(reader.track_count(), 2);
}

#[test]
fn every_root_box_is_reported() {
    let case = CASES.iter().find(|c| c.name == "r-front-stco").unwrap();
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let reading = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    let expected: Vec<_> = common::roots(&pool, case.bytes)
        .unwrap()
        .iter()
        .map(|root| root.header().kind)
        .collect();
    assert_eq!(reading.roots, expected);
    assert!(reader.movie_box().is_some());
    assert!(reader.movie_header().is_some());
    assert!(!reader.media_extents().is_empty());
}

#[test]
fn a_source_without_random_access_is_refused() {
    let pool = Budget::new(1 << 20).unwrap();
    let forward = SourceCapabilities {
        length: None,
        random_access: false,
    };
    assert!(matches!(
        Reader::new(&pool, SourceId(1), forward, ReaderLimits::default(), 4096),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn a_file_without_a_movie_is_refused() {
    let pool = Budget::new(1 << 20).unwrap();
    let file = [&[0, 0, 0, 8][..], b"free"].concat();
    let mut reader = open(&pool, &file);
    assert_eq!(
        read(&mut reader, &pool, &file, 1 << 20, 1 << 16).unwrap_err(),
        Error::Invalid("MP4 missing moov")
    );
}

#[test]
fn cancellation_is_sticky_until_reset() {
    let case = CASES.iter().find(|c| c.name == "r-front-stco").unwrap();
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        reader.step(&mut WorkBudget::new(16), &cancel).unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        reader.step(&mut WorkBudget::new(16), &cancel).unwrap_err(),
        Error::Cancelled
    );
    reader.reset().unwrap();
    let reading = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    assert_samples(case, &reading.packets);
}

#[test]
fn reset_reads_the_same_packets_again() {
    let case = CASES
        .iter()
        .find(|c| c.fragmented && !c.gap && !c.malformed)
        .unwrap();
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let first = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    reader.reset().unwrap();
    assert_eq!(reader.track_count(), 0);
    let second = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    assert_eq!(first.packets.len(), second.packets.len());
    assert_samples(case, &second.packets);
}

fn seek_case() -> &'static Case {
    CASES
        .iter()
        .filter(|c| !c.malformed && !c.gap)
        .max_by_key(|c| c.samples.iter().filter(|s| s.sync).count())
        .unwrap()
}

#[test]
fn a_seek_lands_on_the_latest_sync_sample_at_or_before_the_target() {
    let case = seek_case();
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let opened = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    let track = opened.packets[0].track();
    let base = opened.packets[0].time_base();
    let id = track.0 as u32;
    let syncs: Vec<_> = case
        .samples
        .iter()
        .filter(|s| s.track == id && s.sync)
        .collect();
    assert!(syncs.len() > 1, "{}", case.name);
    let chosen = syncs[syncs.len() - 1];
    for policy in [
        SeekPolicy::PreviousRandomAccess,
        SeekPolicy::PlaybackPreroll,
    ] {
        let target = Time::from_ticks(chosen.cts + 1, base).unwrap();
        reader
            .request_seek(SeekRequest {
                track,
                target,
                policy,
            })
            .unwrap();
        let reading = read(&mut reader, &pool, case.bytes, 3, 1 << 16).unwrap();
        let receipt = reading.receipts[0];
        assert_eq!(receipt.requested, target);
        assert_eq!(receipt.landing, Time::from_ticks(chosen.cts, base).unwrap());
        assert_eq!(receipt.method, SeekMethod::Index);
        assert_eq!(
            receipt.discard_until,
            match policy {
                SeekPolicy::PreviousRandomAccess => receipt.landing,
                SeekPolicy::PlaybackPreroll => target,
            }
        );
        let first = reading.packets.iter().find(|p| p.track() == track).unwrap();
        assert_eq!(first.sample().ordinal, chosen.ordinal);
        assert_eq!(first.random_access(), RandomAccess::Sync);
    }
}

#[test]
fn a_seek_before_the_first_sync_sample_reports_where_it_landed() {
    let case = seek_case();
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let opened = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    let total = opened.packets.len();
    let track = opened.packets[0].track();
    let base = opened.packets[0].time_base();
    let first = case
        .samples
        .iter()
        .find(|s| s.track == track.0 as u32 && s.sync)
        .unwrap();
    let target = Time::from_ticks(first.cts - 1, base).unwrap();
    reader
        .request_seek(SeekRequest {
            track,
            target,
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    let reading = read(&mut reader, &pool, case.bytes, 1 << 20, 1 << 16).unwrap();
    assert_eq!(
        reading.receipts[0].landing,
        Time::from_ticks(first.cts, base).unwrap()
    );
    assert_eq!(
        reading.packets.len(),
        total,
        "delivery restarts from the first samples"
    );
    assert!(reading.receipts[0].replays_from_start);
}

#[test]
fn a_seek_needs_an_open_track() {
    let case = seek_case();
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let request = SeekRequest {
        track: TrackId(1),
        target: Time::new(0, 1).unwrap(),
        policy: SeekPolicy::PreviousRandomAccess,
    };
    assert!(matches!(
        reader.request_seek(request),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn backpressure_resumes_once_output_is_released() {
    let case = CASES.iter().find(|c| c.name == "r-front-stco").unwrap();
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = open(&pool, case.bytes);
    let cancel = AtomicBool::new(false);
    let window_cost = {
        let before = pool.used();
        let window = Bytes::copy_from_slice(&pool, &[0; 3]).unwrap();
        let cost = pool.used() - before;
        drop(window);
        cost
    };
    let mut ballast = None;
    let mut pressured = 0;
    let mut packets = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(1 << 20), &cancel).unwrap() {
            Step::Ready(Neutral::Packet(packet)) => packets.push(packet),
            Step::Ready(Neutral::Other(ReaderEvent::Movie)) => {
                // Room for a 3-byte window, but not for a sample split across them.
                let spare = pool.limit() - pool.used();
                let held = vec![0; spare - 2 * window_cost - 2];
                ballast = Some(Bytes::copy_from_slice(&pool, &held).unwrap());
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => supply(&mut reader, &pool, case.bytes, request, 3),
            Step::Backpressure(_) => {
                pressured += 1;
                ballast = None;
            }
            Step::End(_) => break,
        }
    }
    assert!(ballast.is_none());
    assert_eq!(pressured, 1);
    assert_samples(case, &packets);
}

fn hdr_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(payload.len() + 8).unwrap().to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

#[test]
fn static_hdr_preserves_encoded_values_and_rejects_truncation() {
    let pool = Budget::new(4096).unwrap();
    for (kind, len) in [(b"mdcv", 24), (b"clli", 4)] {
        for n in 0..len {
            let raw = hdr_box(kind, &vec![0xff; n]);
            let view = BoxView::new(Bytes::copy_from_slice(&pool, &raw).unwrap(), 0).unwrap();
            assert!(view.mastering_display_colour_volume().is_none());
            assert!(view.content_light_level().is_none());
        }
    }
    let raw = hdr_box(
        b"mdcv",
        &[
            0, 0, 0xff, 0xff, 0, 5, 0xa4, 0x10, 0x90, 0x88, 0, 4, 0, 1, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0, 0, 0, 0,
        ],
    );
    let view = BoxView::new(Bytes::copy_from_slice(&pool, &raw).unwrap(), 0).unwrap();
    let value = view.mastering_display_colour_volume().unwrap();
    assert_eq!(
        value.display_primaries,
        [[0, 65535], [5, 42000], [37000, 4]]
    );
    assert_eq!(value.white_point, [1, 65535]);
    assert_eq!(value.max_display_mastering_luminance, u32::MAX);
    assert_eq!(value.min_display_mastering_luminance, 0);
    assert!(view.content_light_level().is_none());
    let raw = hdr_box(b"clli", &[0, 0, 0xff, 0xff]);
    let view = BoxView::new(Bytes::copy_from_slice(&pool, &raw).unwrap(), 0).unwrap();
    let value = view.content_light_level().unwrap();
    assert_eq!(
        (
            value.max_content_light_level,
            value.max_pic_average_light_level
        ),
        (0, u16::MAX)
    );
    assert!(view.mastering_display_colour_volume().is_none());
}

fn static_hdr_movie(description_index: usize) -> Vec<u8> {
    let mut file = include_bytes!("fixtures/r-colour-config-change.mov").to_vec();
    let mut mastering: Vec<u8> = [
        10_000u16, 20_000, 15_000, 30_000, 7500, 3000, 15_635, 16_450,
    ]
    .into_iter()
    .flat_map(u16::to_be_bytes)
    .collect();
    mastering.extend(10_000_000u32.to_be_bytes());
    mastering.extend(50u32.to_be_bytes());
    let mut hdr = hdr_box(b"mdcv", &mastering);
    hdr.extend(hdr_box(b"clli", &[3, 232, 1, 144]));
    let added = u32::try_from(hdr.len()).unwrap();
    // The authored movie stores both visual descriptions before the media data.
    let stsd = file.windows(4).position(|v| v == b"stsd").unwrap() - 4;
    let first = stsd + 16;
    let second = first + u32::from_be_bytes(file[first..first + 4].try_into().unwrap()) as usize;
    let entry = [first, second][description_index];
    let size = u32::from_be_bytes(file[entry..entry + 4].try_into().unwrap());
    let end = entry + size as usize;
    for kind in [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"] {
        let at = file.windows(4).position(|v| v == kind).unwrap() - 4;
        let size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap());
        file[at..at + 4].copy_from_slice(&(size + added).to_be_bytes());
    }
    file[entry..entry + 4].copy_from_slice(&(size + added).to_be_bytes());
    let stco = file.windows(4).position(|v| v == b"stco").unwrap() - 4;
    let count = u32::from_be_bytes(file[stco + 12..stco + 16].try_into().unwrap()) as usize;
    for at in (stco + 16..stco + 16 + count * 4).step_by(4) {
        let offset = u32::from_be_bytes(file[at..at + 4].try_into().unwrap());
        file[at..at + 4].copy_from_slice(&(offset + added).to_be_bytes());
    }
    file.splice(end..end, hdr);
    file
}

#[test]
fn selected_description_exposes_both_static_hdr_boxes() {
    let file = static_hdr_movie(1);
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open(&pool, &file);
    let reading = read(&mut reader, &pool, &file, 3, 5).unwrap();
    let selected = reading.packets.last().unwrap().sample().description_index;
    assert_eq!(selected, 2);
    let description = reader.tracks()[0]
        .descriptions()
        .iter()
        .find(|d| d.index == selected)
        .unwrap();
    let first_description = &reader.tracks()[0].descriptions()[0];
    let mut first_children = first_description
        .extensions(EntryLayout::Visual, Limits::default())
        .unwrap();
    loop {
        match first_children.poll(&mut WorkBudget::new(1)).unwrap() {
            BoxStep::Box(child) => {
                assert!(child.mastering_display_colour_volume().is_none());
                assert!(child.content_light_level().is_none());
            }
            BoxStep::End => break,
            BoxStep::Yield => panic!("one unit admits a child"),
        }
    }
    let mut limited = description
        .extensions(
            EntryLayout::Visual,
            Limits {
                max_boxes: 0,
                ..Limits::default()
            },
        )
        .unwrap();
    assert!(limited.poll(&mut WorkBudget::new(1)).is_err());
    let mut children = description
        .extensions(EntryLayout::Visual, Limits::default())
        .unwrap();
    assert!(matches!(
        children.poll(&mut WorkBudget::new(0)).unwrap(),
        BoxStep::Yield
    ));
    let (mut mastering, mut light) = (None, None);
    loop {
        match children.poll(&mut WorkBudget::new(1)).unwrap() {
            BoxStep::Box(child) => {
                if let Some(value) = child.mastering_display_colour_volume() {
                    assert!(mastering.replace(value).is_none());
                }
                if let Some(value) = child.content_light_level() {
                    assert!(light.replace(value).is_none());
                }
            }
            BoxStep::End => break,
            BoxStep::Yield => panic!("one unit admits a child"),
        }
    }
    let mastering = mastering.unwrap();
    assert_eq!(
        mastering.display_primaries,
        [[10_000, 20_000], [15_000, 30_000], [7500, 3000]]
    );
    assert_eq!(mastering.white_point, [15_635, 16_450]);
    assert_eq!(
        (
            mastering.max_display_mastering_luminance,
            mastering.min_display_mastering_luminance
        ),
        (10_000_000, 50)
    );
    let light = light.unwrap();
    assert_eq!(
        (
            light.max_content_light_level,
            light.max_pic_average_light_level
        ),
        (1000, 400)
    );
    assert!(reader.tracks()[0].colour().is_none());
}

#[test]
#[ignore = "requires the prepared formats ffprobe"]
fn static_hdr_matches_stream_side_data_for_either_description() {
    let reference = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/reference-formats/install/bin/ffprobe");
    for description_index in 0..2 {
        let directory = std::env::temp_dir().join(format!(
            "triergon-isobmff-hdr-{}-{description_index}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("hdr.mov");
        std::fs::write(&path, static_hdr_movie(description_index)).unwrap();
        let output = std::process::Command::new(&reference)
            .args([
                "-v",
                "error",
                "-show_entries",
                "packet=stream_index:packet_side_data=side_data_type:stream=codec_type:stream_side_data",
                "-of",
                "compact=p=1:nk=0",
            ])
            .arg(&path)
            .output();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(directory).unwrap();
        let output = output.expect("prepare the pinned formats ffprobe");
        assert!(output.status.success(), "{:?}", output.stderr);
        let output = String::from_utf8(output.stdout).unwrap();
        let mut lines = output.lines().filter(|line| !line.is_empty());
        // No packet carries either static HDR box as side data.
        for _ in 0..6 {
            assert_eq!(lines.next(), Some("packet|stream_index=0"));
        }
        let stream = lines.next().unwrap();
        assert!(stream.starts_with("stream|codec_type=video|"));
        assert_eq!(lines.next(), None);
        let fields: std::collections::BTreeMap<_, _> = stream
            .split('|')
            .filter_map(|field| field.rsplit(':').next()?.split_once('='))
            .collect();
        for (field, expected) in [
            ("green_x", "10000/50000"),
            ("green_y", "20000/50000"),
            ("blue_x", "15000/50000"),
            ("blue_y", "30000/50000"),
            ("red_x", "7500/50000"),
            ("red_y", "3000/50000"),
            ("white_point_x", "15635/50000"),
            ("white_point_y", "16450/50000"),
            ("max_luminance", "10000000/10000"),
            ("min_luminance", "50/10000"),
            ("max_content", "1000"),
            ("max_average", "400"),
        ] {
            assert_eq!(fields[field], expected, "{field}");
        }
    }
}
