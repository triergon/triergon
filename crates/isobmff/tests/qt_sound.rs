mod common;
use common::{child, children, roots, table};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};
use triergon_isobmff::{
    QtPcmEncodingV0, QtPcmSampleReaderV0, QtPcmSourcePacketV0, QtSoundDescriptionV0, Sample,
    SampleStep, SampleTable, TrackHeader,
};
const MONO: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-u8-mono-front.mov");
const STEREO: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-s16be-stereo-front.mov");
const CONFIG: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-description-1-2-1-unused3.mov");
const ALL: &[&[u8]] = &[
    MONO,
    STEREO,
    include_bytes!("fixtures/pcm-v0/pcm-u8-stereo-end.mov"),
    include_bytes!("fixtures/pcm-v0/pcm-s16be-mono-end.mov"),
    CONFIG,
    include_bytes!("fixtures/pcm-v0/pcm-multi-8k-u8-and-48k-twos.mov"),
    include_bytes!("fixtures/pcm-v0/pcm-long-chunk-2049.mov"),
];
struct Parsed {
    pool: Budget,
    tracks: Vec<(TrackHeader, SampleTable)>,
    media: Range<u64>,
    length: u64,
}
impl Parsed {
    fn new(data: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let root = roots(&pool, data).unwrap();
        let movie = root.iter().find(|v| v.header().kind == *b"moov").unwrap();
        let mut tracks = Vec::new();
        for track in children(movie, 0)
            .unwrap()
            .iter()
            .filter(|v| v.header().kind == *b"trak")
        {
            let mdia = child(track, b"mdia").unwrap();
            let header = TrackHeader::parse(
                &child(track, b"tkhd").unwrap(),
                &child(&mdia, b"mdhd").unwrap(),
                &child(&mdia, b"hdlr").unwrap(),
            )
            .unwrap();
            let raw = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
            tracks.push((header, table(raw, &pool, 1).unwrap()));
        }
        Self {
            media: root
                .iter()
                .find(|v| v.header().kind == *b"mdat")
                .unwrap()
                .header()
                .payload_range()
                .unwrap(),
            pool,
            tracks,
            length: data.len() as u64,
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(73),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            4096,
        )
        .unwrap()
    }
    fn first(&self) -> Sample {
        let mut cursor = self.tracks[0].1.cursor();
        loop {
            if let SampleStep::Sample(s) = cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                return s;
            }
        }
    }
    fn sound(&self, index: usize) -> QtSoundDescriptionV0 {
        let (h, t) = &self.tracks[0];
        QtSoundDescriptionV0::parse(h, &t.descriptions()[index]).unwrap()
    }
    fn job(&self, input: &mut InputCursor) -> QtPcmSampleReaderV0 {
        QtPcmSampleReaderV0::new(
            input,
            self.sound(0),
            self.first(),
            self.media.clone(),
            1 << 20,
        )
        .unwrap()
    }
}
fn feed(input: &mut InputCursor, r: InputRequest, raw: &[u8], maximum: usize) {
    let start = r.offset as usize;
    let end = (start + r.maximum.min(maximum)).min(raw.len());
    assert!(end > start);
    let bytes = Bytes::copy_from_slice(input.budget(), &raw[start..end]).unwrap();
    input
        .provide_owned(InputWindow::new(r.source, r.token, r.offset, bytes).unwrap())
        .unwrap();
}
fn read(
    job: &mut QtPcmSampleReaderV0,
    input: &mut InputCursor,
    raw: &[u8],
    quantum: u64,
    maximum: usize,
) -> Result<QtPcmSourcePacketV0, Error> {
    for _ in 0..10000 {
        let mut work = WorkBudget::new(quantum);
        let step = job.poll(input, &mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= quantum);
        match step {
            Step::Ready(v) => return Ok(v),
            Step::NeedInput(r) => feed(input, r, raw, maximum),
            Step::Yield => (),
            Step::End(_) => panic!("source sample became fabricated EOF"),
        }
    }
    panic!("PCM source read did not progress")
}
fn mutate(field: usize, bytes: &[u8]) -> Vec<u8> {
    let s = Parsed::new(STEREO);
    let d = s.tracks[0].1.descriptions()[0].raw();
    let at = d.header().start as usize + d.header().header_size as usize + field;
    let mut raw = STEREO.to_vec();
    raw[at..at + bytes.len()].copy_from_slice(bytes);
    raw
}
#[test]
fn original_sound_fields_and_exact_fixed_rate_are_retained() {
    for (raw, channels, bits, rate, encoding) in [
        (MONO, 1, 8, 8000_u32, QtPcmEncodingV0::Unsigned8),
        (STEREO, 2, 16, 48000, QtPcmEncodingV0::Signed16BigEndian),
    ] {
        let s = Parsed::new(raw);
        let d = s.sound(0);
        assert_eq!(d.channels(), channels);
        assert_eq!(d.sample_bits(), bits);
        assert_eq!(d.sample_rate_16_16(), rate << 16);
        assert_eq!(d.sample_rate_fraction(), (rate << 16, 65536));
        assert_eq!(d.compression_id(), 0);
        assert_eq!(d.packet_size(), 0);
        assert_eq!(d.vendor(), [0; 4]);
        assert_eq!(d.revision(), 0);
        assert_eq!(d.pcm_layout().unwrap().encoding(), encoding);
        let view = d.source().raw();
        assert_eq!(
            view.bytes().as_ref(),
            &raw[view.header().start as usize
                ..(view.header().start + view.header().size.unwrap()) as usize]
        );
    }
}
#[test]
fn every_sample_of_all_seven_movies_is_read_with_its_own_source_description() {
    let mut count = 0;
    for raw in ALL {
        let s = Parsed::new(raw);
        for (h, t) in &s.tracks {
            let mut cursor = t.cursor();
            loop {
                match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
                    SampleStep::Yield => (),
                    SampleStep::End => break,
                    SampleStep::Sample(sample) => {
                        let d = QtSoundDescriptionV0::parse(
                            h,
                            &t.descriptions()[sample.description_index as usize - 1],
                        )
                        .unwrap();
                        let mut input = s.input();
                        let mut job = QtPcmSampleReaderV0::new(
                            &mut input,
                            d,
                            sample.clone(),
                            s.media.clone(),
                            1 << 20,
                        )
                        .unwrap();
                        let packet = read(&mut job, &mut input, raw, 1, 1).unwrap();
                        assert_eq!(packet.packet().sample, sample);
                        assert_eq!(packet.complete_frames(), 1);
                        assert_eq!(
                            packet.packet().payload.as_ref(),
                            &raw[sample.range.start as usize..sample.range.end as usize]
                        );
                        count += 1;
                    }
                }
            }
        }
    }
    assert_eq!(count, 2161); // 4*16 +16 configuration +2*16 multitrack +2049.
}
#[test]
fn source_unused_description_and_return_to_prior_entry_remain_distinct() {
    let s = Parsed::new(CONFIG);
    let (h, t) = &s.tracks[0];
    assert_eq!(t.descriptions().len(), 3);
    assert_eq!(s.sound(2).sample_rate_16_16(), 44100 << 16);
    let mut cursor = t.cursor();
    let mut changed = Vec::new();
    let mut prior = 0;
    loop {
        match cursor.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Yield => (),
            SampleStep::End => break,
            SampleStep::Sample(sample) => {
                if sample.description_index != prior {
                    changed.push((sample.ordinal, sample.description_index));
                    prior = sample.description_index;
                }
                let d = QtSoundDescriptionV0::parse(
                    h,
                    &t.descriptions()[sample.description_index as usize - 1],
                )
                .unwrap();
                assert_eq!(d.source().index, sample.description_index);
            }
        }
    }
    assert_eq!(changed, [(1, 1), (5, 2), (9, 1)]);
}
#[test]
fn unknown_revision_vendor_and_fractional_rate_are_not_normalized() {
    let mut raw = mutate(10, &7_u16.to_be_bytes());
    let s = Parsed::new(&raw);
    let d = s.tracks[0].1.descriptions()[0].raw();
    let at = d.header().start as usize + d.header().header_size as usize;
    raw[at + 12..at + 16].copy_from_slice(b"TEST");
    raw[at + 24..at + 28].copy_from_slice(&((48000_u32 << 16) | 32768).to_be_bytes());
    let s = Parsed::new(&raw);
    let d = s.sound(0);
    assert_eq!(d.revision(), 7);
    assert_eq!(d.vendor(), *b"TEST");
    assert_eq!(d.sample_rate_fraction(), ((48000_u32 << 16) | 32768, 65536));
    assert_eq!(s.tracks[0].0.timescale, 48000);
}
#[test]
fn version1_and2_are_not_parsed_using_v0_offsets() {
    for version in [1_u16, 2, 65535] {
        let raw = mutate(8, &version.to_be_bytes());
        let s = Parsed::new(&raw);
        assert!(matches!(
            QtSoundDescriptionV0::parse(&s.tracks[0].0, &s.tracks[0].1.descriptions()[0]),
            Err(Error::Unsupported(_))
        ));
    }
}
#[test]
fn video_raw_name_cannot_choose_an_audio_layout() {
    let s = Parsed::new(STEREO);
    let mut header = TrackHeader::parse(
        &s.tracks[0].0.raw_tkhd,
        &s.tracks[0].0.raw_mdhd,
        &s.tracks[0].0.raw_hdlr,
    )
    .unwrap();
    header.handler = *b"vide";
    assert!(matches!(
        QtSoundDescriptionV0::parse(&header, &s.tracks[0].1.descriptions()[0]),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn public_metadata_mutation_cannot_forge_raw_sound_fields() {
    let s = Parsed::new(STEREO);
    let (h, t) = &s.tracks[0];
    for field in [0, 1, 2] {
        let mut entry = t.descriptions()[0].clone();
        match field {
            0 => entry.format = *b"raw ",
            1 => entry.data_reference_index = 2,
            _ => entry.index = 0,
        }
        assert!(matches!(
            QtSoundDescriptionV0::parse(h, &entry),
            Err(Error::Invalid(_))
        ));
    }
}
#[test]
fn invalid_or_unestablished_pcm_fields_remain_raw_and_refuse_layout() {
    for (field, bits) in [(16, 0_u16), (16, 3), (18, 24), (20, 65534), (22, 1)] {
        let raw = mutate(field, &bits.to_be_bytes());
        let s = Parsed::new(&raw);
        let d = s.sound(0);
        assert!(d.pcm_layout().is_err());
        assert_eq!(
            &d.source().raw().payload()[field..field + 2],
            bits.to_be_bytes()
        );
    }
    let raw = mutate(24, &0_u32.to_be_bytes());
    let s = Parsed::new(&raw);
    assert_eq!(
        s.sound(0).pcm_layout().unwrap_err(),
        Error::Invalid("QT PCM zero source sample rate")
    );
}
#[test]
fn source_payload_is_identical_for_short_reads_and_work_schedules() {
    for quantum in [1, 3, 4096] {
        for maximum in [1, 4096] {
            let s = Parsed::new(STEREO);
            let mut input = s.input();
            let mut job = s.job(&mut input);
            let p = read(&mut job, &mut input, STEREO, quantum, maximum).unwrap();
            assert_eq!(p.packet().payload.as_ref(), [0x80, 0, 0, 0]);
            assert_eq!(p.packet().sample.dts_ticks, Some(0));
            assert_eq!(p.packet().sample.duration_ticks, 1);
        }
    }
}
#[test]
fn zero_work_never_requests_source_or_changes_charge() {
    let s = Parsed::new(STEREO);
    let mut input = s.input();
    let mut job = s.job(&mut input);
    let used = s.pool.used();
    let token = input.token();
    let at = input.position();
    for _ in 0..8 {
        assert!(matches!(
            job.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            Step::Yield
        ));
        assert_eq!(s.pool.used(), used);
        assert_eq!(input.position(), at);
        assert_eq!(input.token(), token);
    }
    assert_eq!(
        read(&mut job, &mut input, STEREO, 1, 1)
            .unwrap()
            .complete_frames(),
        1
    );
}
#[test]
fn cancellation_releases_only_private_backings_and_is_sticky() {
    let s = Parsed::new(STEREO);
    let mut input = s.input();
    let before = s.pool.used();
    let mut job = s.job(&mut input);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        job.poll(&mut input, &mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(s.pool.used(), before);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut input, &mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn transport_failure_is_not_starvation_end_or_a_demux_error() {
    let s = Parsed::new(STEREO);
    let mut input = s.input();
    let mut job = s.job(&mut input);
    input
        .fail(input.source(), input.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        job.poll(
            &mut input,
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn foreign_pool_or_stale_request_cannot_read_pcm() {
    let s = Parsed::new(STEREO);
    let other = Budget::new(1 << 20).unwrap();
    let mut input = InputCursor::new(
        &other,
        SourceId(73),
        SourceCapabilities {
            length: Some(s.length),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    assert_eq!(
        QtPcmSampleReaderV0::new(&mut input, s.sound(0), s.first(), s.media.clone(), 4096)
            .unwrap_err(),
        Error::ForeignPool
    );
    let mut input = s.input();
    let mut job = s.job(&mut input);
    let at = input.position();
    input.request_range(at).unwrap();
    assert!(matches!(
        job.poll(
            &mut input,
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false)
        ),
        Err(Error::StaleRequest { .. })
    ));
}
#[test]
fn incorrect_description_or_partial_frame_refuses_before_source_mutation() {
    let s = Parsed::new(STEREO);
    let mut input = s.input();
    let token = input.token();
    let mut sample = s.first();
    sample.description_index = 2;
    assert!(
        QtPcmSampleReaderV0::new(&mut input, s.sound(0), sample, s.media.clone(), 4096).is_err()
    );
    assert_eq!(input.token(), token);
    let mut sample = s.first();
    sample.range.end -= 1;
    assert_eq!(
        QtPcmSampleReaderV0::new(&mut input, s.sound(0), sample, s.media.clone(), 4096)
            .unwrap_err(),
        Error::Invalid("QT PCM incomplete source frame")
    );
    assert_eq!(input.token(), token);
}
#[test]
fn byte_limit_and_budget_pressure_leave_original_input_token_intact() {
    let s = Parsed::new(STEREO);
    let mut input = s.input();
    let token = input.token();
    assert_eq!(
        QtPcmSampleReaderV0::new(&mut input, s.sound(0), s.first(), s.media.clone(), 3)
            .unwrap_err(),
        Error::Limit {
            requested: 4,
            maximum: 3
        }
    );
    assert_eq!(input.token(), token);
    let blocker = s.pool.reserve(s.pool.available()).unwrap();
    assert!(matches!(
        QtPcmSampleReaderV0::new(&mut input, s.sound(0), s.first(), s.media.clone(), 4096),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.token(), token);
    drop(blocker);
    assert!(
        QtPcmSampleReaderV0::new(&mut input, s.sound(0), s.first(), s.media.clone(), 4096).is_ok()
    );
}
#[test]
fn retained_source_packet_and_description_outlive_the_decoder_and_input() {
    let s = Parsed::new(STEREO);
    let pool = s.pool.clone();
    let mut input = s.input();
    let mut job = s.job(&mut input);
    let output = read(&mut job, &mut input, STEREO, 1, 1).unwrap();
    drop(job);
    drop(input);
    drop(s);
    assert_eq!(output.packet().payload.as_ref(), [0x80, 0, 0, 0]);
    assert_eq!(output.description().sample_bits(), 16);
    assert_eq!(output.packet().sample.duration_ticks, 1);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(output);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn sequential_unknown_length_source_does_not_turn_starvation_into_eof() {
    let s = Parsed::new(STEREO);
    let sample = s.first();
    let mut input = InputCursor::new(
        &s.pool,
        SourceId(73),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        7,
    )
    .unwrap();
    while input.position() < sample.range.start {
        let mut byte = [0];
        match input
            .read_into(&mut byte, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(_) => (),
            Step::NeedInput(r) => feed(&mut input, r, STEREO, 1),
            Step::Yield => (),
            Step::End(_) => panic!("unknown source fabricated EOF before audio"),
        }
    }
    let mut job = QtPcmSampleReaderV0::new(
        &mut input,
        s.sound(0),
        sample.clone(),
        s.media.clone(),
        4096,
    )
    .unwrap();
    let output = read(&mut job, &mut input, STEREO, 1, 1).unwrap();
    assert_eq!(output.packet().sample, sample);
    assert_eq!(output.packet().payload.as_ref(), [0x80, 0, 0, 0]);
    assert_eq!(input.capabilities().length, None);
}
#[test]
fn freeze_pressure_preserves_completed_bytes_and_allocation_retry() {
    let s = Parsed::new(STEREO);
    let sample = s.first();
    let mut input = s.input();
    let mut job = s.job(&mut input);
    while input.position() < sample.range.end {
        match job
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::NeedInput(r) => feed(&mut input, r, STEREO, 1),
            Step::Yield => (),
            Step::Ready(_) => panic!("freeze published without its work unit"),
            Step::End(_) => panic!("source frame became EOF"),
        }
    }
    let blocker = s.pool.reserve(s.pool.available()).unwrap();
    let at = input.position();
    let token = input.token();
    for _ in 0..3 {
        assert!(matches!(
            job.poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false)),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(input.position(), at);
        assert_eq!(input.token(), token);
    }
    drop(blocker);
    let output = read(&mut job, &mut input, STEREO, 1, 1).unwrap();
    assert_eq!(output.packet().sample, sample);
    assert_eq!(output.packet().payload.as_ref(), [0x80, 0, 0, 0]);
    assert_eq!(output.complete_frames(), 1);
}
