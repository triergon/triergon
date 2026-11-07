mod common;
use common::{child, roots};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_isobmff::{
    BoxHeader, BoxView, BuildStatus, Limits,
    compat::mov::{
        packets::PacketLimits,
        recovery::{
            DescriptionContext, DescriptionRecoveryBuilder, DescriptionRecoveryProfile,
            DescriptionRecoveryTable, RecoveryTrackHeader, RunRecoveryProfile,
        },
    },
    reference_trace::mov::{
        operation::{InitialOperation, InitialOperationJob, RejectedPacketSeekProfile},
        transport::{RootWalkTransportBuilder, RootWalkTransportModel, RootWalkTransportProfile},
    },
};
const BASE: &[u8] = include_bytes!("fixtures/transport-v3/geometry-00.mov");
struct Source {
    pool: Budget,
    ftyp: BoxView,
    movie: BoxView,
    headers: Vec<BoxHeader>,
    table: DescriptionRecoveryTable,
    track: RecoveryTrackHeader,
    media: Vec<Range<u64>>,
    length: u64,
}
impl Source {
    fn new(bytes: &[u8]) -> Self {
        let pool = Budget::new(1 << 20).unwrap();
        let top = roots(&pool, bytes).unwrap();
        let ftyp = top
            .iter()
            .find(|b| b.header().kind == *b"ftyp")
            .unwrap()
            .clone();
        let movie = top
            .iter()
            .find(|b| b.header().kind == *b"moov")
            .unwrap()
            .clone();
        let trak = child(&movie, b"trak").unwrap();
        let mdia = child(&trak, b"mdia").unwrap();
        let track = RecoveryTrackHeader::parse(
            &child(&trak, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
            RunRecoveryProfile,
        )
        .unwrap();
        let raw = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
        let mut build = DescriptionRecoveryBuilder::new(
            raw,
            pool.clone(),
            Limits::default(),
            DescriptionRecoveryProfile,
        )
        .unwrap();
        while build
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            != BuildStatus::Ready
        {}
        let table = build.finish().unwrap();
        Self {
            pool,
            ftyp,
            movie,
            headers: top.iter().map(|b| *b.header()).collect(),
            table,
            track,
            media: top
                .iter()
                .filter(|b| b.header().kind == *b"mdat")
                .map(|b| b.header().payload_range().unwrap())
                .collect(),
            length: bytes.len() as u64,
        }
    }
    fn builder(&self, read: usize) -> Result<RootWalkTransportBuilder<'_>, Error> {
        RootWalkTransportBuilder::new(
            self.ftyp.clone(),
            self.movie.clone(),
            &self.headers,
            &self.table,
            &self.pool,
            DescriptionContext {
                physical_length: self.length,
                custom_read_maximum: read,
            },
            RootWalkTransportProfile,
        )
    }
    fn model(&self, read: usize, work: u64) -> Result<RootWalkTransportModel, Error> {
        let mut job = self.builder(read)?;
        for _ in 0..10000 {
            if job.poll(&mut WorkBudget::new(work), &AtomicBool::new(false))? == BuildStatus::Ready
            {
                return job.finish();
            }
        }
        panic!("bounded transport builder failed to progress");
    }
}

fn offset(value: u32) -> Vec<u8> {
    let mut data = BASE.to_vec();
    let at = data.windows(4).position(|v| v == b"stco").unwrap() + 12;
    data[at..at + 4].copy_from_slice(&value.to_be_bytes());
    data
}
impl Source {
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(57),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            4096,
        )
        .unwrap()
    }
    fn probe<'a>(
        &'a self,
        model: &'a RootWalkTransportModel,
        input: &InputCursor,
        maximum: usize,
    ) -> Result<InitialOperationJob<'a>, Error> {
        InitialOperationJob::new(
            &self.table,
            &self.track,
            &self.media,
            model,
            input,
            PacketLimits::default(),
            maximum,
            RejectedPacketSeekProfile,
        )
    }
}
fn run(
    job: &mut InitialOperationJob<'_>,
    input: &mut InputCursor,
    work: u64,
) -> Result<InitialOperation, Error> {
    let cancel = AtomicBool::new(false);
    for _ in 0..10000 {
        let mut quantum = WorkBudget::new(work);
        let status = job.poll(input, &mut quantum, &cancel)?;
        assert!(quantum.spent() <= work);
        if status == BuildStatus::Ready {
            return job.try_finish(input, &cancel);
        }
    }
    panic!("bounded first operation did not finish");
}
#[test]
fn failed_seek_retains_invalid_source_and_literal_read_result_for_every_quantum() {
    for quantum in [1, 2, 7, 4096] {
        let source = Source::new(&offset(33444));
        let model = source.model(1, quantum).unwrap();
        let mut input = source.input();
        let before = input.token();
        let mut job = source.probe(&model, &input, 1 << 24).unwrap();
        let result = run(&mut job, &mut input, quantum)
            .unwrap()
            .into_rejected_seek()
            .unwrap();
        assert_eq!(result.demux_read_result(), -1094995529);
        assert_eq!(result.demux_packets_delivered(), 0);
        assert!(!result.demux_catalogue_published());
        assert!(!result.demux_confirmed_eof());
        assert_eq!(
            result.source_error(),
            Error::Invalid("range exceeds source length")
        );
        assert_eq!(result.source_event().declaration.range, 33444..33456);
        assert_eq!(
            result.source_event().range_error,
            Some(Error::Invalid("sample outside media extent"))
        );
        assert_eq!(
            result.source_table().bytes().as_ref(),
            source.table.raw().bytes().as_ref()
        );
        assert_eq!(
            result.source_mdhd().bytes().as_ref(),
            source.track.source().raw_mdhd.bytes().as_ref()
        );
        assert_eq!(input.position(), 0);
        assert_eq!(input.token(), before);
    }
}
#[test]
fn independent_exact_discard_boundaries_do_not_turn_small_read_quanta_into_eof() {
    for (read, last_eof, first_error) in [
        (1, 33443, 33444),
        (3, 33444, 33445),
        (64, 33497, 33498),
        (65536, 33515, 33516),
    ] {
        for (at, fails) in [
            (747, false),
            (last_eof, false),
            (first_error, true),
            (u32::MAX, true),
        ] {
            let source = Source::new(&offset(at));
            let model = source.model(read, 1).unwrap();
            let mut input = source.input();
            let mut job = source.probe(&model, &input, 1 << 24).unwrap();
            assert_eq!(
                run(&mut job, &mut input, 1)
                    .unwrap()
                    .rejected_seek()
                    .is_some(),
                fails,
                "read={read}, offset={at}"
            );
        }
    }
}
#[test]
fn no_error_projection_for_available_source_payload() {
    let source = Source::new(BASE);
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
    assert!(
        run(&mut job, &mut input, 1)
            .unwrap()
            .rejected_seek()
            .is_none()
    );
}
#[test]
fn zero_work_cannot_read_allocate_advance_or_publish() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
    let before = source.pool.used();
    let token = input.token();
    for _ in 0..8 {
        assert_eq!(
            job.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            BuildStatus::Yield
        );
        assert_eq!(source.pool.used(), before);
        assert_eq!(input.token(), token);
        assert_eq!(input.position(), 0);
    }
    assert!(
        run(&mut job, &mut input, 1)
            .unwrap()
            .rejected_seek()
            .is_some()
    );
}
#[test]
fn cancellation_is_sticky_and_never_publishes_a_demux_error() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let before = source.pool.used();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        job.poll(&mut input, &mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut input, &mut WorkBudget::new(4096), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        job.try_finish(&mut input, &cancel).unwrap_err(),
        Error::Cancelled
    );
    drop(job);
    assert_eq!(source.pool.used(), before);
}
#[test]
fn cancellation_after_ready_refuses_publication() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
    let cancel = AtomicBool::new(false);
    while job
        .poll(&mut input, &mut WorkBudget::new(1), &cancel)
        .unwrap()
        != BuildStatus::Ready
    {}
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        job.try_finish(&mut input, &cancel).unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn foreign_pool_refuses_before_parsing_or_range_request() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    let input = InputCursor::new(
        &other,
        SourceId(57),
        SourceCapabilities {
            length: Some(source.length),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    assert_eq!(
        source.probe(&model, &input, 1 << 24).unwrap_err(),
        Error::ForeignPool
    );
}
#[test]
fn source_swap_refuses_and_poison_is_sticky() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let input = source.input();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
    let mut wrong = InputCursor::new(
        &source.pool,
        SourceId(58),
        SourceCapabilities {
            length: Some(source.length),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    assert_eq!(
        job.poll(
            &mut wrong,
            &mut WorkBudget::new(4096),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::SourceMismatch
    );
}
#[test]
fn stale_generation_and_request_cannot_select_a_different_operation() {
    for generation in [false, true] {
        let source = Source::new(&offset(40000));
        let model = source.model(1, 1).unwrap();
        let mut input = source.input();
        let mut job = source.probe(&model, &input, 1 << 24).unwrap();
        if generation {
            input.reset(0).unwrap();
        } else {
            input.request_range(0).unwrap();
        }
        let error = job
            .poll(
                &mut input,
                &mut WorkBudget::new(4096),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(if generation {
            matches!(error, Error::StaleGeneration { .. })
        } else {
            matches!(error, Error::StaleRequest { .. })
        });
    }
}
#[test]
fn transport_failure_is_never_demux_invaliddata_or_eof() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
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
fn packet_policy_limit_precedes_an_error_projection_without_allocating_payload() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let mut job = source.probe(&model, &input, 11).unwrap();
    let before = source.pool.used();
    assert_eq!(
        run(&mut job, &mut input, 1).unwrap_err(),
        Error::Limit {
            requested: 12,
            maximum: 11
        }
    );
    assert_eq!(source.pool.used(), before);
}
#[test]
fn unknown_or_nonseekable_source_is_an_explicit_refusal() {
    let source = Source::new(&offset(40000));
    let model = source.model(1, 1).unwrap();
    for capabilities in [
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        SourceCapabilities {
            length: Some(source.length),
            random_access: false,
        },
    ] {
        let input = InputCursor::new(&source.pool, SourceId(57), capabilities, 4096).unwrap();
        assert!(matches!(
            source.probe(&model, &input, 1 << 24),
            Err(Error::Unsupported(_))
        ));
    }
}
#[test]
fn retained_source_facts_outlive_every_job_model_and_input_backing() {
    let source = Source::new(&offset(40000));
    let pool = source.pool.clone();
    let model = source.model(1, 1).unwrap();
    let mut input = source.input();
    let mut job = source.probe(&model, &input, 1 << 24).unwrap();
    let output = run(&mut job, &mut input, 1)
        .unwrap()
        .into_rejected_seek()
        .unwrap();
    let raw = output.source_table().bytes().as_ref().to_vec();
    drop(job);
    drop(model);
    drop(input);
    drop(source);
    assert_eq!(output.source_table().bytes().as_ref(), raw);
    assert_eq!(output.source_event().declaration.range, 40000..40012);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(output);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
