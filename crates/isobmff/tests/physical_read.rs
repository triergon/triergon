mod common;
use common::{child, roots};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_isobmff::{
    BuildStatus, Limits,
    compat::mov::{
        packets::{CursorStatus, PacketLimits},
        recovery::{
            DescriptionContext, DescriptionEvent, DescriptionRead, DescriptionRecoveryBuilder,
            DescriptionRecoveryProfile, DescriptionRecoveryTable, PhysicalRead,
            PhysicalReadProfile, PhysicalReader, RecoveryTrackHeader, RunRecoveryProfile,
        },
        seek::IndexLimits,
    },
};

const BASE: &[u8] = include_bytes!("fixtures/r-front-stco.mov");
struct Source {
    pool: Budget,
    table: DescriptionRecoveryTable,
    header: RecoveryTrackHeader,
    media: Vec<std::ops::Range<u64>>,
    length: u64,
}
impl Source {
    fn new(data: &[u8]) -> Self {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let root = roots(&pool, data).unwrap();
        let movie = root.iter().find(|b| b.header().kind == *b"moov").unwrap();
        let trak = child(movie, b"trak").unwrap();
        let mdia = child(&trak, b"mdia").unwrap();
        let header = RecoveryTrackHeader::parse(
            &child(&trak, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
            RunRecoveryProfile,
        )
        .unwrap();
        let raw = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
        let mut job = DescriptionRecoveryBuilder::new(
            raw,
            pool.clone(),
            Limits::default(),
            DescriptionRecoveryProfile,
        )
        .unwrap();
        while job
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            != BuildStatus::Ready
        {}
        Self {
            pool,
            header,
            table: job.finish().unwrap(),
            media: root
                .iter()
                .filter(|b| b.header().kind == *b"mdat")
                .map(|b| b.header().payload_range().unwrap())
                .collect(),
            length: data.len() as u64,
        }
    }
    fn context(&self) -> DescriptionContext {
        DescriptionContext {
            physical_length: self.length,
            custom_read_maximum: 1,
        }
    }
    fn event(&self) -> DescriptionEvent {
        let mut cursor = self
            .table
            .physical_packet_cursor(
                &self.pool,
                &self.header,
                &self.media,
                PacketLimits::default(),
                self.context(),
                PhysicalReadProfile,
            )
            .unwrap();
        while cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            != CursorStatus::Ready
        {}
        cursor.take_event().unwrap()
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(41),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            7,
        )
        .unwrap()
    }
    fn plan(&self, input: &InputCursor, event: &DescriptionEvent) -> PhysicalRead {
        PhysicalRead::from_event(
            event,
            input,
            self.media[0].clone(),
            16 * 1024 * 1024,
            PhysicalReadProfile,
        )
        .unwrap()
    }
}
fn offset(data: &[u8], start: u64) -> Vec<u8> {
    let source = Source::new(data);
    let raw = child(source.table.raw(), b"stco").unwrap();
    let at = raw.header().start as usize + raw.header().header_size as usize + 8;
    let mut out = data.to_vec();
    out[at..at + 4].copy_from_slice(&u32::try_from(start).unwrap().to_be_bytes());
    out
}
fn short(data: &[u8]) -> Vec<u8> {
    offset(data, data.len() as u64 - 1)
}
fn feed(input: &mut InputCursor, request: triergon_core::InputRequest, data: &[u8]) {
    let start = usize::try_from(request.offset).unwrap();
    let end = start.saturating_add(request.maximum.min(7)).min(data.len());
    assert!(start < end);
    input
        .provide_inline(
            request.source,
            request.token,
            request.offset,
            &data[start..end],
        )
        .unwrap();
}

#[test]
fn original_refusal_and_invalid_source_declaration_are_both_retained() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    assert!(
        source
            .table
            .first_sample_physical_prefix(source.length, PhysicalReadProfile)
            .unwrap()
    );
    let mut old = source
        .table
        .description_packet_cursor(
            &source.pool,
            &source.header,
            &source.media,
            PacketLimits::default(),
            source.context(),
            DescriptionRecoveryProfile,
        )
        .unwrap();
    let error = loop {
        match old.poll(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
            Ok(CursorStatus::Yield) => {}
            Err(error) => break error,
            _ => panic!("original v3 source-range refusal changed"),
        }
    };
    assert_eq!(error, Error::Invalid("sample outside media extent"));
    let event = source.event();
    assert_eq!(event.declaration.range, 746..758);
    assert_eq!(event.packet.as_ref().unwrap().source_range, 746..758);
    assert_eq!(
        event.range_error,
        Some(Error::Invalid("sample outside media extent"))
    );
    assert_eq!(event.read, DescriptionRead::Available);
}

#[test]
fn physical_prefix_bytes_source_token_and_retention_survive_close_for_all_quanta() {
    for quantum in [1, 2, 7, 4096] {
        let bytes = short(BASE);
        let source = Source::new(&bytes);
        let budget = source.pool.clone();
        let mut input = source.input();
        let event = source.event();
        let plan = source.plan(&input, &event);
        assert_eq!(plan.requested(), 746..758);
        assert_eq!(plan.available(), 746..747);
        assert!(plan.is_short_at_physical_end());
        assert!(plan.source_range_error().is_some());
        let mut reader = PhysicalReader::new(
            &mut input,
            event,
            plan,
            16 * 1024 * 1024,
            PhysicalReadProfile,
        )
        .unwrap();
        let output = loop {
            match reader
                .poll(
                    &mut input,
                    &mut WorkBudget::new(quantum),
                    &AtomicBool::new(false),
                )
                .unwrap()
            {
                Step::Ready(output) => break output,
                Step::NeedInput(r) => feed(&mut input, r, &bytes),
                Step::Yield => {}
                Step::End(_) => panic!("prefix was replaced with empty EOF"),
            }
        };
        assert_eq!(output.payload.as_ref(), &bytes[746..]);
        assert_eq!(output.source, SourceId(41));
        assert_eq!(output.token, input.token());
        assert_eq!(output.event.declaration.range, 746..758);
        drop(reader);
        drop(input);
        drop(source);
        assert_eq!(
            budget.used(),
            Budget::control_storage_bytes() + output.payload.retained_storage_bytes()
        );
        assert_eq!(output.payload.as_ref(), [0x7f]);
        drop(output);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn source_mdat_boundary_is_not_replaced_with_the_physical_file_boundary() {
    let mut bytes = BASE.to_vec();
    bytes.extend_from_slice(b"\0\0\0\x08free");
    let bytes = short(&bytes);
    let source = Source::new(&bytes);
    let input = source.input();
    let event = source.event();
    let plan = source.plan(&input, &event);
    assert_eq!(plan.declared_media(), 675..747);
    assert_eq!(plan.physical_length(), 755);
    assert_eq!(plan.available(), 754..755);
    assert!(plan.source_range_error().is_some());
    assert_eq!(event.declaration.range, 754..766);
}

#[test]
fn large_source_prefix_is_factual_without_a_small_buffer_alignment_model() {
    let original = include_bytes!("fixtures/transport-v3/geometry-20.mov");
    let bytes = short(original);
    let source = Source::new(&bytes);
    let input = source.input();
    let event = source.event();
    let plan = source.plan(&input, &event);
    assert_eq!(plan.available(), source.length - 1..source.length);
    assert!(plan.is_short_at_physical_end());
    assert!(event.range_error.is_some());
}

#[test]
fn physically_missing_start_is_not_inferred_to_be_demux_eof() {
    let bytes = offset(BASE, BASE.len() as u64);
    let source = Source::new(&bytes);
    let input = source.input();
    let event = source.event();
    assert_eq!(event.read, DescriptionRead::UnavailablePhysicalStart);
    assert!(matches!(
        PhysicalRead::from_event(
            &event,
            &input,
            source.media[0].clone(),
            4096,
            PhysicalReadProfile
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn original_requested_size_limit_precedes_clipping_the_available_prefix() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let input = source.input();
    let event = source.event();
    assert!(matches!(
        PhysicalRead::from_event(
            &event,
            &input,
            source.media[0].clone(),
            1,
            PhysicalReadProfile
        ),
        Err(Error::Limit {
            requested: 12,
            maximum: 1
        })
    ));
}

#[test]
fn foreign_pool_source_generation_and_request_are_refused_before_allocation() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let mut input = source.input();
    let event = source.event();
    let plan = source.plan(&input, &event);
    let other = Budget::new(4096).unwrap();
    let before = other.used();
    let mut foreign = InputCursor::new(&other, SourceId(41), input.capabilities(), 7).unwrap();
    assert!(matches!(
        PhysicalReader::new(
            &mut foreign,
            event.clone(),
            plan.clone(),
            4096,
            PhysicalReadProfile
        ),
        Err(Error::ForeignPool)
    ));
    assert_eq!(other.used(), before);
    let mut wrong_source =
        InputCursor::new(&source.pool, SourceId(42), input.capabilities(), 7).unwrap();
    assert!(matches!(
        PhysicalReader::new(
            &mut wrong_source,
            event.clone(),
            plan.clone(),
            4096,
            PhysicalReadProfile
        ),
        Err(Error::SourceMismatch)
    ));
    input.request_range(1).unwrap();
    assert!(matches!(
        PhysicalReader::new(&mut input, event.clone(), plan, 4096, PhysicalReadProfile),
        Err(Error::StaleRequest { .. })
    ));
    let plan = source.plan(&input, &event);
    input.reset(0).unwrap();
    assert!(matches!(
        PhysicalReader::new(&mut input, event, plan, 4096, PhysicalReadProfile),
        Err(Error::StaleGeneration { .. })
    ));
}

#[test]
fn zero_work_neither_reads_nor_allocates_after_reader_construction() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let mut input = source.input();
    let event = source.event();
    let plan = source.plan(&input, &event);
    let mut reader =
        PhysicalReader::new(&mut input, event, plan, 4096, PhysicalReadProfile).unwrap();
    let before = source.pool.used();
    let position = input.position();
    for _ in 0..3 {
        assert!(matches!(
            reader
                .poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            Step::Yield
        ));
        assert_eq!(input.position(), position);
        assert_eq!(source.pool.used(), before);
    }
}

#[test]
fn cancellation_is_sticky_and_releases_the_private_byte_buffer() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let mut input = source.input();
    let event = source.event();
    let plan = source.plan(&input, &event);
    let before = source.pool.used();
    let mut reader =
        PhysicalReader::new(&mut input, event, plan, 4096, PhysicalReadProfile).unwrap();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        reader.poll(&mut input, &mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(source.pool.used(), before);
}

#[test]
fn freeze_pressure_retries_do_not_repeat_paid_completion_work() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let mut input = source.input();
    let event = source.event();
    let plan = source.plan(&input, &event);
    let mut reader =
        PhysicalReader::new(&mut input, event, plan, 4096, PhysicalReadProfile).unwrap();
    let Step::NeedInput(request) = reader
        .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("physical source request missing")
    };
    feed(&mut input, request, &bytes);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    let blocker = source.pool.reserve(source.pool.available()).unwrap();
    for attempt in 0..3 {
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            reader.poll(&mut input, &mut work, &AtomicBool::new(false)),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(work.spent(), u64::from(attempt == 0));
    }
    drop(blocker);
    assert!(matches!(
        reader
            .poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    let mut work = WorkBudget::new(1);
    let Step::Ready(output) = reader
        .poll(&mut input, &mut work, &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("retained prefix lost under pressure")
    };
    assert_eq!(work.spent(), 0);
    assert_eq!(output.payload.as_ref(), [0x7f]);
}

#[test]
fn demux_index_keeps_original_sizes_and_all_six_entries_after_short_read() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let mut index = source
        .table
        .physical_index_cursor(
            &source.pool,
            &source.header,
            &source.media,
            IndexLimits::default(),
            source.context(),
            PhysicalReadProfile,
        )
        .unwrap();
    let mut entries = Vec::new();
    loop {
        match index
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                entries.push(index.take_event().unwrap().demux_entry.unwrap());
            }
        }
    }
    assert_eq!(entries.len(), 6);
    assert_eq!(entries[0].position, 746);
    assert_eq!(entries[0].size, 12);
    assert_eq!(entries[1].position, 758);
}

#[test]
fn short_prefix_does_not_discard_a_later_in_source_chunk_or_guess_a_far_seek() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let input = source.input();
    let first = source.event();
    let plan = source.plan(&input, &first);
    let mut next = first.clone();
    next.read = DescriptionRead::UnavailablePhysicalStart;
    next.packet.as_mut().unwrap().source_range = 758..770;
    assert!(plan.modeled_following_eof(&next, PhysicalReadProfile));
    next.read = DescriptionRead::Available;
    next.packet.as_mut().unwrap().source_range = 699..711;
    assert!(!plan.modeled_following_eof(&next, PhysicalReadProfile));
    next.read = DescriptionRead::UnavailablePhysicalStart;
    next.packet.as_mut().unwrap().source_range = 759..771;
    assert!(!plan.modeled_following_eof(&next, PhysicalReadProfile));
    next.packet.as_mut().unwrap().source_range = 33516..33528;
    assert!(!plan.modeled_following_eof(&next, PhysicalReadProfile));
    next.packet.as_mut().unwrap().source_range = 747..747;
    assert!(!plan.modeled_following_eof(&next, PhysicalReadProfile));
}

#[test]
fn empty_demux_request_cannot_mint_a_zero_progress_physical_prefix() {
    let bytes = short(BASE);
    let source = Source::new(&bytes);
    let input = source.input();
    let mut event = source.event();
    event.packet.as_mut().unwrap().source_range = 746..746;
    let before = source.pool.used();
    let token = input.token();
    assert!(matches!(
        PhysicalRead::from_event(
            &event,
            &input,
            source.media[0].clone(),
            16 * 1024 * 1024,
            PhysicalReadProfile
        ),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(source.pool.used(), before);
    assert_eq!(input.token(), token);
}
