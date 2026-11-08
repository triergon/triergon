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
use triergon_isobmff::{Packet, Reader, ReaderEvent, ReaderLimits};

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
