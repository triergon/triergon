//! Chapter documents: UID namespaces, display and flag cardinality, nested ranges,
//! late ordering flags, and work that grows linearly with the chapter count.
use crate::ebml_kit::{element, uint};
use crate::metadata::{
    CHAPTERS, SharedDocument,
    build::{DocumentJob, DocumentLimits},
};
use crate::{Error, Result, step::ParseStep, storage::SharedBytes};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};

fn parse(raw: &[u8], quantum: u64) -> Result<(SharedDocument, u64)> {
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
        Some(71),
        &pool,
    )?;
    let cancel = AtomicBool::new(false);
    let mut work = WorkBudget::new(0);
    assert!(matches!(job.step(&mut work, &cancel)?, ParseStep::Yield));
    assert_eq!(work.spent(), 0);
    let mut spent = 0;
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(quantum);
        let step = job.step(&mut work, &cancel)?;
        assert!(work.spent() <= quantum);
        spent += work.spent();
        match step {
            ParseStep::Yield => (),
            ParseStep::Event(document) => return Ok((document, spent)),
            other => panic!("bounded authored chapter source: {other:?}"),
        }
    }
    panic!("bounded chapter validation did not finish");
}

#[test]
fn one_uid_namespace_does_not_force_extra_full_walks_of_unrelated_opaque_nodes() {
    let atom = element(0xb6, &[uint(0x73c4, 1), uint(0x91, 0)].concat());
    let mut raw = Vec::new();
    for _ in 0..2000 {
        raw.extend(element(0xec, &[]));
    }
    raw.extend(element(0x45b9, &atom));
    let (document, spent) = parse(&raw, 7).unwrap();
    assert_eq!(document.raw(), raw.as_slice());
    assert_eq!(document.source_offset(), Some(71));
    assert_eq!(document.node_count(), 2005);
    println!(
        "one UID namespace: nodes={} work={spent}",
        document.node_count()
    );
    // This includes syntax, amortized charged arena copies, scalar checks and
    // complete validation. Unrelated Void nodes cannot cause absent UID kinds
    // to walk the full arena again. No time or wall-clock threshold is used.
    let ceiling = document.node_count() as u64 * 11 + 256;
    assert!(
        spent <= ceiling,
        "redundant UID walks: spent={spent} ceiling={ceiling}"
    );
}

#[test]
fn uid_namespaces_keep_full_u64_values_and_reject_only_same_kind_duplicates() {
    let atom = |uid| {
        element(
            0xb6,
            &[uint(0x73c4, uid), uint(0x91, 1), uint(0x92, u64::MAX)].concat(),
        )
    };
    for quantum in [1, 7, 8192] {
        // Equal EditionUID/ChapterUID values are distinct namespaces. Values
        // must not be truncated to make a tag fit in the scalar workspace.
        let raw = element(0x45b9, &[uint(0x45bc, u64::MAX), atom(u64::MAX)].concat());
        let (document, _) = parse(&raw, quantum).unwrap();
        let edition = document.editions().next().unwrap();
        assert_eq!(edition.uid().unwrap(), Some(u64::MAX));
        let chapter = edition.chapters().next().unwrap();
        assert_eq!(chapter.uid().unwrap(), u64::MAX);
        assert_eq!(chapter.end_ns().unwrap(), Some(u64::MAX));
        let duplicate_chapter = element(0x45b9, &[atom(u64::MAX), atom(u64::MAX)].concat());
        assert!(matches!(
            parse(&duplicate_chapter, quantum),
            Err(Error::Invalid("duplicate metadata UID"))
        ));
        let duplicate_edition = [
            element(0x45b9, &[uint(0x45bc, 9), atom(1)].concat()),
            element(0x45b9, &[uint(0x45bc, 9), atom(2)].concat()),
        ]
        .concat();
        assert!(matches!(
            parse(&duplicate_edition, quantum),
            Err(Error::Invalid("duplicate metadata UID"))
        ));
    }
}

#[test]
fn chapter_displays_repeat_and_the_enabled_flag_is_unique() {
    let mut fields = [
        uint(0x73c4, 1),
        uint(0x91, 0),
        uint(0x4598, 1),
        element(0x80, &element(0x85, b"First title")),
        element(0x80, &element(0x85, b"Second title")),
    ]
    .concat();
    let valid = element(0x45b9, &element(0xb6, &fields));
    fields.extend(uint(0x4598, 0));
    let duplicate = element(0x45b9, &element(0xb6, &fields));

    for quantum in [1, 7, 8192] {
        let (document, _) = parse(&valid, quantum).unwrap();
        let chapter = document
            .editions()
            .next()
            .unwrap()
            .chapters()
            .next()
            .unwrap();
        assert!(chapter.enabled().unwrap());
        assert!(
            chapter
                .displays()
                .map(|display| display.title().unwrap())
                .eq(["First title", "Second title"])
        );
        assert!(matches!(
            parse(&duplicate, quantum),
            Err(Error::Invalid("duplicate metadata field"))
        ));
    }
}

#[test]
fn typed_chapter_ranges_keep_nested_unknown_payloads_and_final_empty_children_exact() {
    let opaque = element(0x4abc, &[0x73, 0xc4, 0x81, 1, 0xb6, 0x80]);
    let display = element(0x80, &element(0x85, &[]));
    let inner = element(
        0xb6,
        &[
            uint(0x73c4, 2),
            uint(0x91, 2),
            uint(0x92, 3),
            opaque.clone(),
            display,
        ]
        .concat(),
    );
    let parent = element(
        0xb6,
        &[uint(0x73c4, 1), uint(0x91, 1), uint(0x92, 4), inner].concat(),
    );
    let raw = element(0x45b9, &[uint(0x45dd, 1), parent].concat());
    for quantum in [1, 7, 8192] {
        let (document, _) = parse(&raw, quantum).unwrap();
        assert_eq!(document.raw(), raw.as_slice());
        let parent = document
            .editions()
            .next()
            .unwrap()
            .chapters()
            .next()
            .unwrap();
        let child = parent.children().next().unwrap();
        assert_eq!(
            (parent.start_ns().unwrap(), parent.end_ns().unwrap()),
            (1, Some(4))
        );
        assert_eq!(
            (child.start_ns().unwrap(), child.end_ns().unwrap()),
            (2, Some(3))
        );
        assert_eq!(child.displays().next().unwrap().title().unwrap(), "");
        let unknown = child
            .0
            .children()
            .find(|element| element.id() == 0x4abc)
            .unwrap();
        assert_eq!(unknown.raw(), &[0x73, 0xc4, 0x81, 1, 0xb6, 0x80]);
        assert!(unknown.children().next().is_none());
        let alias = document.clone();
        drop(document);
        assert_eq!(alias.raw(), raw.as_slice());
    }
}

fn chapter(uid: u64, start: u64, end: Option<u64>, nested: &[u8]) -> Vec<u8> {
    let mut payload = [uint(0x73c4, uid), uint(0x91, start)].concat();
    if let Some(end) = end {
        payload.extend(uint(0x92, end));
    }
    payload.extend(nested);
    element(0xb6, &payload)
}

fn parse_chapters(raw: &[u8], quantum: u64) -> Result<(SharedDocument, u64)> {
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
fn flat_chapter_work_does_not_rescan_all_siblings_for_each_atom() {
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
fn chapter_context_keeps_late_ordering_flags_and_sibling_editions_separate() {
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
