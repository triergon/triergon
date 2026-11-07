mod common;
use common::{child, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};
use triergon_isobmff::{
    SampleTable, TrackHeader,
    compat::mov::{
        packets::CursorStatus,
        pcm::{PcmCursor, PcmLimits, PcmPacket, PcmProfile, PcmReader, PcmStream},
    },
};

const MONO: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-u8-mono-front.mov");
const STEREO: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-s16be-stereo-front.mov");
const SWITCH: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-description-1-2-1-unused3.mov");
const LONG: &[u8] = include_bytes!("fixtures/pcm-v0/pcm-long-chunk-2049.mov");

struct Parsed {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    media: Range<u64>,
    length: u64,
}
impl Parsed {
    fn new(raw: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let top = roots(&pool, raw).unwrap();
        let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
        let track = child(movie, b"trak").unwrap();
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
        Self {
            pool,
            header,
            table,
            media: top
                .iter()
                .find(|x| x.header().kind == *b"mdat")
                .unwrap()
                .header()
                .payload_range()
                .unwrap(),
            length: raw.len() as u64,
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
    fn cursor<'a>(&'a self, input: &InputCursor) -> PcmCursor<'a> {
        PcmCursor::new(
            &self.header,
            &self.table,
            input,
            self.media.clone(),
            PcmLimits::default(),
            PcmProfile,
        )
        .unwrap()
    }
}
fn next(
    cursor: &mut PcmCursor<'_>,
    input: &mut InputCursor,
    quantum: u64,
) -> Result<Option<PcmPacket>, Error> {
    for _ in 0..100000 {
        let mut work = WorkBudget::new(quantum);
        let step = cursor.poll(input, &mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= quantum);
        match step {
            CursorStatus::Yield => (),
            CursorStatus::Ready => {
                return Ok(Some(cursor.take_packet(input, &AtomicBool::new(false))?));
            }
            CursorStatus::End => return Ok(None),
        }
    }
    panic!("bounded PCM packet cursor did not progress")
}
fn feed(input: &mut InputCursor, request: InputRequest, raw: &[u8], maximum: usize) {
    let start = request.offset as usize;
    let end = (start + request.maximum.min(maximum)).min(raw.len());
    let bytes = Bytes::copy_from_slice(input.budget(), &raw[start..end]).unwrap();
    input
        .provide_owned(
            InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
        )
        .unwrap();
}
fn read(
    reader: &mut PcmReader,
    input: &mut InputCursor,
    raw: &[u8],
    maximum: usize,
) -> triergon_isobmff::compat::mov::pcm::PcmPayload {
    for _ in 0..100000 {
        match reader
            .poll(input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(packet) => return packet,
            Step::NeedInput(r) => feed(input, r, raw, maximum),
            Step::Yield => (),
            Step::End(_) => panic!("native PCM physical read manufactured EOF"),
        }
    }
    panic!("bounded PCM reader did not progress")
}
#[test]
fn literal_demux_chunk_boundaries_are_not_global_1024_splits() {
    let p = Parsed::new(LONG);
    let mut input = p.input();
    let mut cursor = p.cursor(&input);
    let mut rows = Vec::new();
    while let Some(packet) = next(&mut cursor, &mut input, 1).unwrap() {
        rows.push((
            packet.dts(),
            packet.duration(),
            packet.demux_range().clone(),
        ));
    }
    assert_eq!(
        rows,
        [
            (0, 1023, 573..4665),
            (1023, 1, 4665..4669),
            (1024, 1024, 4669..8765),
            (2048, 1, 8765..8769)
        ]
    );
    assert_eq!(p.table.sample_count(), 2049);
}
#[test]
fn switched_source_description_never_replaces_initial_demux_layout() {
    let p = Parsed::new(SWITCH);
    let mut input = p.input();
    let mut cursor = p.cursor(&input);
    let first = next(&mut cursor, &mut input, 1).unwrap().unwrap();
    let second = next(&mut cursor, &mut input, 1).unwrap().unwrap();
    let third = next(&mut cursor, &mut input, 1).unwrap().unwrap();
    assert_eq!(first.source_description().source().index, 1);
    assert_eq!(second.source_description().source().index, 2);
    assert_eq!(second.source_description().channels(), 1);
    assert_eq!(second.initial_description().channels(), 2);
    assert_eq!(second.source_range(), &(725..733));
    assert_eq!(second.demux_range(), &(725..741));
    assert_eq!(third.demux_range().start, 733);
    assert_eq!(p.table.descriptions().len(), 3);
    assert_eq!(second.source_samples(), 5..9);
    assert_eq!(
        first.source_table().bytes().as_ref(),
        p.table.raw().bytes().as_ref()
    );
}
#[test]
fn physical_reader_keeps_overlap_and_original_source_span() {
    let p = Parsed::new(SWITCH);
    let mut input = p.input();
    let mut cursor = p.cursor(&input);
    next(&mut cursor, &mut input, 1).unwrap();
    let packet = next(&mut cursor, &mut input, 1).unwrap().unwrap();
    let mut reader = PcmReader::new(&mut input, packet, 8192).unwrap();
    let result = read(&mut reader, &mut input, SWITCH, 1);
    assert_eq!(result.payload().as_ref(), &SWITCH[725..741]);
    assert_eq!(result.plan().source_range(), &(725..733));
    assert_eq!(result.plan().source_description().source().index, 2);
}
#[test]
fn whole_and_work_one_plans_are_identical() {
    let p = Parsed::new(LONG);
    let collect = |quantum| {
        let mut input = p.input();
        let mut c = p.cursor(&input);
        let mut rows = Vec::new();
        while let Some(x) = next(&mut c, &mut input, quantum).unwrap() {
            rows.push((
                x.demux_ordinal(),
                x.source_samples(),
                x.demux_range().clone(),
                x.dts(),
                x.duration(),
            ));
        }
        rows
    };
    assert_eq!(collect(1), collect(4096));
}
#[test]
fn initial_demux_catalogue_retains_exact_source_entry() {
    for (raw, codec) in [(MONO, "pcm_u8"), (STEREO, "pcm_s16be")] {
        let p = Parsed::new(raw);
        let s = PcmStream::new(&p.header, &p.table, Some(*b"qt  "), PcmProfile).unwrap();
        assert_eq!(s.demux().codec, codec);
        assert_eq!(s.demux().media_type, 1);
        assert!(s.demux().extradata.is_empty());
        assert_eq!(s.demux().vendor_id, Some([0; 4]));
        assert_eq!(
            s.initial().source().raw().bytes().as_ref(),
            p.table.descriptions()[0].raw().bytes().as_ref()
        );
    }
}
#[test]
fn zero_work_keeps_plan_position_budget_and_work_unchanged() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let used = p.pool.used();
    let mut c = p.cursor(&input);
    let mut w = WorkBudget::new(0);
    assert_eq!(
        c.poll(&mut input, &mut w, &AtomicBool::new(false)).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(input.position(), 0);
    assert_eq!(w.spent(), 0);
    assert_eq!(p.pool.used(), used);
}
#[test]
fn cancellation_precedes_work_and_stays_poisoned() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    assert_eq!(
        c.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        c.poll(
            &mut input,
            &mut WorkBudget::new(100),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn wrong_pool_is_rejected_before_plan_or_read() {
    let p = Parsed::new(MONO);
    let foreign = Budget::new(1 << 20).unwrap();
    let input = InputCursor::new(
        &foreign,
        SourceId(73),
        SourceCapabilities {
            length: Some(p.length),
            random_access: true,
        },
        1,
    )
    .unwrap();
    assert!(matches!(
        PcmCursor::new(
            &p.header,
            &p.table,
            &input,
            p.media.clone(),
            PcmLimits::default(),
            PcmProfile
        ),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn generation_change_refuses_pending_demux_plan() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    while c
        .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != CursorStatus::Ready
    {}
    input.reset(0).unwrap();
    assert!(matches!(
        c.take_packet(&mut input, &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn packet_plan_survives_legitimate_read_requests_but_not_reset() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    let packet = next(&mut c, &mut input, 1).unwrap().unwrap();
    let mut r = PcmReader::new(&mut input, packet, 8192).unwrap();
    read(&mut r, &mut input, MONO, 1);
    assert_eq!(next(&mut c, &mut input, 1).unwrap().unwrap().dts(), 4);
    input.reset(0).unwrap();
    assert!(matches!(
        next(&mut c, &mut input, 1),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn physical_reader_rejects_foreign_range_request_and_remains_sticky() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let packet = next(&mut p.cursor(&input), &mut input, 1).unwrap().unwrap();
    let mut r = PcmReader::new(&mut input, packet, 8192).unwrap();
    input.request_range(0).unwrap();
    assert!(matches!(
        r.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::StaleRequest { .. })
    ));
    assert!(matches!(
        r.poll(
            &mut input,
            &mut WorkBudget::new(99),
            &AtomicBool::new(false)
        ),
        Err(Error::StaleRequest { .. })
    ));
}
#[test]
fn packet_limit_is_not_a_demux_codec_error_or_input_mutation() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let packet = next(&mut p.cursor(&input), &mut input, 1).unwrap().unwrap();
    let token = input.token();
    let used = p.pool.used();
    assert!(matches!(
        PcmReader::new(&mut input, packet, 3),
        Err(Error::Limit {
            requested: 4,
            maximum: 3
        })
    ));
    assert_eq!(input.token(), token);
    assert_eq!(p.pool.used(), used);
}
#[test]
fn source_sample_limit_is_checked_before_metadata_walk() {
    let p = Parsed::new(LONG);
    let input = p.input();
    let limits = PcmLimits {
        max_samples: 2048,
        ..PcmLimits::default()
    };
    assert!(matches!(
        PcmCursor::new(
            &p.header,
            &p.table,
            &input,
            p.media.clone(),
            limits,
            PcmProfile
        ),
        Err(Error::Limit {
            requested: 2049,
            maximum: 2048
        })
    ));
}
#[test]
fn transport_failure_precedes_zero_work_in_both_jobs() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    input
        .fail(input.source(), input.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        c.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn retained_physical_payload_and_all_raw_backings_outlive_input_and_index() {
    let p = Parsed::new(SWITCH);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    next(&mut c, &mut input, 1).unwrap();
    let packet = next(&mut c, &mut input, 1).unwrap().unwrap();
    let mut reader = PcmReader::new(&mut input, packet, 8192).unwrap();
    let result = read(&mut reader, &mut input, SWITCH, 1);
    let pool = p.pool.clone();
    drop(reader);
    drop(c);
    drop(input);
    drop(p);
    assert_eq!(result.payload().as_ref(), &SWITCH[725..741]);
    assert_eq!(result.plan().initial_description().channels(), 2);
    assert_eq!(result.plan().source_description().channels(), 1);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(result);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn public_header_mutation_does_not_change_demux_catalogue() {
    let mut p = Parsed::new(MONO);
    p.header.timescale = 1;
    assert!(matches!(
        PcmStream::new(&p.header, &p.table, Some(*b"qt  "), PcmProfile),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn unsupported_brand_does_not_rewrite_source_pcm() {
    let p = Parsed::new(MONO);
    assert!(matches!(
        PcmStream::new(&p.header, &p.table, Some(*b"isom"), PcmProfile),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(p.table.descriptions()[0].format, *b"raw ");
}
#[test]
fn freeze_backpressure_preserves_bytes_token_and_paid_completion_work() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    let packet = next(&mut c, &mut input, 1).unwrap().unwrap();
    let mut r = PcmReader::new(&mut input, packet, 8192).unwrap();
    let Step::NeedInput(request) = r
        .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("missing real physical request")
    };
    feed(&mut input, request, MONO, 4);
    assert!(matches!(
        r.poll(&mut input, &mut WorkBudget::new(4), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    let token = input.token();
    let position = input.position();
    let blocker = p.pool.reserve(p.pool.available()).unwrap();
    for attempt in 0..3 {
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            r.poll(&mut input, &mut work, &AtomicBool::new(false)),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(work.spent(), u64::from(attempt == 0));
        assert_eq!(input.token(), token);
        assert_eq!(input.position(), position);
    }
    drop(blocker);
    assert!(matches!(
        r.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    let mut work = WorkBudget::new(1);
    let Step::Ready(output) = r
        .poll(&mut input, &mut work, &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("freeze retry lost completed bytes")
    };
    assert_eq!(work.spent(), 0);
    assert_eq!(output.payload().as_ref(), &MONO[561..565]);
}
#[test]
fn constructor_pressure_is_retriable_without_starting_an_io_request() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    let packet = next(&mut c, &mut input, 1).unwrap().unwrap();
    let token = input.token();
    let used = p.pool.used();
    let block = p.pool.reserve(p.pool.available()).unwrap();
    assert!(matches!(
        PcmReader::new(&mut input, packet.clone(), 8192),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(input.token(), token);
    drop(block);
    assert_eq!(p.pool.used(), used);
    let mut r = PcmReader::new(&mut input, packet, 8192).unwrap();
    assert_eq!(
        read(&mut r, &mut input, MONO, 1).payload().as_ref(),
        &MONO[561..565]
    );
}
#[test]
fn reader_zero_work_and_cancellation_do_not_publish_or_read() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    let packet = next(&mut c, &mut input, 1).unwrap().unwrap();
    let mut r = PcmReader::new(&mut input, packet, 8192).unwrap();
    let position = input.position();
    assert!(matches!(
        r.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(input.position(), position);
    assert_eq!(
        r.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        r.poll(
            &mut input,
            &mut WorkBudget::new(99),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn generation_change_after_plan_take_refuses_physical_read() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let mut c = p.cursor(&input);
    let packet = next(&mut c, &mut input, 1).unwrap().unwrap();
    input.reset(0).unwrap();
    assert!(matches!(
        PcmReader::new(&mut input, packet, 8192),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn packet_policy_limit_preserves_source_and_poisoned_cursor() {
    let p = Parsed::new(MONO);
    let mut input = p.input();
    let limits = PcmLimits {
        max_packet_bytes: 3,
        ..PcmLimits::default()
    };
    let mut c = PcmCursor::new(
        &p.header,
        &p.table,
        &input,
        p.media.clone(),
        limits,
        PcmProfile,
    )
    .unwrap();
    assert_eq!(
        next(&mut c, &mut input, 1).unwrap_err(),
        Error::Limit {
            requested: 4,
            maximum: 3
        }
    );
    assert_eq!(
        next(&mut c, &mut input, 4096).unwrap_err(),
        Error::Limit {
            requested: 4,
            maximum: 3
        }
    );
    assert_eq!(input.position(), 0);
    assert_eq!(p.table.sample_count(), 16);
}
#[test]
fn actual_unit_source_duration_is_required_without_clock_repair() {
    let p = Parsed::new(MONO);
    let stts = child(p.table.raw(), b"stts").unwrap();
    let at = stts.header().start as usize + stts.header().header_size as usize + 12;
    let mut raw = MONO.to_vec();
    raw[at..at + 4].copy_from_slice(&2u32.to_be_bytes());
    let q = Parsed::new(&raw);
    let mut input = q.input();
    let mut c = q.cursor(&input);
    assert!(matches!(
        next(&mut c, &mut input, 1),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(q.table.sample_count(), p.table.sample_count());
}
#[test]
fn used_description_remains_raw_when_its_pcm_layout_is_unsupported() {
    let p = Parsed::new(SWITCH);
    let desc = p.table.descriptions()[1].raw();
    let at = desc.header().start as usize + desc.header().header_size as usize + 18;
    let mut raw = SWITCH.to_vec();
    raw[at..at + 2].copy_from_slice(&24u16.to_be_bytes());
    let q = Parsed::new(&raw);
    let mut input = q.input();
    let mut c = q.cursor(&input);
    next(&mut c, &mut input, 1).unwrap();
    assert!(matches!(
        next(&mut c, &mut input, 1),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(
        q.table.descriptions()[1].raw().payload()[18..20],
        24u16.to_be_bytes()
    );
}
