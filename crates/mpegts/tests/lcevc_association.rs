//! Authored PMT relationships; payload framing and decoding are deliberately absent.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_mpegts::psi::{
    DescriptorNamespace, DvbNamespaceEvidence, PmtReader, PsiCrc32, SectionAssembler, TableStep,
};
use triergon_mpegts::{AssemblyStep, ParseLimits, TraceEvent};

const VIDEO: &[u8] = &[0x3f, 5, 0x17, 7, 0x23, 7, 0];
const LINK: &[u8] = &[0x3f, 3, 0x18, 1, 7];

fn event(
    streams: &[(u8, u16, &[u8])],
    program_descriptors: &[u8],
    namespace: DescriptorNamespace,
    epoch: u64,
) -> TraceEvent {
    let pool = Budget::new(1 << 20).unwrap();
    let mut raw = vec![
        2,
        0xb0,
        0,
        0,
        1,
        0xc1,
        0,
        0,
        0xe1,
        0,
        0xf0,
        u8::try_from(program_descriptors.len()).unwrap(),
    ];
    raw.extend_from_slice(program_descriptors);
    for &(kind, pid, descriptors) in streams {
        raw.extend_from_slice(&[
            kind,
            0xe0 | u8::try_from(pid >> 8).unwrap(),
            pid.to_be_bytes()[1],
            0xf0,
            u8::try_from(descriptors.len()).unwrap(),
        ]);
        raw.extend_from_slice(descriptors);
    }
    let length = u16::try_from(raw.len() + 1).unwrap().to_be_bytes();
    raw[1] |= length[0];
    raw[2] = length[1];
    raw.extend_from_slice(&PsiCrc32::of(&raw).to_be_bytes());
    let mut payload = vec![0];
    payload.extend_from_slice(&raw);
    let mut assembler =
        SectionAssembler::new(0x1000, pool.clone(), ParseLimits::default()).unwrap();
    assembler
        .push(
            Bytes::copy_from_slice(&pool, &payload).unwrap(),
            true,
            SourceId(42),
            1234,
        )
        .unwrap();
    let cancel = AtomicBool::new(false);
    let AssemblyStep::Output(section) = assembler
        .poll(&mut WorkBudget::new(u64::MAX), &cancel)
        .unwrap()
    else {
        panic!("section")
    };
    let mut reader =
        PmtReader::new_with_namespace(&section, &pool, ParseLimits::default(), namespace).unwrap();
    loop {
        match reader.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
            TableStep::Output(map) => {
                return TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch: epoch,
                };
            }
            TableStep::Yield => {}
            TableStep::Done => panic!("PMT"),
        }
    }
}

fn error(event: &TraceEvent, pid: u16) -> Error {
    event
        .require_lcevc_association(pid, &mut WorkBudget::new(u64::MAX))
        .unwrap_err()
}

#[test]
fn source_relationship_retains_namespace_coordinates_order_and_optional_codec_lookup() {
    let descriptors = [0x80, 1, 0xaa, 0x3f, 3, 0x18, 1, 7, 0x7f, 1, 0x18];
    for namespace in [
        DescriptorNamespace::Unspecified,
        DescriptorNamespace::Atsc,
        DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
    ] {
        for kind in [0x01, 0x02, 0x1b, 0x24] {
            let source = event(
                &[(kind, 0x101, &descriptors), (0x36, 0x102, VIDEO)],
                &[],
                namespace,
                11,
            );
            let relation = source
                .require_lcevc_association(0x102, &mut WorkBudget::new(u64::MAX))
                .unwrap();
            assert_eq!(
                (
                    relation.base.pid,
                    relation.enhancement.pid,
                    relation.video.lcevc_stream_tag,
                    relation.configuration_epoch
                ),
                (0x101, 0x102, 7, 11)
            );
            assert_eq!(relation.program_map.descriptor_namespace, namespace);
            assert_eq!(relation.base.descriptor_namespace(), namespace);
            assert_eq!(relation.base.descriptor_bytes.as_ref(), descriptors);
            assert_eq!(relation.enhancement.descriptor_bytes.as_ref(), VIDEO);
            assert_eq!(relation.enhancement.codec(), None);
            assert!(relation.base.codec().is_some());
            assert_eq!(relation.program_map.header.extension, 1);
            assert_eq!(relation.program_map.source_ranges.len(), 1);
            let range = &relation.program_map.source_ranges[0];
            assert_eq!(range.entity, SourceId(42));
            assert_eq!(range.offset, 1235);
        }
    }
}

#[test]
fn missing_links_never_fall_back_to_program_level_wrong_namespace_or_stream_order() {
    for (streams, program_descriptors, expected) in [
        (
            vec![(0x1b, 0x101, LINK)],
            &[][..],
            Error::Invalid("missing LCEVC enhancement PID"),
        ),
        (
            vec![(0x1b, 0x101, LINK), (0x36, 0x102, &[])],
            &[],
            Error::Invalid("missing LCEVC video descriptor"),
        ),
        (
            vec![(0x1b, 0x101, &[]), (0x36, 0x102, VIDEO)],
            LINK,
            Error::Invalid("missing LCEVC base linkage"),
        ),
        (
            vec![(0x1b, 0x101, &[0x7f, 3, 0x18, 1, 7]), (0x36, 0x102, VIDEO)],
            &[],
            Error::Invalid("missing LCEVC base linkage"),
        ),
        (
            vec![(0x1b, 0x101, &[0x3f, 3, 0x18, 1, 8]), (0x36, 0x102, VIDEO)],
            &[],
            Error::Invalid("missing LCEVC base linkage"),
        ),
    ] {
        assert_eq!(
            error(
                &event(
                    &streams,
                    program_descriptors,
                    DescriptorNamespace::Unspecified,
                    0
                ),
                0x102
            ),
            expected
        );
    }
}

#[test]
fn duplicates_on_one_base_are_equivalent_but_distinct_matches_are_ambiguous() {
    let repeated = [0x3f, 5, 0x18, 3, 7, 7, 7, 0x3f, 3, 0x18, 1, 7];
    let source = event(
        &[(0x36, 0x102, VIDEO), (0x1b, 0x101, &repeated)],
        &[],
        DescriptorNamespace::Unspecified,
        0,
    );
    let relation = source
        .require_lcevc_association(0x102, &mut WorkBudget::new(u64::MAX))
        .unwrap();
    assert_eq!(relation.base.descriptor_bytes.as_ref(), repeated);
    assert_eq!(
        error(
            &event(
                &[
                    (0x1b, 0x101, LINK),
                    (0x24, 0x103, LINK),
                    (0x36, 0x102, VIDEO)
                ],
                &[],
                DescriptorNamespace::Unspecified,
                0
            ),
            0x102
        ),
        Error::Invalid("ambiguous LCEVC base tag")
    );
    let shared = event(
        &[
            (0x1b, 0x101, LINK),
            (0x36, 0x103, VIDEO),
            (0x36, 0x102, VIDEO),
        ],
        &[],
        DescriptorNamespace::Unspecified,
        0,
    );
    for pid in [0x102, 0x103] {
        assert_eq!(
            shared
                .require_lcevc_association(pid, &mut WorkBudget::new(u64::MAX))
                .unwrap()
                .base
                .pid,
            0x101
        );
    }
}

#[test]
fn malformed_unsupported_and_self_links_have_exact_refusals() {
    let duplicate = [VIDEO, VIDEO].concat();
    let self_link = [VIDEO, LINK].concat();
    for (streams, expected) in [
        (
            vec![(0x36, 0x102, &self_link[..])],
            Error::Invalid("LCEVC self association"),
        ),
        (
            vec![(0x1b, 0x101, LINK), (0x36, 0x102, &duplicate)],
            Error::Unsupported("multiple LCEVC video descriptors"),
        ),
        (
            vec![(0x1b, 0x101, &[0x3f, 2, 0x18, 1]), (0x36, 0x102, VIDEO)],
            Error::Invalid("LCEVC linkage descriptor length"),
        ),
        (
            vec![(0x1b, 0x101, LINK), (0x36, 0x102, &[0x3f, 2, 0x17, 7])],
            Error::Invalid("LCEVC video descriptor length"),
        ),
        (
            vec![(0x03, 0x101, LINK), (0x36, 0x102, VIDEO)],
            Error::Unsupported("LCEVC base stream type"),
        ),
        (
            vec![(0x1b, 0x101, LINK), (0x33, 0x102, VIDEO)],
            Error::Unsupported("LCEVC enhancement stream type"),
        ),
    ] {
        assert_eq!(
            error(
                &event(&streams, &[], DescriptorNamespace::Unspecified, 0),
                0x102
            ),
            expected
        );
    }
}

#[test]
fn replacement_is_local_to_the_exact_pmt_epoch_and_old_relationship_survives() {
    let old = event(
        &[(0x1b, 0x101, LINK), (0x36, 0x102, VIDEO)],
        &[],
        DescriptorNamespace::Unspecified,
        4,
    );
    let relation = old
        .require_lcevc_association(0x102, &mut WorkBudget::new(u64::MAX))
        .unwrap();
    let new = event(
        &[(0x1b, 0x103, LINK), (0x36, 0x102, VIDEO)],
        &[],
        DescriptorNamespace::Atsc,
        5,
    );
    let replacement = new
        .require_lcevc_association(0x102, &mut WorkBudget::new(u64::MAX))
        .unwrap();
    assert_eq!(
        (relation.base.pid, relation.configuration_epoch),
        (0x101, 4)
    );
    assert_eq!(
        (replacement.base.pid, replacement.configuration_epoch),
        (0x103, 5)
    );
    let missing = event(
        &[(0x1b, 0x103, &[]), (0x36, 0x102, VIDEO)],
        &[],
        DescriptorNamespace::Unspecified,
        6,
    );
    assert_eq!(
        error(&missing, 0x102),
        Error::Invalid("missing LCEVC base linkage")
    );
    assert_eq!(relation.base.descriptor_bytes.as_ref(), LINK);
}

#[test]
fn short_work_and_inactive_event_are_explicit_and_retryable() {
    let mut source = event(
        &[(0x1b, 0x101, LINK), (0x36, 0x102, VIDEO)],
        &[],
        DescriptorNamespace::Unspecified,
        0,
    );
    for (available, spent) in [(0, 0), (29, 2)] {
        let mut work = WorkBudget::new(available);
        assert_eq!(
            source
                .require_lcevc_association(0x102, &mut work)
                .unwrap_err(),
            Error::Invalid("LCEVC association work grant")
        );
        assert_eq!(work.spent(), spent);
    }
    let mut work = WorkBudget::new(30);
    assert_eq!(
        source
            .require_lcevc_association(0x102, &mut work)
            .unwrap()
            .base
            .pid,
        0x101
    );
    assert_eq!(work.spent(), 30);
    if let TraceEvent::ProgramMap { active, .. } = &mut source {
        *active = false;
    }
    assert_eq!(
        error(&source, 0x102),
        Error::Invalid("LCEVC association requires active PMT")
    );
    assert_eq!(
        error(&TraceEvent::Finished, 0x102),
        Error::Invalid("LCEVC association requires active PMT")
    );
}

#[test]
fn transport_replacements_publish_new_epochs_without_reusing_old_links() {
    use triergon_core::{InputCursor, InputWindow, SourceCapabilities, Step};
    use triergon_mpegts::{PacketFormat, TransportStream};
    let mut raw = Vec::new();
    let mut packet = |pid: u16, cc: u8, section: &[u8]| {
        let mut carrier = [0xff; 188];
        carrier[..5].copy_from_slice(&[
            0x47,
            0x40 | pid.to_be_bytes()[0],
            pid.to_be_bytes()[1],
            0x10 | cc,
            0,
        ]);
        carrier[5..5 + section.len()].copy_from_slice(section);
        raw.extend_from_slice(&carrier);
    };
    packet(
        0,
        0,
        &[
            0x00, 0xb0, 0x0d, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0, 0x2a, 0xb1, 0x04, 0xb2,
        ],
    );
    for (version, base, links) in [(0_u8, 0x101, LINK), (1, 0x103, LINK), (2, 0x103, &[][..])] {
        let TraceEvent::ProgramMap { map, .. } = event(
            &[(0x1b, base, links), (0x36, 0x102, VIDEO)],
            &[],
            DescriptorNamespace::Unspecified,
            0,
        ) else {
            unreachable!()
        };
        let mut section = map.source.as_ref().to_vec();
        section[5] = 0xc1 | version << 1;
        let end = section.len() - 4;
        let crc = PsiCrc32::of(&section[..end]);
        section[end..].copy_from_slice(&crc.to_be_bytes());
        packet(0x1000, version, &section);
    }
    let pool = Budget::new(1 << 20).unwrap();
    let mut cursor = InputCursor::new(
        &pool,
        SourceId(9),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        raw.len(),
    )
    .unwrap();
    let mut transport =
        TransportStream::new(pool.clone(), PacketFormat::Ts188, ParseLimits::default(), 1).unwrap();
    let mut maps = Vec::new();
    loop {
        match transport
            .poll(
                &mut cursor,
                &mut WorkBudget::new(31),
                &AtomicBool::new(false),
            )
            .unwrap()
        {
            Step::Ready(source @ TraceEvent::ProgramMap { active: true, .. }) => maps.push(source),
            Step::Ready(TraceEvent::Diagnostic(fault)) => panic!("{fault:?}"),
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).unwrap();
                if start == raw.len() {
                    cursor.finish(request.source, request.token).unwrap();
                    continue;
                }
                let end = (start + request.maximum).min(raw.len());
                assert!(start < end);
                let bytes = Bytes::copy_from_slice(&pool, &raw[start..end]).unwrap();
                cursor
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            Step::End(_) => break,
            Step::Backpressure(other) => panic!("{other:?}"),
        }
    }
    assert_eq!(maps.len(), 3);
    let first = maps[0]
        .require_lcevc_association(0x102, &mut WorkBudget::new(u64::MAX))
        .unwrap();
    let second = maps[1]
        .require_lcevc_association(0x102, &mut WorkBudget::new(u64::MAX))
        .unwrap();
    // Installing the PAT route advances to epoch 1; each PMT commit advances it.
    assert_eq!((first.base.pid, first.configuration_epoch), (0x101, 2));
    assert_eq!((second.base.pid, second.configuration_epoch), (0x103, 3));
    assert!(matches!(
        &maps[2],
        TraceEvent::ProgramMap {
            configuration_epoch: 4,
            ..
        }
    ));
    assert_eq!(first.program_map.source_ranges[0].offset, 193);
    assert_eq!(second.program_map.source_ranges[0].offset, 381);
    assert_eq!(
        error(&maps[2], 0x102),
        Error::Invalid("missing LCEVC base linkage")
    );
    assert_eq!(first.base.descriptor_bytes.as_ref(), LINK);
}

#[test]
fn one_base_can_serve_distinct_enhancements_including_zero_tag() {
    let video_zero = [0x3f, 5, 0x17, 0, 0x23, 7, 0];
    let links = [0x3f, 4, 0x18, 2, 0, 7];
    let source = event(
        &[
            (0x36, 0x103, &video_zero),
            (0x1b, 0x101, &links),
            (0x36, 0x102, VIDEO),
        ],
        &[],
        DescriptorNamespace::Unspecified,
        8,
    );
    for (pid, tag) in [(0x102, 7), (0x103, 0)] {
        let relation = source
            .require_lcevc_association(pid, &mut WorkBudget::new(u64::MAX))
            .unwrap();
        assert_eq!(
            (relation.base.pid, relation.video.lcevc_stream_tag),
            (0x101, tag)
        );
    }
}
