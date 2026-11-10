mod common;
use common::{child, children, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    BuildStatus, ConfigurationLimits, EditList, Limits, MovieHeader, SampleTable, TrackHeader,
    check_dts_core_description,
    compat::mov::{
        edits::{
            EditCursor, EditLimits, EncodedEditContext, EncodedEditJob, EncodedEditLimits,
            EncodedEditPlan, EncodedEditProfile,
        },
        encoded::{
            EncodedConfigProfile, EncodedConfigSequence, EncodedContextProfile, EncodedEntryKind,
            EncodedMetadataProfile, EncodedRecoveryProfile, EncodedSelection, EncodedStream,
            EncodedStreamJob, EncodedStreamProfile,
        },
        packets::{CursorStatus, DemuxTrack, PacketCursor, PacketLimits},
        seek::{GlobalSeekJob, IndexCursor, IndexLimits},
    },
};
const ORIGINAL: &[u8] = include_bytes!("fixtures/encoded-edit-v2/original.mp4");
const AUDIO0: &[u8] = include_bytes!("fixtures/encoded-edit-v2/audio0.mp4");
const AUDIO1024: &[u8] = include_bytes!("fixtures/encoded-edit-v2/audio1024.mp4");
const AUDIO2048: &[u8] = include_bytes!("fixtures/encoded-edit-v2/audio2048.mp4");
const VIDEO0: &[u8] = include_bytes!("fixtures/encoded-edit-v2/video0.mp4");
const VIDEO2673: &[u8] = include_bytes!("fixtures/encoded-edit-v2/video2673.mp4");
const LEADING_AC3: &[u8] = include_bytes!("fixtures/range-encoded-edits/leading-ac3.mp4");
const LEADING_EAC3: &[u8] = include_bytes!("fixtures/range-encoded-edits/leading-eac3.mp4");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    edits: EditList,
    media: Vec<Range<u64>>,
    length: u64,
    movie_base: TimeBase,
}
impl Source {
    fn new(data: &[u8], handler: &[u8; 4]) -> Self {
        Self::in_pool(data, handler, Budget::new(1 << 22).unwrap())
    }
    fn in_pool(data: &[u8], handler: &[u8; 4], pool: Budget) -> Self {
        let top = roots(&pool, data).unwrap();
        let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
        let track = children(movie, 0)
            .unwrap()
            .into_iter()
            .filter(|x| x.header().kind == *b"trak")
            .find(|x| {
                child(&child(x, b"mdia").unwrap(), b"hdlr")
                    .unwrap()
                    .payload()
                    .get(8..12)
                    == Some(handler.as_slice())
            })
            .unwrap();
        let mdia = child(&track, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = table(
            child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
            &pool,
            1,
        )
        .unwrap();
        let edits = EditList::parse(
            child(&child(&track, b"edts").unwrap(), b"elst").unwrap(),
            Limits::default(),
        )
        .unwrap();
        let media = top
            .iter()
            .filter(|x| x.header().kind == *b"mdat")
            .map(|x| x.header().payload_range().unwrap())
            .collect();
        Self {
            pool,
            header,
            table,
            edits,
            media,
            length: data.len() as u64,
            movie_base: MovieHeader::parse(&child(movie, b"mvhd").unwrap())
                .unwrap()
                .time_base()
                .unwrap(),
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(917),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn stream(&self, i: &mut InputCursor) -> EncodedStream {
        let mut j = EncodedStreamJob::new(
            &self.header,
            &self.table,
            i,
            Some(*b"isom"),
            ConfigurationLimits::default(),
            EncodedStreamProfile,
        )
        .unwrap();
        for _ in 0..100000 {
            if j.poll(i, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == CursorStatus::Ready
            {
                return j.take(i, &AtomicBool::new(false)).unwrap();
            }
        }
        panic!("stream progress")
    }
    fn job<'a>(
        &'a self,
        s: &'a EncodedStream,
        i: &InputCursor,
        maximum: u64,
    ) -> Result<EncodedEditJob<'a>, Error> {
        EncodedEditJob::new(
            EncodedEditContext {
                table: &self.table,
                edits: &self.edits,
                stream: s,
                media_base: self.header.time_base().unwrap(),
                movie_base: self.movie_base,
            },
            i,
            EncodedEditLimits {
                max_samples: maximum,
            },
            EncodedEditProfile,
        )
    }
    fn plan(&self, q: u64) -> Result<EncodedEditPlan, Error> {
        let mut i = self.input();
        let s = self.stream(&mut i);
        complete(&mut self.job(&s, &i, 1000000)?, &mut i, q)
    }
    fn track(&self) -> DemuxTrack<'_> {
        DemuxTrack {
            track_id: self.header.track_id,
            table: &self.table,
            fragments: &[],
            media_base: self.header.time_base().unwrap(),
            edits: Some((&self.edits, self.movie_base)),
        }
    }
}
fn complete(
    j: &mut EncodedEditJob<'_>,
    i: &mut InputCursor,
    q: u64,
) -> Result<EncodedEditPlan, Error> {
    for _ in 0..100000 {
        let mut w = WorkBudget::new(q);
        let state = j.poll(i, &mut w, &AtomicBool::new(false))?;
        assert!(w.spent() <= q);
        if state == CursorStatus::Ready {
            return j.take(i, &AtomicBool::new(false));
        }
    }
    panic!("edit scan must progress with real work1")
}
fn rows(
    p: &Source,
    plan: EncodedEditPlan,
) -> Vec<(
    triergon_isobmff::compat::mov::packets::PacketOccurrence,
    Option<[u8; 10]>,
)> {
    let plans = [Some(plan)];
    let mut c = PacketCursor::new_encoded_regular(
        &p.pool,
        &[p.track()],
        &p.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    let mut out = Vec::new();
    for _ in 0..100000 {
        let mut w = WorkBudget::new(1);
        match c.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            CursorStatus::Yield => {}
            CursorStatus::Ready => {
                let side = c.ready_encoded_skip().unwrap();
                out.push((c.take_occurrence().unwrap(), side));
            }
            CursorStatus::End => return out,
        }
        assert!(w.spent() <= 1);
    }
    panic!("occurrence progress")
}

fn range_config(
    p: &Source,
    major: [u8; 4],
    input: &mut InputCursor,
    limits: ConfigurationLimits,
) -> Result<EncodedStream, Error> {
    let cancel = AtomicBool::new(false);
    let mut stream = EncodedStreamJob::new_range_playback(
        &p.header,
        &p.table,
        input,
        EncodedSelection {
            major_brand: Some(major),
            description: 1,
            limits,
        },
    )?;
    for _ in 0..10000 {
        if stream.poll(input, &mut WorkBudget::new(1), &cancel)? == CursorStatus::Ready {
            return stream.take(input, &cancel);
        }
    }
    panic!("range description progress")
}

fn range_plan(p: &Source, major: [u8; 4]) -> Result<EncodedEditPlan, Error> {
    let mut input = p.input();
    let configs = [range_config(
        p,
        major,
        &mut input,
        ConfigurationLimits::default(),
    )?];
    let mut edit = EncodedEditJob::new_range_playback(
        EncodedEditContext {
            table: &p.table,
            edits: &p.edits,
            stream: &configs[0],
            media_base: p.header.time_base()?,
            movie_base: p.movie_base,
        },
        &configs,
        &input,
        EncodedEditLimits::default(),
    )?;
    complete(&mut edit, &mut input, 1)
}

#[test]
fn range_playback_keeps_quicktime_visual_decode_order_and_signed_clocks() {
    let raw = include_bytes!("fixtures/range-encoded-edits/quicktime-avc.mov");
    let source = Source::new(raw, b"vide");
    let packets = rows(&source, range_plan(&source, *b"qt  ").unwrap());
    assert_eq!(packets.len(), 6);
    for ((packet, skip), (pts, dts, offset, length, key)) in packets.into_iter().zip([
        (0, -1024, 36, 716, 1),
        (1024, -512, 752, 12, 0),
        (512, 0, 764, 12, 0),
        (1536, 512, 776, 30, 1),
        (2560, 1024, 806, 12, 0),
        (2048, 1536, 818, 12, 0),
    ]) {
        assert_eq!(packet.source.range, offset..offset + length);
        assert_eq!((packet.demux.pts, packet.demux.dts), (Some(pts), Some(dts)));
        assert_eq!((packet.demux.duration, packet.demux.flags), (512, key));
        assert_eq!(skip, None);
    }
}

#[test]
fn range_playback_preserves_long_aac_sample_durations_and_coded_trim() {
    let original = Source::new(ORIGINAL, b"soun");
    let stts = child(original.table.raw(), b"stts").unwrap();
    let start = usize::try_from(stts.header().payload_range().unwrap().start).unwrap();
    let mut raw = ORIGINAL.to_vec();
    // Extend the second sample and shorten the final sample by the same amount.
    for (offset, before, after) in [(20, 1016u32, 1272u32), (36, 528, 272)] {
        let delta = &mut raw[start + offset..start + offset + 4];
        assert_eq!(delta, &before.to_be_bytes());
        delta.copy_from_slice(&after.to_be_bytes());
    }

    let source = Source::new(&raw, b"soun");
    assert!(matches!(
        source.plan(1),
        Err(Error::Unsupported(
            "encoded AAC source packet clock profile"
        ))
    ));
    let packets = rows(&source, range_plan(&source, *b"isom").unwrap());
    assert_eq!(packets.len(), 189);
    assert_eq!(packets[0].1, Some([0xf0, 3, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(packets[1].0.demux.pts, Some(16));
    assert_eq!(packets[1].0.demux.duration, 1272);
    assert_eq!(packets[1].1, None);
    assert_eq!(packets[2].0.demux.dts, Some(1288));
    assert_eq!(packets[188].0.demux.duration, 272);
    assert_eq!(packets[188].1, Some([0, 0, 0, 0, 0xf0, 2, 0, 0, 0, 0]));
}

#[test]
fn range_playback_keeps_dolby_packets_and_reports_the_leading_sample_skip() {
    for raw in [LEADING_AC3, LEADING_EAC3] {
        let source = Source::new(raw, b"soun");
        assert!(matches!(
            source.plan(1),
            Err(Error::Unsupported(
                "encoded edited audio codec frame clock not established"
            ))
        ));
        let plan = range_plan(&source, *b"isom").unwrap();
        let packets = rows(&source, plan);
        assert_eq!(packets.len(), 6);
        for (i, (packet, skip)) in packets.into_iter().enumerate() {
            let offset = 48 + i as u64 * 384;
            let pts = i as i64 * 1536 - 256;
            assert_eq!(packet.source.range, offset..offset + 384);
            assert_eq!((packet.demux.pts, packet.demux.dts), (Some(pts), Some(pts)));
            assert_eq!((packet.demux.duration, packet.demux.flags), (1536, 1));
            assert_eq!(skip, (i == 0).then_some([0, 1, 0, 0, 0, 0, 0, 0, 0, 0]));
        }
    }
}

fn fixture_payload(raw: &[u8], kind: &[u8; 4]) -> usize {
    raw.windows(4).position(|bytes| bytes == kind).unwrap() + 4
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut raw = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    raw.extend_from_slice(kind);
    raw.extend_from_slice(body);
    raw
}

fn ddts(construction: u8, frequency: u32, frame_code: u8) -> Vec<u8> {
    let mut record = [0; 20];
    record[..4].copy_from_slice(&frequency.to_be_bytes());
    record[12] = 24;
    record[13] = frame_code << 6 | construction << 1;
    let core = (2u32 << 18) | (384 << 4); // Stereo, 384-byte core.
    record[14..17].copy_from_slice(&core.to_be_bytes()[1..]);
    record[18] = 2; // Left and right front channels.
    atom(b"ddts", &record)
}

fn mp4a_esds(oti: u8) -> Vec<u8> {
    // MPEG-4 descriptors with FFmpeg-style four-byte lengths and no DSI.
    atom(
        b"esds",
        &[
            0, 0, 0, 0, 3, 0x80, 0x80, 0x80, 27, 0, 1, 0, 4, 0x80, 0x80, 0x80, 13, oti, 0x15, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 0x80, 0x80, 0x80, 1, 2,
        ],
    )
}

fn replace_fixture_body(raw: &mut Vec<u8>, kind: &[u8; 4], parents: &[&[u8; 4]], body: &[u8]) {
    let at = fixture_payload(raw, kind);
    let old_size = u32::from_be_bytes(raw[at - 8..at - 4].try_into().unwrap());
    for name in std::iter::once(&kind).chain(parents) {
        let size_at = fixture_payload(raw, name) - 8;
        let size = u32::from_be_bytes(raw[size_at..size_at + 4].try_into().unwrap());
        let size = size - old_size + 8 + body.len() as u32;
        raw[size_at..size_at + 4].copy_from_slice(&size.to_be_bytes());
    }
    raw.splice(at..at - 8 + old_size as usize, body.iter().copied());
}

fn dts_fixture(
    format: &[u8; 4],
    version: u8,
    config: &[u8],
    rate: u32,
    frame: u32,
    tail: u32,
    edits: &[(u32, i32)],
) -> Vec<u8> {
    // Reuse the bounded six-packet table and its extents. The tests only use
    // metadata. All edited boxes come after mdat, so offsets remain valid.
    let mut raw = LEADING_EAC3.to_vec();
    let at = fixture_payload(&raw, b"ec-3");
    let mut entry = raw[at..at + 28].to_vec();
    entry[9] = version;
    if version == 1 {
        entry[20..22].copy_from_slice(&0xfffeu16.to_be_bytes());
        entry.extend_from_slice(&512u32.to_be_bytes());
        entry.extend_from_slice(&[0; 8]);
        entry.extend_from_slice(&2u32.to_be_bytes());
    }
    entry.extend_from_slice(config);
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(atom(format, &entry));
    let table_parents = [b"stbl", b"minf", b"mdia", b"trak", b"moov"];
    replace_fixture_body(&mut raw, b"stsd", &table_parents, &stsd);

    let stts: Vec<_> = [0u32, 2, 5, frame, 1, tail]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect();
    replace_fixture_body(&mut raw, b"stts", &table_parents, &stts);
    let mut elst = vec![0; 4];
    elst.extend_from_slice(&(edits.len() as u32).to_be_bytes());
    for &(duration, start) in edits {
        elst.extend_from_slice(&duration.to_be_bytes());
        elst.extend_from_slice(&start.to_be_bytes());
        elst.extend_from_slice(&[0, 1, 0, 0]);
    }
    replace_fixture_body(&mut raw, b"elst", &[b"edts", b"trak", b"moov"], &elst);
    let duration = edits.iter().map(|(duration, _)| duration).sum();
    for (kind, field, word) in [
        (b"mdhd", 12, rate),
        (b"mdhd", 16, 5 * frame + tail),
        (b"mvhd", 12, rate),
        (b"mvhd", 16, duration),
        (b"tkhd", 20, duration),
    ] {
        let at = fixture_payload(&raw, kind) + field;
        raw[at..at + 4].copy_from_slice(&word.to_be_bytes());
    }
    raw
}

const OPUS_DURATIONS: [u32; 6] = [960, 480, 1920, 480, 960, 240];

fn opus_fixture(config: &[u8], edit_start: i32, edit_duration: u32) -> Vec<u8> {
    let mut raw = dts_fixture(
        b"Opus",
        0,
        &atom(b"dOps", config),
        48_000,
        960,
        240,
        &[(edit_duration, edit_start)],
    );
    let entry = fixture_payload(&raw, b"Opus");
    raw[entry + 16..entry + 20].copy_from_slice(&[0, 2, 0, 16]);
    raw[entry + 24..entry + 28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
    let mut stts = vec![0, 0, 0, 0, 0, 0, 0, 6];
    for duration in OPUS_DURATIONS {
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&duration.to_be_bytes());
    }
    replace_fixture_body(
        &mut raw,
        b"stts",
        &[b"stbl", b"minf", b"mdia", b"trak", b"moov"],
        &stts,
    );
    let duration = fixture_payload(&raw, b"mdhd") + 16;
    raw[duration..duration + 4].copy_from_slice(&OPUS_DURATIONS.iter().sum::<u32>().to_be_bytes());
    raw
}

#[test]
fn range_playback_uses_the_opus_edit_and_keeps_variable_packet_durations() {
    let dops = [0, 2, 1, 56, 0, 0, 187, 128, 0, 0, 0];
    let raw = opus_fixture(&dops, 720, 4320);
    let source = Source::new(&raw, b"soun");
    let config = range_config(
        &source,
        *b"isom",
        &mut source.input(),
        ConfigurationLimits::default(),
    )
    .unwrap();
    assert_eq!(config.kind(), EncodedEntryKind::Opus);
    assert_eq!(config.configuration_box().payload(), dops);
    assert_eq!(
        config.demux().extradata.as_ref(),
        b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00"
    );
    let plan = range_plan(&source, *b"isom").unwrap();
    assert_eq!(plan.anchor_ticks(), 720);
    assert_eq!(plan.selected_end_ticks(), 5040);
    let packets = rows(&source, plan);
    assert_eq!(packets.len(), OPUS_DURATIONS.len());
    for (i, ((packet, skip), (duration, pts))) in packets
        .iter()
        .zip(
            OPUS_DURATIONS
                .into_iter()
                .zip([-720, 240, 720, 2640, 3120, 4080]),
        )
        .enumerate()
    {
        assert_eq!(packet.demux.pts, Some(pts));
        assert_eq!(packet.demux.dts, Some(pts));
        assert_eq!(packet.demux.duration, i64::from(duration));
        assert_eq!(packet.demux.flags, 1);
        assert_eq!(*skip, (i == 0).then_some([208, 2, 0, 0, 0, 0, 0, 0, 0, 0]));
    }
}

#[test]
fn range_playback_keeps_opus_preroll_and_stops_at_the_edit_end() {
    let dops = [0, 2, 1, 56, 0, 0, 187, 128, 0, 0, 0];
    let raw = opus_fixture(&dops, 1440, 2300);
    let source = Source::new(&raw, b"soun");
    let packets = rows(&source, range_plan(&source, *b"isom").unwrap());
    assert_eq!(
        packets
            .iter()
            .map(|(packet, _)| (packet.demux.pts, packet.demux.flags))
            .collect::<Vec<_>>(),
        [
            (Some(-1440), 5),
            (Some(-480), 5),
            (Some(0), 1),
            (Some(1920), 1)
        ]
    );
    assert_eq!(packets[0].1, Some([160, 5, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(packets[3].1, Some([0, 0, 0, 0, 100, 0, 0, 0, 0, 0]));
    let after_end = opus_fixture(&dops, 5040, 100);
    assert_eq!(
        range_plan(&Source::new(&after_end, b"soun"), *b"isom").unwrap_err(),
        Error::Invalid("Opus edit starts after the last sample")
    );
}

#[test]
fn cropped_opus_keeps_source_and_edit_ordinals_distinct() {
    let mut raw = include_bytes!("fixtures/range-encoded-edits/avc-opus-edit.mp4").to_vec();
    for (at, value) in [
        (1508, 2050_u32),
        (1512, 52_800),
        (1552, 154_896),
        (1780, 20),
        (1788, 140),
    ] {
        raw[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
    let source = Source::new(&raw, b"soun");
    let packets = rows(&source, range_plan(&source, *b"isom").unwrap());
    let (first, _) = &packets[0];
    assert_eq!((first.source.ordinal, first.edit.unwrap().sequence), (5, 1));
    let (retained, _) = &packets[16];
    assert_eq!(
        (retained.source.ordinal, retained.edit.unwrap().sequence),
        (21, 17)
    );
    assert_eq!(retained.source.duration_ticks, 960);
    assert_eq!(retained.demux.duration, 1008);
}

#[test]
fn opus_global_seek_aligns_audio_without_the_range_checkpoints() {
    let raw = include_bytes!("fixtures/range-encoded-edits/avc-opus-edit.mp4");
    let video = Source::new(raw, b"vide");
    let audio = Source::in_pool(raw, b"soun", video.pool.clone());
    let plans = [
        Some(range_plan(&video, *b"isom").unwrap()),
        Some(range_plan(&audio, *b"isom").unwrap()),
    ];
    let recovery = plans[1]
        .as_ref()
        .unwrap()
        .recovery_observation()
        .unwrap()
        .source_result()
        .unwrap()
        .source_scope(triergon_isobmff::RecoveryKind::Roll);
    assert!(recovery.track_descriptions.is_some());
    assert!(recovery.mappings.is_some());
    let tracks = [video.track(), audio.track()];
    let cancel = AtomicBool::new(false);
    let mut seek = GlobalSeekJob::new_encoded_regular_rescaled(
        &video.pool,
        &tracks,
        &video.media,
        (video.header.track_id, 32000),
        triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        &plans,
    )
    .unwrap();
    while seek.poll(&mut WorkBudget::new(1), &cancel).unwrap() == BuildStatus::Yield {}
    let seek = seek.finish().unwrap();
    assert_eq!(seek.landing().selected.timestamp, 32000);
    let mut cursor = seek.cursor().unwrap();
    while cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap() == CursorStatus::Yield {}
    let first = cursor.take_occurrence().unwrap();
    assert_eq!(first.track_id, audio.header.track_id);
    assert_eq!(first.demux.pts, Some(94752));
    assert_eq!(first.source.range.start, 42991);
}

#[test]
fn dts_core_descriptions_agree_with_range_admission() {
    for (tag, version, config, major, configuration_box) in [
        (b"dtsc", 0, ddts(1, 48_000, 0), b"isom", b"ddts"),
        (b"dtsh", 0, ddts(14, 96_000, 1), b"isom", b"ddts"),
        (b"dtsl", 0, ddts(14, 96_000, 1), b"isom", b"ddts"),
        (b"dtsc", 1, vec![], b"qt  ", b"dtsc"),
        (b"mp4a", 0, mp4a_esds(0xa9), b"isom", b"esds"),
        (b"mp4a", 0, mp4a_esds(0xaa), b"isom", b"esds"),
    ] {
        let source = Source::new(
            &dts_fixture(tag, version, &config, 48_000, 512, 384, &[(2944, 0)]),
            b"soun",
        );
        let description = &source.table.descriptions()[0];
        let checked = check_dts_core_description(description, ConfigurationLimits::default())
            .unwrap()
            .unwrap();
        assert_eq!(checked.sample_entry_rate_hz(), 48_000);
        let stream = range_config(
            &source,
            *major,
            &mut source.input(),
            ConfigurationLimits::default(),
        )
        .unwrap();
        assert_eq!(stream.kind(), EncodedEntryKind::Dts);
        assert_eq!(stream.demux().codec, "dts");
        assert!(stream.demux().extradata.is_empty());
        assert_eq!(stream.configuration_box().header().kind, *configuration_box);
        let packets = rows(&source, range_plan(&source, *major).unwrap());
        assert_eq!(packets.len(), 6);
        assert_eq!(packets[5].0.demux.dts, Some(2560));
        assert_eq!(packets[5].0.demux.duration, 384);
        assert_eq!(packets[5].1, Some([0, 0, 0, 0, 128, 0, 0, 0, 0, 0]));
    }

    let aac = Source::new(AUDIO0, b"soun");
    assert_eq!(
        check_dts_core_description(&aac.table.descriptions()[0], ConfigurationLimits::default()),
        Ok(None)
    );
}

#[test]
fn dts_single_packet_edit_needs_a_declared_coded_frame_clock() {
    for (tag, config, has_clock) in [
        (b"mp4a", mp4a_esds(0xa9), false),
        (b"dtsc", ddts(1, 48_000, 0), true),
    ] {
        let mut raw = dts_fixture(tag, 0, &config, 48_000, 512, 384, &[(384, 0)]);
        let stts: Vec<_> = [0u32, 1, 1, 384]
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect();
        replace_fixture_body(
            &mut raw,
            b"stts",
            &[b"stbl", b"minf", b"mdia", b"trak", b"moov"],
            &stts,
        );
        for (kind, field, word) in [(b"stsc", 12, 1u32), (b"stsz", 8, 1), (b"mdhd", 16, 384)] {
            let at = fixture_payload(&raw, kind) + field;
            raw[at..at + 4].copy_from_slice(&word.to_be_bytes());
        }
        let source = Source::new(&raw, b"soun");
        let plan = range_plan(&source, *b"isom");
        if has_clock {
            let packets = rows(&source, plan.unwrap());
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].0.demux.duration, 384);
            assert_eq!(packets[0].1, Some([0, 0, 0, 0, 128, 0, 0, 0, 0, 0]));
        } else {
            assert!(matches!(
                plan,
                Err(Error::Unsupported("DTS edit needs a coded frame clock"))
            ));
        }
    }
}

#[test]
fn dts_core_description_refusals_and_limits_precede_publication() {
    let ddts = ddts(1, 48_000, 0);
    let mut invalid_esds = mp4a_esds(0xa9);
    invalid_esds[26] = 0x11; // DecoderConfigDescriptor StreamType is no longer audio.
    for (tag, config) in [
        (b"dtsc", vec![]),
        (b"dtsc", atom(b"ddts", &ddts[8..27])),
        (b"dtsc", [ddts.clone(), ddts.clone()].concat()),
        (b"dtsc", [ddts[..27].to_vec(), vec![1]].concat()),
        (
            b"dtsh",
            [ddts[..21].to_vec(), vec![19 << 1], ddts[22..].to_vec()].concat(),
        ),
        (
            b"dtsl",
            [ddts[..21].to_vec(), vec![17 << 1], ddts[22..].to_vec()].concat(),
        ),
        (b"mp4a", invalid_esds),
        (b"mp4a", mp4a_esds(0xab)),
    ] {
        let source = Source::new(
            &dts_fixture(tag, 0, &config, 48_000, 512, 384, &[(2944, 0)]),
            b"soun",
        );
        assert!(
            check_dts_core_description(
                &source.table.descriptions()[0],
                ConfigurationLimits::default()
            )
            .is_err()
        );
        assert!(
            range_config(
                &source,
                *b"isom",
                &mut source.input(),
                ConfigurationLimits::default()
            )
            .is_err()
        );
    }
    for (tag, config, limits) in [
        (
            b"dtsc",
            ddts,
            ConfigurationLimits {
                max_config_bytes: 19,
                ..Default::default()
            },
        ),
        (
            b"mp4a",
            mp4a_esds(0xa9),
            ConfigurationLimits {
                max_descriptors: 2,
                ..Default::default()
            },
        ),
        (
            b"mp4a",
            mp4a_esds(0xa9),
            ConfigurationLimits {
                max_child_boxes: 0,
                ..Default::default()
            },
        ),
    ] {
        let source = Source::new(
            &dts_fixture(tag, 0, &config, 48_000, 512, 384, &[(2944, 0)]),
            b"soun",
        );
        assert!(matches!(
            check_dts_core_description(&source.table.descriptions()[0], limits),
            Err(Error::Limit { .. })
        ));
        assert!(matches!(
            range_config(&source, *b"isom", &mut source.input(), limits),
            Err(Error::Limit { .. })
        ));
    }
}

#[test]
fn encoded_views_refuse_caller_supplied_dts_range_configurations() {
    let source = Source::new(
        &dts_fixture(b"mp4a", 0, &mp4a_esds(0xa9), 48_000, 512, 384, &[(2944, 0)]),
        b"soun",
    );
    let mut input = source.input();
    let limits = ConfigurationLimits::default();
    let mut reference = EncodedStreamJob::new(
        &source.header,
        &source.table,
        &input,
        Some(*b"isom"),
        limits,
        EncodedStreamProfile,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        reference.poll(&mut input, &mut WorkBudget::new(4096), &cancel),
        Err(Error::Unsupported(
            "encoded mp4a object-type is not selected AAC"
        )),
    );

    let configs = [range_config(&source, *b"isom", &mut input, limits).unwrap()];
    let context = || EncodedEditContext {
        table: &source.table,
        edits: &source.edits,
        stream: &configs[0],
        media_base: source.header.time_base().unwrap(),
        movie_base: source.movie_base,
    };
    let limits = EncodedEditLimits::default();
    #[allow(clippy::large_stack_arrays)] // A handful of jobs in a test.
    for admission in [
        EncodedEditJob::new(context(), &input, limits, EncodedEditProfile),
        EncodedEditJob::new_with_configs(context(), &configs, &input, limits, EncodedConfigProfile),
        EncodedEditJob::new_with_dependency_table(
            context(),
            &configs,
            &input,
            limits,
            EncodedMetadataProfile,
        ),
        EncodedEditJob::new_with_recovery_groups(
            context(),
            &configs,
            &input,
            limits,
            EncodedRecoveryProfile,
        ),
        EncodedEditJob::new_with_recovery_contexts(
            context(),
            &configs,
            &input,
            limits,
            EncodedContextProfile,
        ),
    ] {
        assert!(matches!(
            admission,
            Err(Error::Unsupported("encoded codec requires range playback"))
        ));
    }
    assert!(matches!(
        EncodedConfigSequence::new(&source.table, &configs, &input, EncodedConfigProfile),
        Err(Error::Unsupported("encoded codec requires range playback"))
    ));
}

#[test]
fn range_dts_edit_and_seek_use_media_ticks_and_sample_entry_trim_units() {
    let raw = dts_fixture(
        b"dtsh",
        0,
        &ddts(14, 96_000, 1),
        144_000,
        1536,
        1152,
        &[(300, -1), (8640, 96)],
    );
    let source = Source::new(&raw, b"soun");
    let plan = range_plan(&source, *b"isom").unwrap();
    assert_eq!(plan.anchor_ticks(), -204);
    assert_eq!(plan.selected_end_ticks(), 8736);
    let packets = rows(&source, plan.clone());
    assert_eq!(packets.len(), 6);
    for (i, (packet, skip)) in packets.iter().enumerate() {
        assert_eq!(packet.demux.pts, Some(204 + i as i64 * 1536));
        assert_eq!(packet.demux.pts, packet.demux.dts);
        assert_eq!(packet.demux.flags, 1);
        assert_eq!(packet.edit.as_ref().unwrap().edit_ordinal, 1);
        assert_eq!(packet.demux.duration, if i == 5 { 1152 } else { 1536 });
        assert_eq!(
            *skip,
            match i {
                0 => Some([32, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                // 128 samples from the shorter stts duration and 32
                // more from the edit's presentation end.
                5 => Some([0, 0, 0, 0, 160, 0, 0, 0, 0, 0]),
                _ => None,
            }
        );
    }

    let plans = [Some(plan)];
    let tracks = [source.track()];
    let cancel = AtomicBool::new(false);
    let mut cursor = PacketCursor::new_encoded_regular(
        &source.pool,
        &tracks,
        &source.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    cursor.enable_range_seek_checkpoints();
    loop {
        match cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
            CursorStatus::Yield => {}
            CursorStatus::Ready => {
                cursor.take_occurrence().unwrap();
            }
            CursorStatus::End => break,
        }
    }
    cursor.rewind_at_end().unwrap();
    let request = (source.header.track_id, 1750);
    let mut checked = cursor
        .prepare_checked_range_seek(
            request.0,
            request.1,
            triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        )
        .unwrap()
        .unwrap();
    while checked.poll(&mut WorkBudget::new(1), &cancel).unwrap() == BuildStatus::Yield {}
    let (mut landed, landing) = checked.finish().unwrap();
    assert_eq!(landing.selected.timestamp, 1740);
    assert_eq!(
        landing.selected.occurrence.source.range,
        packets[1].0.source.range
    );
    assert_eq!(
        landed.poll(&mut WorkBudget::new(128), &cancel).unwrap(),
        CursorStatus::Ready
    );
    assert_eq!(landed.take_occurrence().unwrap().demux.dts, Some(1740));

    let mut fallback = GlobalSeekJob::new_encoded_regular_rescaled(
        &source.pool,
        &tracks,
        &source.media,
        request,
        triergon_isobmff::compat::mov::seek::SeekLimits::default(),
        &plans,
    )
    .unwrap();
    while fallback.poll(&mut WorkBudget::new(1), &cancel).unwrap() == BuildStatus::Yield {}
    let fallback = fallback.finish().unwrap();
    assert_eq!(
        fallback.landing().selected.timestamp,
        landing.selected.timestamp
    );
    assert_eq!(
        fallback.landing().selected.position,
        landing.selected.position
    );
}

#[test]
fn range_playback_refuses_truncated_dec3_descriptors() {
    // Each flag declares bytes beyond the fixture's one three-byte descriptor.
    for (field, flag) in [(1, 1), (4, 2)] {
        let mut raw = LEADING_EAC3.to_vec();
        let config = fixture_payload(&raw, b"dec3");
        raw[config + field] |= flag;
        let source = Source::new(&raw, b"soun");
        assert!(matches!(
            range_plan(&source, *b"isom"),
            Err(Error::Truncated)
        ));
    }
}

#[test]
fn range_playback_requires_one_clock_across_dec3_descriptors() {
    let mut raw = LEADING_EAC3.to_vec();
    let config = fixture_payload(&raw, b"dec3");
    let descriptor = [raw[config + 2], raw[config + 3], raw[config + 4]];
    raw[config + 1] |= 1;
    // The enclosing boxes are all after mdat, so packet offsets stay valid.
    for kind in [
        b"dec3", b"ec-3", b"stsd", b"stbl", b"minf", b"mdia", b"trak", b"moov",
    ] {
        let at = fixture_payload(&raw, kind) - 8;
        let size = u32::from_be_bytes(raw[at..at + 4].try_into().unwrap());
        raw[at..at + 4].copy_from_slice(&(size + 3).to_be_bytes());
    }
    raw.splice(config + 5..config + 5, descriptor);
    assert!(range_plan(&Source::new(&raw, b"soun"), *b"isom").is_ok());

    // Give the second descriptor a different fscod from the first.
    raw[config + 5] |= 0x40;
    assert!(range_plan(&Source::new(&raw, b"soun"), *b"isom").is_err());
}

#[test]
fn range_playback_refuses_invalid_dac3_fields() {
    let mut raw = LEADING_AC3.to_vec();
    let config = fixture_payload(&raw, b"dac3");
    // Change bsid from 8 to 9 while retaining the same rate code.
    raw[config] |= 2;
    let source = Source::new(&raw, b"soun");
    assert!(range_plan(&source, *b"isom").is_err());
}

#[test]
fn range_playback_requires_an_exact_dolby_edit_end() {
    for fixture in [LEADING_AC3, LEADING_EAC3] {
        for duration in [17_920u32, 17_921, 17_918, 17_922] {
            let mut raw = fixture.to_vec();
            for (kind, field, value) in [
                (b"mvhd", 12, 96_000u32),
                (b"mvhd", 16, duration),
                (b"tkhd", 20, duration),
                (b"elst", 8, duration),
            ] {
                let offset = fixture_payload(&raw, kind) + field;
                raw[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
            }
            let source = Source::new(&raw, b"soun");
            let plan = range_plan(&source, *b"isom");
            if duration == 17_920 {
                assert_eq!(plan.unwrap().selected_end_ticks(), 9216);
            } else {
                assert!(plan.is_err(), "range playback admitted duration {duration}");
            }
        }
    }
}

#[test]
fn original_video_all48_source_ranges_raw_cto_and_shift_are_separate() {
    let p = Source::new(ORIGINAL, b"vide");
    let plan = p.plan(1).unwrap();
    assert_eq!(plan.anchor_ticks(), 2672);
    let r = rows(&p, plan);
    assert_eq!(r.len(), 48);
    assert_eq!(r[0].0.source.dts_ticks, Some(0));
    assert_eq!(r[0].0.source.cts_ticks, Some(2672));
    assert_eq!(r[0].0.demux.dts, Some(-2672));
    assert_eq!(r[0].0.demux.pts, Some(0));
    assert_eq!(r[0].0.source.range, 48..3128);
    assert!(r.iter().all(|x| x.1.is_none()));
}
#[test]
fn video_boundary2673_chooses_next_real_presentation_point_not_a_constant() {
    let p = Source::new(VIDEO2673, b"vide");
    let plan = p.plan(1).unwrap();
    assert_eq!(plan.anchor_ticks(), 4000);
    let r = rows(&p, plan);
    assert_eq!(r.len(), 48);
    assert_eq!(r[0].0.demux.pts, Some(-1328));
    assert_eq!(r[0].0.demux.flags, 5);
    assert_eq!(r[0].0.source.cts_ticks, Some(2672));
}
#[test]
fn video_zero_edit_still_uses_first_real_composition_anchor() {
    let p = Source::new(VIDEO0, b"vide");
    let plan = p.plan(1).unwrap();
    assert_eq!(plan.source_entry().media_time, 0);
    assert_eq!(plan.anchor_ticks(), 2672);
    assert_eq!(rows(&p, plan)[0].0.demux.dts, Some(-2672));
}
#[test]
fn original_audio189_keeps_source_delta_and_both_skip_recommendations() {
    let p = Source::new(ORIGINAL, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 189);
    assert_eq!(r[0].0.source.dts_ticks, Some(0));
    assert_eq!(r[0].0.demux.dts, Some(-1008));
    assert_eq!(r[0].0.demux.flags, 1);
    assert_eq!(r[0].1, Some([0xf0, 3, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(r[188].0.source.duration_ticks, 528);
    assert_eq!(r[188].0.demux.duration, 528);
    assert_eq!(r[188].1, Some([0, 0, 0, 0, 0xf0, 1, 0, 0, 0, 0]));
}
#[test]
fn encoded_cursor_rejects_plan_with_wrong_movie_timebase() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut input = p.input();
    let stream = p.stream(&mut input);
    let mut job = EncodedEditJob::new(
        EncodedEditContext {
            table: &p.table,
            edits: &p.edits,
            stream: &stream,
            media_base: p.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 2000).unwrap(),
        },
        &input,
        EncodedEditLimits::default(),
        EncodedEditProfile,
    )
    .unwrap();
    let plans = [Some(complete(&mut job, &mut input, 1).unwrap())];
    let result = PacketCursor::new_encoded_regular(
        &p.pool,
        &[p.track()],
        &p.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .map(|_| ());
    assert_eq!(
        result,
        Err(Error::Invalid("encoded edit source association"))
    );
}
#[test]
fn zero_audio_edit_selects188_but_preserves_original189_source_table() {
    let p = Source::new(AUDIO0, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(p.table.sample_count(), 189);
    assert_eq!(r.len(), 188);
    assert_eq!(r[0].0.demux.dts, Some(0));
    assert_eq!(r[0].1, None);
    assert_eq!(r[187].0.source.ordinal, 188);
    assert_eq!(r[187].1, Some([0, 0, 0, 0, 0xc8, 1, 0, 0, 0, 0]));
}
#[test]
fn complete_preroll_packet_is_discard_but_source_key_and_duration_remain() {
    let p = Source::new(AUDIO1024, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r[0].0.demux.flags, 5);
    assert!(r[0].0.source.is_container_sync());
    assert_eq!(r[0].0.source.duration_ticks, 1024);
    assert_eq!(r[0].1, Some([0, 4, 0, 0, 0, 0, 0, 0, 0, 0]));
}
#[test]
fn two_audio_preroll_packets_keep_order_and_one_start_recommendation() {
    let p = Source::new(AUDIO2048, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 189);
    assert_eq!(r[0].0.demux.dts, Some(-2048));
    assert_eq!(r[0].0.demux.flags, 5);
    assert_eq!(r[1].0.demux.flags, 5);
    assert_eq!(r[2].0.demux.flags, 1);
    assert_eq!(r[0].1, Some([0, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert!(r[1].1.is_none());
}
#[test]
fn plan_work1_and_bulk_are_identical_and_keep_literal_elst() {
    let p = Source::new(ORIGINAL, b"vide");
    let a = p.plan(1).unwrap();
    let b = p.plan(4096).unwrap();
    assert_eq!(a.anchor_ticks(), b.anchor_ticks());
    assert_eq!(a.selected_end_ticks(), b.selected_end_ticks());
    assert_eq!(
        a.source_edit().bytes().as_ref(),
        p.edits.raw().bytes().as_ref()
    );
}
#[test]
fn zero_work_has_no_scan_allocation_position_or_ready_progress() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    let used = p.pool.used();
    let position = i.position();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut i, &mut w, &AtomicBool::new(false)).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(i.position(), position);
    assert_eq!(p.pool.used(), used);
}
#[test]
fn cancelled_plan_is_sticky_even_at_zero_work() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn ready_plan_cannot_be_taken_after_cancel_then_resurrected() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    while j
        .poll(&mut i, &mut WorkBudget::new(7), &AtomicBool::new(false))
        .unwrap()
        != CursorStatus::Ready
    {}
    assert_eq!(
        j.take(&mut i, &AtomicBool::new(true)).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.take(&mut i, &AtomicBool::new(false)).unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn transport_failure_precedes_zero_work() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    i.fail(i.source(), i.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn logical_generation_change_rejects_even_without_work() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn range_request_change_preserves_logical_source_plan() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    let generation = i.generation();
    i.request_range(32).unwrap();
    let plan = complete(&mut j, &mut i, 1).unwrap();
    assert_eq!(i.generation(), generation);
    assert_eq!(i.position(), 32);
    assert_eq!(plan.source_entry().media_time, 1008);
}
#[test]
fn foreign_pool_is_not_a_codec_epoch() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let other = Budget::new(1 << 20).unwrap();
    let f = InputCursor::new(
        &other,
        SourceId(917),
        SourceCapabilities {
            length: Some(p.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(p.job(&s, &f, 1000), Err(Error::ForeignPool)));
}
#[test]
fn retained_plan_keeps_full_hidden_source_backing_charged() {
    let p = Source::new(ORIGINAL, b"soun");
    let plan = p.plan(1).unwrap();
    let pool = p.pool.clone();
    drop(p);
    assert_eq!(plan.source_edit().header().kind, *b"elst");
    assert!(plan.source_table().bytes().retained_capacity() > plan.source_table().bytes().len());
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(plan);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn source_association_cannot_rebind_equal_bytes_from_different_backing() {
    let p = Source::new(ORIGINAL, b"soun");
    let plan = p.plan(1).unwrap();
    let other = Source::in_pool(ORIGINAL, b"soun", p.pool.clone());
    let plans = [Some(plan)];
    assert!(matches!(
        PacketCursor::new_encoded_regular(
            &p.pool,
            &[other.track()],
            &other.media,
            PacketLimits::default(),
            &plans,
            EncodedEditProfile
        ),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn source_sample_limit_counts_actual_scan_and_keeps_source_intact() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 1).unwrap();
    assert_eq!(
        complete(&mut j, &mut i, 1).unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
    assert_eq!(p.table.sample_count(), 189);
}
#[test]
fn old_strict_reordered_edit_cursor_stays_refused_after_new_admission() {
    let p = Source::new(ORIGINAL, b"vide");
    let _plan = p.plan(1).unwrap();
    let mut old = EditCursor::new(
        &p.table,
        &p.edits,
        p.header.time_base().unwrap(),
        TimeBase::new(1, 1000).unwrap(),
        EditLimits::default(),
    )
    .unwrap();
    for _ in 0..10000 {
        match old.poll(&mut WorkBudget::new(1)) {
            Err(Error::Unsupported(_)) => return,
            Ok(CursorStatus::Yield) => {}
            value => panic!("old strict policy changed {value:?}"),
        }
    }
    panic!("strict refusal did not progress")
}
#[test]
fn new_index_uses_same_real_source_positions_and_decode_ticks() {
    let p = Source::new(ORIGINAL, b"vide");
    let plans = [Some(p.plan(1).unwrap())];
    let mut c = IndexCursor::new_encoded_regular(
        &p.pool,
        p.track(),
        &p.media,
        IndexLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    let mut count = 0;
    for _ in 0..100000 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::Ready => {
                let e = c.take_entry().unwrap();
                if count == 0 {
                    assert_eq!(e.position, 48);
                    assert_eq!(e.timestamp, -2672);
                    assert_eq!(e.size, 3080);
                    assert_eq!(e.flags, 1);
                }
                count += 1;
            }
            CursorStatus::End => {
                assert_eq!(count, 48);
                return;
            }
        }
    }
    panic!("index progress")
}

#[test]
fn charged_cursor_pressure_preserves_plan_for_bounded_retry() {
    let p = Source::new(ORIGINAL, b"soun");
    let plans = [Some(p.plan(1).unwrap())];
    let hold = p.pool.reserve(p.pool.limit() - p.pool.used()).unwrap();
    assert!(matches!(
        PacketCursor::new_encoded_regular(
            &p.pool,
            &[p.track()],
            &p.media,
            PacketLimits::default(),
            &plans,
            EncodedEditProfile
        ),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    let mut cursor = PacketCursor::new_encoded_regular(
        &p.pool,
        &[p.track()],
        &p.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    for _ in 0..100000 {
        if cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == CursorStatus::Ready
        {
            assert_eq!(cursor.take_occurrence().unwrap().demux.dts, Some(-1008));
            return;
        }
    }
    panic!("pressure retry progress")
}

#[test]
fn video_zero_edit_tail_discard_follows_declared_window_not_shifted_anchor() {
    let p = Source::new(VIDEO0, b"vide");
    let r = rows(&p, p.plan(1).unwrap());
    let flags: Vec<_> = r
        .iter()
        .filter(|(row, _)| row.demux.flags & 4 != 0)
        .map(|(row, _)| row.source.ordinal)
        .collect();
    assert_eq!(flags, vec![45, 48]);
    assert_eq!(r[44].0.source.cts_ticks, Some(64000));
    assert_eq!(r[44].0.demux.pts, Some(61328));
    assert_eq!(p.edits.entry(0).unwrap().media_time, 0);
}
#[test]
fn original_video_tail_keeps_declared_window_separate_from_zero_edit_control() {
    let p = Source::new(ORIGINAL, b"vide");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 48);
    assert!(r.iter().all(|(row, _)| row.demux.flags & 4 == 0));
    assert_eq!(p.edits.entry(0).unwrap().media_time, 2672);
}

#[test]
fn encoded_occurrence_provenance_video_retains_real_edit_ordinal_and_local_sequence() {
    let p = Source::new(ORIGINAL, b"vide");
    let r = rows(&p, p.plan(1).unwrap());
    for (row, _) in r {
        let e = row
            .edit
            .expect("known applied single edit must have identity");
        assert_eq!(e.edit_ordinal, 0);
        assert_eq!(e.sequence, row.source.ordinal);
        assert_eq!(
            e.duration_basis,
            triergon_isobmff::compat::mov::edits::DurationBasis::OriginalDecodeTableOrdinal
        );
        assert_eq!(row.source.composition_offset.unwrap().version(), 0);
    }
}
#[test]
fn encoded_occurrence_provenance_audio_prefix_omits_no_source_fact() {
    let p = Source::new(AUDIO0, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 188);
    let e = r[187].0.edit.expect("selected edit identity");
    assert_eq!(e.edit_ordinal, 0);
    assert_eq!(e.sequence, 188);
    assert_eq!(r[187].0.source.ordinal, 188);
    assert_eq!(p.table.sample_count(), 189);
    assert_eq!(p.edits.entry(0).unwrap().media_time, 0);
}

#[test]
fn without_sdtp_keeps_existing_aac_roll_observation() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut source = p.table.cursor();
    match source.poll(&mut WorkBudget::new(1)).unwrap() {
        triergon_isobmff::SampleStep::Sample(s) => assert_eq!(s.dependency, None),
        _ => panic!("original source first sample absent"),
    }
    let expected = rows(&p, p.plan(1).unwrap());
    let mut input = p.input();
    let configs = [p.stream(&mut input)];
    let mut job = EncodedEditJob::new_with_dependency_table(
        EncodedEditContext {
            table: &p.table,
            edits: &p.edits,
            stream: &configs[0],
            media_base: p.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        },
        &configs,
        &input,
        EncodedEditLimits::default(),
        EncodedMetadataProfile,
    )
    .unwrap();
    let actual = rows(&p, complete(&mut job, &mut input, 1).unwrap());
    assert_eq!(actual.len(), 189);
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual.0.source, expected.0.source);
        assert_eq!(actual.0.demux, expected.0.demux);
        assert_eq!(actual.1, expected.1);
    }
}
