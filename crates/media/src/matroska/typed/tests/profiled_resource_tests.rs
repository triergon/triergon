//! Resource/progress guards for the observed d288 performance failures.
//! This file is private/unexecuted until the parent publishes tests alone.
use super::*;
use crate::{
    Error, Limits,
    incremental::{ElementEvent, ElementReader},
    metadata::{
        CHAPTERS, SharedDocument,
        typed_document::{DocumentJob, DocumentLimits},
    },
};
use std::mem::size_of;

fn chapter(uid: u64, start: u64, end: Option<u64>, nested: &[u8]) -> Vec<u8> {
    let mut payload = [uint(0x73c4, uid), uint(0x91, start)].concat();
    if let Some(end) = end {
        payload.extend(uint(0x92, end));
    }
    payload.extend(nested);
    element(0xb6, &payload)
}
fn parse_chapters(raw: &[u8], quantum: u64) -> crate::Result<(SharedDocument, u64)> {
    let pool = Budget::new(2 * 1024 * 1024)?;
    let source = SharedBytes::copy_from_slice(&pool, raw)?;
    let mut job = DocumentJob::new(
        CHAPTERS,
        source,
        DocumentLimits {
            node_limit: 8192,
            depth_limit: 64,
            storage_limit: 1024 * 1024,
            attachment_limit: 0,
        },
        Some(123),
        &pool,
    )?;
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &cancel)?,
        ParseStep::Yield
    ));
    let mut spent = 0;
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        let result = job.step(&mut work, &cancel)?;
        assert!(work.spent() <= quantum);
        spent += work.spent();
        match result {
            ParseStep::Event(document) => return Ok((document, spent)),
            ParseStep::Yield => (),
            other => panic!("complete bounded source: {other:?}"),
        }
    }
    panic!("bounded document did not finish");
}

#[test]
fn profiled_flat_chapter_work_does_not_rescan_all_siblings_for_each_atom() {
    let authored = |count| {
        let mut payload = Vec::new();
        for i in 0..count {
            payload.extend(chapter(i + 1, 2 * i, Some(2 * i + 1), &[]));
        }
        // The flag follows all chapters; encounter order cannot supply the rule.
        payload.extend(uint(0x45dd, 1));
        element(0x45b9, &payload)
    };
    let small = authored(64);
    let large = authored(512);
    let (a, small_work) = parse_chapters(&small, 7).unwrap();
    let (b, large_work) = parse_chapters(&large, 7).unwrap();
    assert_eq!(a.raw(), small.as_slice());
    assert_eq!(b.raw(), large.as_slice());
    assert_eq!(a.node_count(), 4 * 64 + 3);
    assert_eq!(b.node_count(), 4 * 512 + 3);
    println!("chapter work64={small_work} work512={large_work}");
    // Eight times the input permits sixteen times the real work, including UID
    // sorting and arena growth. Repeated full sibling scans exceed this bound.
    assert!(
        large_work <= small_work * 16,
        "unrelated sibling rescans: {small_work} -> {large_work}"
    );
}

#[test]
fn profiled_chapter_context_keeps_late_ordering_flags_and_sibling_editions_separate() {
    for quantum in [1, 7, 8192] {
        let inner = chapter(2, 10, Some(20), &element(0x4abc, b"opaque"));
        let first = element(
            0x45b9,
            &[chapter(1, 0, Some(50), &inner), uint(0x45dd, 1)].concat(),
        );
        let second = element(0x45b9, &chapter(3, 100, None, &[]));
        let raw = [first, second].concat();
        let (document, _) = parse_chapters(&raw, quantum).unwrap();
        assert_eq!(document.raw(), raw.as_slice());
        assert_eq!(document.source_offset(), Some(123));
        let alias = document.clone();
        drop(document);
        assert_eq!(alias.raw(), raw.as_slice());

        let bad_inner = chapter(2, 51, Some(60), &[]);
        let bad_range = element(0x45b9, &chapter(1, 0, Some(50), &bad_inner));
        assert!(matches!(
            parse_chapters(&bad_range, quantum),
            Err(Error::Invalid("nested chapter time range"))
        ));
        let missing_end = element(
            0x45b9,
            &[chapter(1, 0, None, &[]), uint(0x45dd, 1)].concat(),
        );
        assert!(matches!(
            parse_chapters(&missing_end, quantum),
            Err(Error::Invalid("missing metadata field"))
        ));
    }
}

#[test]
fn profiled_unlaced_batch_needs_no_heap_pointer_arena() {
    let pool = Budget::new(128 * 1024).unwrap();
    let config = seed_configuration(&pool, &[]);
    let source = SharedBytes::copy_from_slice(&pool, &[0x81, 0, 0, 0x80, b'V']).unwrap();
    let mut job =
        packet::PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool)
            .unwrap();
    let before = pool.used();
    println!(
        "Packet={} PacketBatch={}",
        size_of::<crate::matroska::Packet>(),
        size_of::<packet::PacketBatchJob>()
    );
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        ParseStep::Yield
    ));
    let mut packet = None;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(1);
        let result = job.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        assert_eq!(
            pool.used(),
            before,
            "one validated frame needs no pointer-vector allocation"
        );
        match result {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                packet = Some(value);
                break;
            }
            other => panic!("one authored packet: {other:?}"),
        }
    }
    let packet = packet.unwrap();
    drop(job);
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(packet.configuration.tracks()[0].number, 1);
}

fn collected_leaf(hidden_parent: bool, quantum: u64) -> (Budget, usize, SharedBytes, SharedBytes) {
    let header = element(0x1a45dfa3, &element(0x4282, b"matroska"));
    let info = uint(0x2ad7b1, 1_000_000);
    let info_element = element(0x1549a966, &info);
    let segment = element(0x18538067, &info_element);
    let start = header.len() + segment.len() - info.len();
    let raw = [header, segment].concat();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(901),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut input_owner = None;
    let mut output = None;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(quantum);
        let step = reader.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= quantum);
        match step {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let bytes = if at == start {
                    assert!(request.maximum >= info.len());
                    let bytes = if hidden_parent {
                        Bytes::copy_from_slice(&pool, &raw)
                            .unwrap()
                            .slice(start..raw.len())
                            .unwrap()
                    } else {
                        Bytes::copy_from_slice(&pool, &info).unwrap()
                    };
                    input_owner = Some(SharedBytes::from_core(bytes.clone()));
                    bytes
                } else {
                    let end = (at + request.maximum).min(raw.len());
                    assert!(at < end);
                    Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap()
                };
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0x1549a966 => {
                assert_eq!(header.data, start as u64);
                assert_eq!(data.as_ref(), info.as_slice());
                assert_eq!(data.retained_capacity(), info.len());
                output = Some(data);
                break;
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("complete source leaf: {other:?}"),
        }
    }
    drop(reader);
    (pool, baseline, input_owner.unwrap(), output.unwrap())
}

#[test]
fn profiled_complete_leaf_reuses_its_exact_input_owner() {
    let (pool, baseline, input, output) = collected_leaf(false, 8192);
    assert!(
        output.shares_storage_with(&input),
        "exact whole body owner should transfer without a second allocation/copy"
    );
    // Exact tracked charge, not SharedBytes' conservative hard-policy allowance.
    let charge = input.retained_capacity() + Bytes::owner_overhead_bytes();
    assert_eq!(pool.used(), baseline + charge);
    drop(input);
    assert_eq!(output.as_ref(), uint(0x2ad7b1, 1_000_000).as_slice());
    assert_eq!(pool.used(), baseline + charge);
    drop(output);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn profiled_hidden_leaf_parent_keeps_the_exact_output_capacity_under_all_work_schedules() {
    for quantum in [1, 7, 8192] {
        let (pool, baseline, input, output) = collected_leaf(true, quantum);
        assert!(!output.shares_storage_with(&input));
        let output_charge = output.retained_capacity() + Bytes::owner_overhead_bytes();
        drop(input);
        assert_eq!(pool.used(), baseline + output_charge);
        assert_eq!(output.as_ref(), uint(0x2ad7b1, 1_000_000).as_slice());
        drop(output);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn profiled_complete_leaf_waits_for_work_accounts_its_owner_and_observes_cancel() {
    let header = element(0x1a45dfa3, &element(0x4282, b"matroska"));
    let info = uint(0x2ad7b1, 1_000_000);
    let info_element = element(0x1549a966, &info);
    let segment = element(0x18538067, &info_element);
    let start = header.len() + segment.len() - info.len();
    let raw = [header, segment].concat();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = ElementReader::new(
        &pool,
        SourceId(902),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut owner = None;
    for _ in 0..1000 {
        match reader.step(&mut WorkBudget::new(8192), &cancel).unwrap() {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let end = (at + request.maximum).min(raw.len());
                assert!(at < end);
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                if at == start {
                    owner = Some(SharedBytes::from_core(bytes.clone()));
                }
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
                if at == start {
                    break;
                }
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("header-only progress: {other:?}"),
        }
    }
    let owner = owner.unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(info.len() as u64), &cancel)
            .unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(reader.position(), raw.len() as u64);
    let before = pool.used();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(pool.used(), before);
    // No outside allocation is unowned by this graph: the complete source
    // owner (shared with this alias) must appear in the minimum-pool proof.
    for (requested, permanent) in [(pool.available(), false), (pool.available() + 1, true)] {
        let mut proof = usage::Pressure::new(requested);
        let mut done = false;
        for _ in 0..1000 {
            let mut work = WorkBudget::new(1);
            match proof.step(&reader, &pool, &mut work, &cancel) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(())) => {
                    assert!(!permanent);
                    done = true;
                    break;
                }
                Err(Error::Policy("operation retained storage")) => {
                    assert!(permanent);
                    done = true;
                    break;
                }
                other => panic!("pending leaf owner proof: {other:?}"),
            }
            assert!(work.spent() <= 1);
        }
        assert!(done);
    }
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    drop(reader);
    assert_eq!(owner.as_ref(), info.as_slice());
    assert_eq!(
        pool.used(),
        baseline + owner.retained_capacity() + Bytes::owner_overhead_bytes()
    );
    drop(owner);
    assert_eq!(pool.used(), baseline);
}
