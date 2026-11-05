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
    Mov9RunRecoveryV3Builder, Mov9RunRecoveryV3Table, Mov9V3EntryRevisit, Mov9V3TransportBuilder,
    Mov9V3TransportModel, PinnedMovD32RegularRecoveryV1, PinnedMovD32RegularRecoveryV3,
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
    fn builder(&self, read: usize) -> Result<Mov9V3TransportBuilder<'_>, Error> {
        Mov9V3TransportBuilder::new(
            self.ftyp.clone(),
            self.movie.clone(),
            &self.headers,
            &self.table,
            &self.pool,
            Mov9RecoveryV3Context {
                physical_length: self.length,
                custom_read_maximum: read,
            },
            PinnedMovD32SmallRawVideoTransportV3,
        )
    }
    fn model(&self, read: usize, work: u64) -> Result<Mov9V3TransportModel, Error> {
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

#[test]
fn all84_independent_geometry_read_windows_are_members_or_explicitly_unreviewed() {
    assert_eq!(cases::OPEN_WINDOWS.len(), 84);
    for &(bytes, read, position, ptr, end, supported) in cases::OPEN_WINDOWS {
        let source = Source::new(bytes);
        let model = source.model(read, 1);
        if supported {
            let model = model.unwrap();
            assert!(
                model.includes_open_window(position, ptr, end),
                "read={read} length={}",
                bytes.len()
            );
            assert!(model.models_initial_native_eof(source.length..source.length + 12));
        } else {
            assert!(matches!(model, Err(Error::Unsupported(_))));
        }
    }
}

#[test]
fn cursor_model_keeps_uncertain_alignment_and_source_ranges_explicit() {
    let bytes = set_first_offset(BASE, 33444);
    let source = Source::new(&bytes);
    let model = source.model(64, 1).unwrap();
    let state = model.modeled_open_state();
    assert_eq!(
        (
            state.source_position_min,
            state.source_position_max,
            state.logical_position
        ),
        (704, 729, 675)
    );
    assert_eq!(
        state.entry_revisit,
        Mov9V3EntryRevisit::OneWindowCallbackSet
    );
    assert!(model.includes_open_window(729, 10, 64));
    assert!(!model.includes_open_window(730, 11, 64));
    let context = Mov9RecoveryV3Context {
        physical_length: source.length,
        custom_read_maximum: 64,
    };
    assert_eq!(
        context
            .read_disposition(33444, source.media[0].clone())
            .unwrap(),
        Mov9RecoveryV3Read::UnavailablePhysicalStart
    );
    let mut cursor = source
        .table
        .packet_cursor_v3_with_transport(
            &source.pool,
            &source.track,
            &source.media,
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RegularRecoveryV3,
        )
        .unwrap();
    while cursor
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != Mov9CursorStatus::Ready
    {}
    let event = cursor.take_event().unwrap();
    assert_eq!(event.read, Mov9RecoveryV3Read::ModeledInitialNativeEof);
    assert_eq!(event.declaration.range, 33444..33456);
    assert_eq!(event.declaration.dts_ticks, Some(0));
    assert!(event.range_error.is_some());
    assert_eq!(event.packet.unwrap().source_range, 33444..33456);
}

#[test]
fn boundary_and_quantum_controls_do_not_turn_native_seek_refusal_into_eof() {
    let source = Source::new(BASE);
    let full = source.model(65536, 1).unwrap();
    assert_eq!(full.modeled_open_state().source_position_min, 747);
    assert!(full.models_initial_native_eof(33515..33527));
    assert!(!full.models_initial_native_eof(33516..33528));
    let tiny = source.model(1, 1).unwrap();
    assert!(tiny.models_initial_native_eof(33443..33455));
    assert!(!tiny.models_initial_native_eof(33444..33456));
    let three = source.model(3, 1).unwrap();
    assert!(three.models_initial_native_eof(33444..33456));
    assert!(!three.models_initial_native_eof(33445..33457));
    let sixty_four = source.model(64, 1).unwrap();
    assert!(sixty_four.models_initial_native_eof(33445..33457));
    assert!(!sixty_four.models_initial_native_eof(33473..33485));
    for read in [0, 5, 6, 65, 32768, 65537, usize::MAX] {
        assert!(matches!(source.builder(read), Err(Error::Unsupported(_))));
    }
}

#[test]
fn empty_oversized_or_source_available_reads_do_not_get_modeled_eof() {
    let source = Source::new(BASE);
    let model = source.model(65536, 1).unwrap();
    assert!(!model.models_initial_native_eof(747..747));
    assert!(!model.models_initial_native_eof(747..u64::MAX));
    assert!(!model.models_initial_native_eof(675..687));
}

#[test]
fn index_keeps_every_raw_entry_after_the_first_modeled_packet_eof() {
    let bytes = set_first_offset(BASE, 33444);
    let source = Source::new(&bytes);
    let model = source.model(65536, 1).unwrap();
    let mut index = source
        .table
        .index_cursor_v3_with_transport(
            &source.pool,
            &source.track,
            &source.media,
            Mov9NativeIndexLimits::default(),
            &model,
            PinnedMovD32RegularRecoveryV3,
        )
        .unwrap();
    let mut positions = Vec::new();
    loop {
        match index
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::Ready => {
                positions.push(index.take_event().unwrap().native_entry.unwrap().position)
            }
            Mov9CursorStatus::End => break,
        }
    }
    assert_eq!(positions, [33444, 33456, 699, 711, 723, 735]);
}

#[test]
fn model_cannot_cross_source_table_or_pool_bindings() {
    let first = set_first_offset(BASE, 33444);
    let source = Source::new(&first);
    let model = source.model(65536, 1).unwrap();
    let second = set_first_offset(BASE, 33445);
    let other_pool_source = Source::new(&second);
    let before = other_pool_source.pool.used();
    assert!(matches!(
        other_pool_source.table.packet_cursor_v3_with_transport(
            &other_pool_source.pool,
            &other_pool_source.track,
            &other_pool_source.media,
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RegularRecoveryV3
        ),
        Err(Error::ForeignPool)
    ));
    assert_eq!(other_pool_source.pool.used(), before);
    let top = roots(&source.pool, &second).unwrap();
    let moov = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let mdia = child(&child(moov, b"trak").unwrap(), b"mdia").unwrap();
    let raw = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
    let mut builder = Mov9RunRecoveryV3Builder::new(
        raw,
        source.pool.clone(),
        Limits::default(),
        PinnedMovD32RegularRecoveryV3,
    )
    .unwrap();
    while builder
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != BuildStatus::Ready
    {}
    let other_table = builder.finish().unwrap();
    assert!(matches!(
        other_table.packet_cursor_v3_with_transport(
            &source.pool,
            &source.track,
            &source.media,
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RegularRecoveryV3
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn zero_work_cancel_ready_cancel_and_schedules_keep_source_bytes_unchanged() {
    let source = Source::new(BASE);
    let raw = source.movie.bytes().clone();
    let mut builder = source.builder(64).unwrap();
    assert_eq!(
        builder.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStatus::Yield)
    );
    let cancel = AtomicBool::new(true);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        builder.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(builder.finish(), Err(Error::Cancelled)));
    let a = source.model(64, 1).unwrap();
    let b = source.model(64, 4096).unwrap();
    assert_eq!(a.modeled_open_state(), b.modeled_open_state());
    assert_eq!(source.movie.bytes().as_ref(), raw.as_ref());
    let mut ready = source.builder(64).unwrap();
    while ready.poll(&mut WorkBudget::new(4096), &cancel).unwrap() != BuildStatus::Ready {}
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        ready.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(ready.finish(), Err(Error::Cancelled)));
}

#[test]
fn altered_source_geometry_or_configuration_is_refused_without_repair() {
    let mut source = Source::new(BASE);
    source.headers[0].start = 1;
    assert!(matches!(source.model(64, 1), Err(Error::Unsupported(_))));
    let original = Source::new(BASE);
    let entry = original.table.descriptions()[0].raw();
    let at = entry.header().start as usize + entry.header().header_size as usize + 24;
    let mut changed = BASE.to_vec();
    changed[at..at + 2].copy_from_slice(&3u16.to_be_bytes());
    let changed_source = Source::new(&changed);
    assert!(matches!(
        changed_source.model(64, 1),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(
        changed_source.table.descriptions()[0].raw().payload()[24..26],
        [0, 3]
    );
}

#[test]
fn metadata_owners_survive_caller_close_and_release_after_model_close() {
    let source = Source::new(BASE);
    let pool = source.pool.clone();
    let model = source.model(64, 1).unwrap();
    let movie = source.movie.bytes().as_ref().to_vec();
    drop(source);
    assert_eq!(model.source_movie().bytes().as_ref(), movie);
    assert!(model.source_file_type().bytes().belongs_to(&pool));
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(model);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn modeled_pending_packet_and_index_cancellation_clears_ready_outputs() {
    let bytes = set_first_offset(BASE, 33444);
    let source = Source::new(&bytes);
    let model = source.model(65536, 1).unwrap();
    let cancel = AtomicBool::new(false);
    let mut packet = source
        .table
        .packet_cursor_v3_with_transport(
            &source.pool,
            &source.track,
            &source.media,
            Mov9PacketLimits::default(),
            &model,
            PinnedMovD32RegularRecoveryV3,
        )
        .unwrap();
    while packet.poll(&mut WorkBudget::new(1), &cancel).unwrap() != Mov9CursorStatus::Ready {}
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        packet.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(packet.take_event(), Err(Error::Cancelled)));
    cancel.store(false, Ordering::Relaxed);
    let limits = Mov9NativeIndexLimits {
        max_entries: 0,
        ..Mov9NativeIndexLimits::default()
    };
    let mut index = source
        .table
        .index_cursor_v3_with_transport(
            &source.pool,
            &source.track,
            &source.media,
            limits,
            &model,
            PinnedMovD32RegularRecoveryV3,
        )
        .unwrap();
    let error = loop {
        match index.poll(&mut WorkBudget::new(1), &cancel) {
            Ok(Mov9CursorStatus::Yield) => {}
            Err(error) => break error,
            _ => panic!("zero-entry policy was waived"),
        }
    };
    assert!(matches!(error, Error::Limit { .. }));
}
