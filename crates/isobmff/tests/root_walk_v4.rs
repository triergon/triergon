#[path = "common/transport_v3_cases.rs"]
mod cases;
mod common;
use common::{child, roots};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxHeader, BoxView, BuildStatus, Limits, Mov9CursorStatus, Mov9NativeIndexLimits,
    Mov9PacketLimits, Mov9RecoveryTrackHeader, Mov9RecoveryV3Context, Mov9RecoveryV3Read,
    Mov9RunRecoveryV3Builder, Mov9RunRecoveryV3Table, Mov9V3TransportBuilder,
    Mov9V4RootWalkTransportBuilder, Mov9V4RootWalkTransportModel, PinnedMovD32RegularRecoveryV1,
    PinnedMovD32RegularRecoveryV3, PinnedMovD32RootWalkTransportV4,
    PinnedMovD32SmallRawVideoTransportV3,
};

const BASE: &[u8] = include_bytes!("fixtures/r-front-stco.mov");

struct Source {
    pool: Budget,
    ftyp: BoxView,
    movie: BoxView,
    headers: Vec<BoxHeader>,
    table: Mov9RunRecoveryV3Table,
    track: Mov9RecoveryTrackHeader,
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
        let track = Mov9RecoveryTrackHeader::parse(
            &child(&trak, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
            PinnedMovD32RegularRecoveryV1,
        )
        .unwrap();
        let raw = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
        let mut build = Mov9RunRecoveryV3Builder::new(
            raw,
            pool.clone(),
            Limits::default(),
            PinnedMovD32RegularRecoveryV3,
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
    fn builder(&self, read: usize) -> Result<Mov9V4RootWalkTransportBuilder<'_>, Error> {
        Mov9V4RootWalkTransportBuilder::new(
            self.ftyp.clone(),
            self.movie.clone(),
            &self.headers,
            &self.table,
            &self.pool,
            Mov9RecoveryV3Context {
                physical_length: self.length,
                custom_read_maximum: read,
            },
            PinnedMovD32RootWalkTransportV4,
        )
    }
    fn model(&self, read: usize, work: u64) -> Result<Mov9V4RootWalkTransportModel, Error> {
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

fn set_first_offset(bytes: &[u8], value: u32) -> Vec<u8> {
    let source = Source::new(bytes);
    let raw = child(source.table.raw(), b"stco").unwrap();
    let at = raw.header().start as usize + raw.header().header_size as usize + 8;
    let mut out = bytes.to_vec();
    out[at..at + 4].copy_from_slice(&value.to_be_bytes());
    out
}

fn footer(payload: usize, count: usize) -> Vec<u8> {
    let mut out = BASE.to_vec();
    for _ in 0..count {
        out.extend_from_slice(&u32::try_from(payload + 8).unwrap().to_be_bytes());
        out.extend_from_slice(b"free");
        out.resize(out.len() + payload, 0);
    }
    let length = u32::try_from(out.len()).unwrap();
    set_first_offset(&out, length)
}

#[test]
fn actual_nonterminal_windows_all_remain_in_the_modeled_set_without_selection() {
    let mut checked = 0;
    for &(bytes, read, position, ptr, end, _) in cases::OPEN_WINDOWS {
        let source = Source::new(bytes);
        if source.media[0].end == source.length {
            continue;
        }
        for work in [1, 2, 64] {
            let model = source.model(read, work).unwrap();
            assert!(model.includes_open_window(position, ptr, end));
            let state = model.modeled_open_state();
            assert_eq!(
                (
                    state.source_position_min,
                    state.source_position_max,
                    state.logical_position
                ),
                (source.length, source.length, source.length)
            );
            assert_eq!(model.declared_media(), source.media[0]);
        }
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn original_v3_terminal_only_refusal_is_unchanged() {
    let source = Source::new(&footer(0, 1));
    let old = Mov9V3TransportBuilder::new(
        source.ftyp.clone(),
        source.movie.clone(),
        &source.headers,
        &source.table,
        &source.pool,
        Mov9RecoveryV3Context {
            physical_length: source.length,
            custom_read_maximum: 64,
        },
        PinnedMovD32SmallRawVideoTransportV3,
    );
    assert!(matches!(
        old,
        Err(Error::Unsupported("v3 media is not terminal"))
    ));
    assert!(source.model(64, 1).is_ok());
}

#[test]
fn modeled_read_does_not_repair_the_invalid_source_range_or_clock() {
    let source = Source::new(&footer(0, 1));
    let model = source.model(64, 1).unwrap();
    let mut cursor = source
        .table
        .packet_cursor_v4_with_root_walk(
            &source.pool,
            &source.track,
            &source.media,
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RootWalkTransportV4,
        )
        .unwrap();
    while cursor
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != Mov9CursorStatus::Ready
    {}
    let event = cursor.take_event().unwrap();
    assert_eq!(event.declaration.range, 755..767);
    assert_eq!(event.packet.as_ref().unwrap().source_range, 755..767);
    assert_eq!(event.source_timescale, 1000);
    assert_eq!(
        event.range_error,
        Some(Error::Invalid("sample outside media extent"))
    );
    assert_eq!(event.read, Mov9RecoveryV3Read::ModeledInitialNativeEof);
    let plan = event.modeled_transport_read.unwrap();
    assert_eq!(plan.terminal_callback_position, 755);
    assert_eq!(
        (
            plan.positive_callback_calls_min,
            plan.positive_callback_calls_max
        ),
        (0, 0)
    );
    assert_eq!(model.declared_media(), 675..747);
}

#[test]
fn modeled_eof_requires_a_positive_bounded_request_and_all_window_paths() {
    let source = Source::new(&footer(0, 1));
    for read in [1, 2, 3, 4, 7, 64, 65536] {
        let model = source.model(read, 1).unwrap();
        assert!(model.models_initial_native_eof(755..767));
        assert!(model.models_initial_native_eof(33523..33535));
        for range in [754..766, 755..755, 33524..33536, 755..16777972] {
            assert!(!model.models_initial_native_eof(range));
        }
    }
    assert!(matches!(source.builder(0), Err(Error::Unsupported(_))));
    assert!(matches!(source.builder(5), Err(Error::Unsupported(_))));
}

#[test]
fn footer_payloads_preserve_the_terminal_probe_position_instead_of_forcing_eof() {
    for payload in [1, 8, 9, 16, 64, 32768] {
        let source = Source::new(&footer(payload, 1));
        let model = source.model(1, 1).unwrap();
        let probe = if payload >= 8 { payload.min(16) } else { 0 };
        assert_eq!(
            model.modeled_open_state().logical_position,
            755 + probe as u64
        );
        assert_eq!(model.declared_media(), 675..747);
        assert!(model.models_initial_native_eof(source.length..source.length + 12));
        if payload > 16 {
            assert!(model.modeled_open_state().logical_position < source.length);
            assert!(
                model
                    .initial_native_read_plan(source.length..source.length + 12)
                    .unwrap()
                    .positive_callback_calls_min
                    > 0
            );
        }
    }
    for count in [2, 3] {
        let source = Source::new(&footer(0, count));
        let model = source.model(64, 1).unwrap();
        assert_eq!(model.declared_media(), 675..747);
        assert_eq!(model.modeled_open_state().logical_position, source.length);
    }
}

#[test]
fn existing_large_free_observations_are_all_retained_after_an_in_source_skip() {
    let mut checked = 0;
    for &(bytes, read, position, ptr, end, _) in cases::OPEN_WINDOWS {
        if bytes.len() <= 32768 {
            continue;
        }
        let source = Source::new(bytes);
        let model = source.model(read, 1).unwrap();
        assert!(model.includes_open_window(position, ptr, end));
        assert_eq!(model.modeled_absolute_seek_bounds(), (1, 1));
        assert_eq!(
            model.modeled_open_state().logical_position,
            source.media[0].start
        );
        assert_eq!(model.physical_length(), source.length);
        assert!(model.models_initial_native_eof(source.length..source.length + 12));
        checked += 1;
    }
    assert_eq!(checked, 4);
}

#[test]
fn every_source_root_header_is_retained_as_original_geometry() {
    let source = Source::new(&footer(0, 3));
    let model = source.model(64, 1).unwrap();
    let headers: Vec<_> = model.source_root_headers().copied().collect();
    assert_eq!(headers, source.headers);
    assert_eq!(model.declared_media(), source.media[0]);
    assert_ne!(model.declared_media().end, model.physical_length());
}

#[test]
fn large_physical_length_is_not_itself_a_source_eof_receipt() {
    let &(bytes, _, _, _, _, _) = cases::OPEN_WINDOWS
        .iter()
        .find(|r| r.0.len() > 32768)
        .unwrap();
    let source = Source::new(bytes);
    let model = source.model(64, 1).unwrap();
    assert!(!model.models_initial_native_eof(source.media[0].start..source.media[0].start + 12));
    assert!(!model.models_initial_native_eof(source.length..source.length));
    assert!(!model.models_initial_native_eof(source.length + 32769..source.length + 32781));
    assert!(model.modeled_open_state().source_position_min < source.length);
}

#[test]
fn root_operation_paths_are_bounded_and_independent_of_poll_quantum() {
    let &(bytes, _, _, _, _, _) = cases::OPEN_WINDOWS
        .iter()
        .find(|r| r.0.len() > 32768)
        .unwrap();
    let source = Source::new(bytes);
    for read in [1, 3, 64, 65536] {
        let small = source.model(read, 1).unwrap();
        let large = source.model(read, 4096).unwrap();
        assert_eq!(small.modeled_open_state(), large.modeled_open_state());
        assert_eq!(
            small.modeled_absolute_seek_bounds(),
            large.modeled_absolute_seek_bounds()
        );
        assert!(small.modeled_open_state().possible_windows <= 64);
    }
}

#[test]
fn terminal_media_and_trailing_probe_are_distinct_operation_endpoints() {
    let terminal = Source::new(BASE);
    let short = Source::new(&footer(1, 1));
    let m = terminal.model(1, 1).unwrap();
    let f = short.model(1, 1).unwrap();
    assert_eq!(m.modeled_open_state().logical_position, 675);
    assert_eq!(f.modeled_open_state().logical_position, 755);
    assert_eq!(f.physical_length(), 756);
    assert_eq!(
        f.initial_native_read_plan(756..768)
            .unwrap()
            .positive_callback_calls_min,
        1
    );
}

#[test]
fn unrepresentable_or_discontinuous_source_roots_remain_refused() {
    let source = Source::new(BASE);
    assert!(matches!(
        Mov9V4RootWalkTransportBuilder::new(
            source.ftyp.clone(),
            source.movie.clone(),
            &source.headers,
            &source.table,
            &source.pool,
            Mov9RecoveryV3Context {
                physical_length: i64::MAX as u64 + 1,
                custom_read_maximum: 64
            },
            PinnedMovD32RootWalkTransportV4
        ),
        Err(Error::Unsupported(_))
    ));
    let mut source = Source::new(&footer(0, 1));
    source.headers[0].start = 1;
    assert!(matches!(source.model(64, 1), Err(Error::Unsupported(_))));
}

#[test]
fn foreign_pool_and_changed_root_or_media_binding_fail_closed() {
    let mut source = Source::new(&footer(0, 1));
    let foreign = Budget::new(1 << 20).unwrap();
    assert!(matches!(
        Mov9V4RootWalkTransportBuilder::new(
            source.ftyp.clone(),
            source.movie.clone(),
            &source.headers,
            &source.table,
            &foreign,
            Mov9RecoveryV3Context {
                physical_length: source.length,
                custom_read_maximum: 64
            },
            PinnedMovD32RootWalkTransportV4
        ),
        Err(Error::ForeignPool)
    ));
    let model = source.model(64, 1).unwrap();
    let media_extent = 675..755;
    assert!(matches!(
        source.table.packet_cursor_v4_with_root_walk(
            &source.pool,
            &source.track,
            std::slice::from_ref(&media_extent),
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RootWalkTransportV4
        ),
        Err(Error::Unsupported(_))
    ));
    source.headers[3].start += 1;
    assert!(matches!(source.model(64, 1), Err(Error::Unsupported(_))));
}

#[test]
fn zero_work_yields_and_cancellation_stays_terminal() {
    let source = Source::new(&footer(0, 1));
    let cancel = AtomicBool::new(false);
    let mut builder = source.builder(64).unwrap();
    let before = source.pool.used();
    assert_eq!(
        builder.poll(&mut WorkBudget::new(0), &cancel),
        Ok(BuildStatus::Yield)
    );
    assert_eq!(source.pool.used(), before);
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(64), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(builder.finish(), Err(Error::Cancelled)));
}

#[test]
fn retained_model_keeps_exact_source_bytes_after_close_then_releases_owners() {
    let source = Source::new(&footer(0, 1));
    let pool = source.pool.clone();
    let movie = source.movie.bytes().as_ref().to_vec();
    let model = source.model(64, 1).unwrap();
    drop(source);
    assert_eq!(model.source_movie().bytes().as_ref(), movie);
    assert!(model.source_file_type().bytes().belongs_to(&pool));
    assert_eq!(model.declared_media(), 675..747);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(model);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn all_native_index_declarations_survive_even_when_initial_packet_is_eof() {
    let source = Source::new(&footer(0, 1));
    let model = source.model(64, 1).unwrap();
    let mut index = source
        .table
        .index_cursor_v4_with_root_walk(
            &source.pool,
            &source.track,
            &source.media,
            Mov9NativeIndexLimits::default(),
            &model,
            PinnedMovD32RootWalkTransportV4,
        )
        .unwrap();
    let mut entries = Vec::new();
    loop {
        match index
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::End => break,
            Mov9CursorStatus::Ready => {
                entries.push(index.take_event().unwrap().native_entry.unwrap());
            }
        }
    }
    assert_eq!(entries.len(), 6);
    assert_eq!(entries[0].position, 755);
    assert_eq!(entries[1].position, 767);
    assert!(entries.iter().all(|e| e.size == 12));
    let limits = Mov9NativeIndexLimits {
        max_entries: 0,
        ..Mov9NativeIndexLimits::default()
    };
    assert!(matches!(
        source
            .table
            .index_cursor_v4_with_root_walk(
                &source.pool,
                &source.track,
                &source.media,
                limits,
                &model,
                PinnedMovD32RootWalkTransportV4
            )
            .unwrap()
            .poll(&mut WorkBudget::new(64), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn cancelled_pending_event_cannot_be_published_after_resetting_cancel() {
    let source = Source::new(&footer(0, 1));
    let model = source.model(64, 1).unwrap();
    let mut cursor = source
        .table
        .packet_cursor_v4_with_root_walk(
            &source.pool,
            &source.track,
            &source.media,
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RootWalkTransportV4,
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    while cursor.poll(&mut WorkBudget::new(1), &cancel).unwrap() != Mov9CursorStatus::Ready {}
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        cursor.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(cursor.take_event(), Err(Error::Cancelled)));
}
