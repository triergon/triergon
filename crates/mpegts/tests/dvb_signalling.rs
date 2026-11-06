//! EN 300 468 6.2.41 regressions. D0 fixture bodies remain opaque subtitle input.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::psi::{
    CodecIdentification, Descriptor, DescriptorNamespace, Descriptors, DvbNamespaceEvidence,
    DvbNativeViewProfile, PmtReader, ProgramMap, Section, SectionAssembler, TableStep,
};
use triergon_mpegts::{AssemblyStep, PacketFormat, ParseLimits, TraceEvent, TransportStream};

const DVB: DescriptorNamespace =
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);
const SINGLE: [u8; 10] = [0x59, 8, b'e', b'n', b'g', 0x14, 0x12, 0x34, 0xab, 0xcd];

fn descriptor(raw: &[u8]) -> Descriptor<'_> {
    Descriptors::new(raw).next().unwrap().unwrap()
}

// Polynomial long division, independent of production's incremental PSI CRC.
fn crc(data: &[u8]) -> u32 {
    let mut value = 0xffff_ffff_u64;
    for byte in data {
        value = (value << 8) ^ (u64::from(*byte) << 32);
        for shift in (0..8).rev() {
            if value & (1_u64 << (32 + shift)) != 0 {
                value ^= 0x1_04c1_1db7_u64 << shift;
            }
        }
    }
    value as u32
}

fn section(program: &[u8], stream_type: u8, es: &[u8], pool: &Budget) -> Section {
    let length = 18 + program.len() + es.len();
    let mut data = vec![
        2,
        0xb0 | (length >> 8) as u8,
        length as u8,
        0,
        1,
        0xc1,
        0,
        0,
        0xe1,
        0,
        0xf0 | (program.len() >> 8) as u8,
        program.len() as u8,
    ];
    data.extend_from_slice(program);
    data.extend_from_slice(&[
        stream_type,
        0xe1,
        0,
        0xf0 | (es.len() >> 8) as u8,
        es.len() as u8,
    ]);
    data.extend_from_slice(es);
    data.extend_from_slice(&crc(&data).to_be_bytes());
    let mut payload = vec![0];
    payload.extend_from_slice(&data);
    let mut assembler = SectionAssembler::new(4096, pool.clone(), ParseLimits::default()).unwrap();
    assembler
        .push(
            Bytes::copy_from_slice(pool, &payload).unwrap(),
            true,
            SourceId(73),
            901,
        )
        .unwrap();
    for _ in 0..10_000 {
        match assembler
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            AssemblyStep::Output(value) => return value,
            AssemblyStep::Yield => {}
            other => panic!("section incomplete: {other:?}"),
        }
    }
    panic!("section failed to progress");
}

fn finish(reader: &mut PmtReader) -> Result<ProgramMap, Error> {
    for _ in 0..10_000 {
        match reader.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))? {
            TableStep::Output(map) => return Ok(map),
            TableStep::Yield => {}
            TableStep::Done => panic!("missing PMT"),
        }
    }
    panic!("PMT failed to progress");
}

fn map(program: &[u8], kind: u8, es: &[u8], namespace: DescriptorNamespace) -> ProgramMap {
    let pool = Budget::new(1 << 20).unwrap();
    let source = section(program, kind, es, &pool);
    finish(
        &mut PmtReader::new_with_namespace(&source, &pool, ParseLimits::default(), namespace)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn subtitling_view_borrows_original_tlv_and_keeps_big_endian_page_ids() {
    let raw = SINGLE;
    let view = descriptor(&raw).dvb_subtitling().unwrap();
    assert_eq!(view.source, raw);
    assert_eq!(view.data.as_ptr(), raw[2..].as_ptr());
    assert_eq!(view.service_count(), 1);
    let service = view.services().next().unwrap();
    assert_eq!(service.source.as_ptr(), raw[2..].as_ptr());
    assert_eq!(service.language_code, *b"eng");
    assert_eq!(service.subtitling_type, 0x14);
    assert_eq!(service.composition_page_id, 0x1234);
    assert_eq!(service.ancillary_page_id, 0xabcd);
    assert!(service.has_dvb_subtitling_type());
}

#[test]
fn service_shape_has_a_31_entry_bound_and_no_invented_empty_service() {
    assert_eq!(
        descriptor(&[0x59, 0])
            .dvb_subtitling()
            .unwrap()
            .service_count(),
        0
    );
    for count in [1, 7, 9, 255] {
        let mut raw = vec![0x59, count];
        raw.resize(usize::from(count) + 2, 0);
        assert_eq!(
            descriptor(&raw).dvb_subtitling().unwrap_err(),
            Error::Invalid("DVB subtitling descriptor length")
        );
    }
    let raw = [0_u8; 256];
    assert!(
        Descriptor {
            tag: 0x59,
            source: &raw,
            data: &raw
        }
        .dvb_subtitling()
        .is_err()
    );
    assert!(descriptor(&[0x58, 0]).dvb_subtitling().is_err());
    let mut maximum = vec![0x59, 248];
    for _ in 0..31 {
        maximum.extend_from_slice(&SINGLE[2..]);
    }
    let view = descriptor(&maximum).dvb_subtitling().unwrap();
    assert_eq!(view.service_count(), 31);
    let mut services = view.services();
    assert_eq!(services.len(), 31);
    for _ in 0..31 {
        assert_eq!(services.next().unwrap().composition_page_id, 0x1234);
    }
    assert_eq!(services.len(), 0);
    assert_eq!(services.next(), None);
    assert_eq!(services.next(), None);
}

#[test]
fn services_retain_order_duplicates_raw_language_and_reserved_type() {
    let mut raw = vec![0x59, 24];
    raw.extend_from_slice(&SINGLE[2..]);
    raw.extend_from_slice(&[0xff, 0, b'X', 0x88, 0, 0, 0xff, 0xff]);
    raw.extend_from_slice(&SINGLE[2..]);
    let entries: Vec<_> = descriptor(&raw)
        .dvb_subtitling()
        .unwrap()
        .services()
        .collect();
    assert_eq!(entries[0], entries[2]);
    assert_eq!(entries[1].language_code, [0xff, 0, b'X']);
    assert_eq!(entries[1].subtitling_type, 0x88);
    assert!(!entries[1].has_dvb_subtitling_type());
    assert_eq!(entries[1].composition_page_id, 0);
    assert_eq!(entries[1].ancillary_page_id, 65535);
}

#[test]
fn unknown_namespace_keeps_even_malformed_private_tag_bodies_opaque() {
    let raw = [0x59, 1, 0xaa, 0x6a, 0];
    let map = map(&[], 6, &raw, DescriptorNamespace::Unspecified);
    let stream = &map.streams.as_slice()[0];
    assert_eq!(map.descriptor_namespace, DescriptorNamespace::Unspecified);
    assert_eq!(
        stream.descriptor_namespace(),
        DescriptorNamespace::Unspecified
    );
    assert_eq!(stream.descriptor_bytes.as_ref(), raw);
    assert_eq!(stream.dvb_subtitling_signalling(), None);
    assert_eq!(stream.codec_identification(), CodecIdentification::Unknown);
    assert!(
        stream
            .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
            .is_err()
    );
}

#[test]
fn only_explicit_dvb_private_pes_es_context_identifies_dvb_subtitles() {
    let declared = map(&[], 6, &SINGLE, DVB);
    let stream = &declared.streams.as_slice()[0];
    assert_eq!(stream.descriptor_namespace(), DVB);
    assert_eq!(
        stream.codec_identification(),
        CodecIdentification::DvbSubtitles
    );
    assert_eq!(stream.dvb_subtitling_signalling().unwrap().services, 1);
    assert_eq!(
        map(&[], 0x1b, &SINGLE, DVB).streams.as_slice()[0].codec_identification(),
        CodecIdentification::Unknown
    );
    let program_only = map(&SINGLE, 6, &[], DVB);
    assert_eq!(program_only.program_descriptors.as_ref(), SINGLE);
    assert_eq!(
        program_only.streams.as_slice()[0].codec_identification(),
        CodecIdentification::Unknown
    );
}

#[test]
fn registration_or_private_specifier_does_not_infer_a_dvb_namespace() {
    let mut raw = vec![
        5, 4, b'D', b'V', b'B', b' ', 0x5f, 4, 0, 0, 0, 0x28, 0x80, 2, 9, 8,
    ];
    raw.extend_from_slice(&SINGLE);
    raw.extend_from_slice(&[0x5f, 4, 0, 0, 0, 0x29, 0x80, 1, 7]);
    let unspecified = map(&[], 6, &raw, DescriptorNamespace::Unspecified);
    assert_eq!(
        unspecified.streams.as_slice()[0].codec_identification(),
        CodecIdentification::Unknown
    );
    let scoped = map(&[], 6, &raw, DVB);
    let stream = &scoped.streams.as_slice()[0];
    assert_eq!(stream.descriptor_bytes.as_ref(), raw);
    assert_eq!(
        stream
            .descriptors()
            .map(|d| d.unwrap().tag)
            .collect::<Vec<_>>(),
        [5, 0x5f, 0x80, 0x59, 0x5f, 0x80]
    );
    assert_eq!(
        stream.codec_identification(),
        CodecIdentification::DvbSubtitles
    );
    assert!(
        stream
            .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
            .is_err()
    );
    // Source context TLVs stay in order; no global private-tag interpretation.
}

#[test]
fn named_single_service_native_view_keeps_its_source_and_explicit_pin() {
    let parsed = map(&[], 6, &SINGLE, DVB);
    let stream = &parsed.streams.as_slice()[0];
    let view = stream
        .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
        .unwrap();
    assert_eq!(view.bytes, [0x12, 0x34, 0xab, 0xcd, 0x14]);
    assert_eq!(view.source.language_code, *b"eng");
    assert_eq!(
        view.source.source.as_ptr(),
        stream.descriptor_bytes.as_ref()[2..].as_ptr()
    );
    assert_eq!(
        view.profile.source_pin(),
        "d32b387f2b0a484599d4587d651891f0c63c4238"
    );
    let mut no_ancillary = SINGLE;
    no_ancillary[8..].copy_from_slice(&[0x12, 0x34]);
    let parsed = map(&[], 6, &no_ancillary, DVB);
    assert_eq!(
        parsed.streams.as_slice()[0]
            .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
            .unwrap()
            .bytes,
        [0x12, 0x34, 0x12, 0x34, 0x14]
    );
}

#[test]
fn multi_service_and_repeated_tlvs_never_select_a_first_native_service() {
    let mut multiple = vec![0x59, 16];
    multiple.extend_from_slice(&SINGLE[2..]);
    multiple.extend_from_slice(&SINGLE[2..]);
    let duplicate = [SINGLE.as_slice(), SINGLE.as_slice()].concat();
    for (raw, descriptors, services) in [
        (&multiple[..], 1, 2),
        (&duplicate[..], 2, 2),
        (&[0x59, 0][..], 1, 0),
    ] {
        let parsed = map(&[], 6, raw, DVB);
        let stream = &parsed.streams.as_slice()[0];
        assert_eq!(stream.descriptor_bytes.as_ref(), raw);
        let summary = stream.dvb_subtitling_signalling().unwrap();
        assert_eq!(
            (summary.descriptors, summary.services),
            (descriptors, services)
        );
        assert!(
            stream
                .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
                .is_err()
        );
    }
}

#[test]
fn conflicting_dvb_audio_signals_do_not_silently_win_or_disappear() {
    for tag in [0x6a, 0x7a] {
        let mut raw = SINGLE.to_vec();
        raw.extend_from_slice(&[tag, 1, 0]);
        let parsed = map(&[], 6, &raw, DVB);
        let stream = &parsed.streams.as_slice()[0];
        assert_eq!(
            stream.codec_identification(),
            CodecIdentification::ConflictingDvbSignalling
        );
        assert!(stream.dvb_subtitling_signalling().unwrap().audio_conflict);
        assert_eq!(stream.descriptor_bytes.as_ref(), raw);
        assert!(
            stream
                .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
                .is_err()
        );
    }
}

#[test]
fn scoped_bad_shape_is_rejected_before_a_program_map_but_raw_section_survives() {
    let pool = Budget::new(1 << 20).unwrap();
    for (program, es) in [
        (&[][..], &[0x59, 1, 0][..]),
        (&[0x59, 7, 0, 0, 0, 0, 0, 0, 0][..], &[][..]),
    ] {
        let source = section(program, 6, es, &pool);
        let before = source.bytes().clone();
        let mut reader =
            PmtReader::new_with_namespace(&source, &pool, ParseLimits::default(), DVB).unwrap();
        assert_eq!(
            finish(&mut reader).unwrap_err(),
            Error::Invalid("DVB subtitling descriptor length")
        );
        assert_eq!(source.bytes().as_ref(), before.as_ref());
        assert_eq!(source.crc_remainder(), Some(0));
    }
}

#[test]
fn pmt_dvb_summary_is_work_bounded_cancellable_charged_and_owned() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let source = section(&[], 6, &SINGLE, &pool);
    let mut reader =
        PmtReader::new_with_namespace(&source, &pool, ParseLimits::default(), DVB).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
        TableStep::Yield
    ));
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(99), &cancel),
        Err(Error::Backpressure { .. })
    ));
    drop(pressure);
    let parsed = finish(&mut reader).unwrap();
    drop(reader);
    drop(source);
    assert!(pool.used() > baseline);
    let retained = pool.used();
    let stream = &parsed.streams.as_slice()[0];
    assert_eq!(
        stream
            .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
            .unwrap()
            .bytes,
        [0x12, 0x34, 0xab, 0xcd, 0x14]
    );
    assert_eq!(pool.used(), retained);
    assert_eq!(parsed.source_ranges.as_slice()[0].entity, SourceId(73));
    drop(parsed);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn duplicate_descriptors_are_counted_against_the_same_source_limit() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = [SINGLE.as_slice(), SINGLE.as_slice()].concat();
    let source = section(&[], 6, &raw, &pool);
    let limits = ParseLimits {
        descriptors: 1,
        ..ParseLimits::default()
    };
    let mut reader = PmtReader::new_with_namespace(&source, &pool, limits, DVB).unwrap();
    assert_eq!(
        finish(&mut reader).unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
}

fn collect(source: &[u8]) -> Vec<TraceEvent> {
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(31),
        SourceCapabilities {
            length: Some(source.len() as u64),
            random_access: false,
        },
        4096,
    )
    .unwrap();
    let mut demux = TransportStream::new_with_namespace(
        pool.clone(),
        PacketFormat::Ts188,
        ParseLimits::default(),
        1,
        DVB,
    )
    .unwrap();
    let mut events = Vec::new();
    for _ in 0..100_000 {
        match demux
            .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(TraceEvent::Finished) => return events,
            Step::Ready(value) => events.push(value),
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = request.offset as usize;
                if start == source.len() {
                    input.finish(request.source, request.token).unwrap();
                } else {
                    let end = source.len().min(start + request.maximum.min(7));
                    let bytes = Bytes::copy_from_slice(&pool, &source[start..end]).unwrap();
                    input
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => demux.end_of_source().unwrap(),
        }
    }
    panic!("DVB source failed to progress");
}

#[test]
fn six_dvb_carriers_keep_pes_envelopes_raw33_absent_dts_and_service_epochs() {
    let cases: &[(&[u8], &[u8], u8)] = &[
        (
            include_bytes!("data/dvb/dvb2-literals-fields.ts"),
            include_bytes!("data/dvb/dvb2-literals-fields.pes-data"),
            0x14,
        ),
        (
            include_bytes!("data/dvb/dvb-ancillary-pages.ts"),
            include_bytes!("data/dvb/dvb-ancillary-pages.pes-data"),
            0x14,
        ),
        (
            include_bytes!("data/dvb/dvb-default-clut2.ts"),
            include_bytes!("data/dvb/dvb-default-clut2.pes-data"),
            0x10,
        ),
        (
            include_bytes!("data/dvb/dvb-nonmodifying.ts"),
            include_bytes!("data/dvb/dvb-nonmodifying.pes-data"),
            0x14,
        ),
        (
            include_bytes!("data/dvb/dvb-partial-alpha-projection.ts"),
            include_bytes!("data/dvb/dvb-partial-alpha-projection.pes-data"),
            0x14,
        ),
        (
            include_bytes!("data/dvb/dvb-truncated-segment.ts"),
            include_bytes!("data/dvb/dvb-truncated-segment.pes-data"),
            0x14,
        ),
    ];
    for (source, original_pes, kind) in cases {
        let events = collect(source);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, TraceEvent::Diagnostic(_)))
        );
        let maps: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::ProgramMap {
                    map,
                    active: true,
                    configuration_epoch,
                } => Some((map, *configuration_epoch)),
                _ => None,
            })
            .collect();
        assert_eq!(maps.len(), 1);
        let stream = &maps[0].0.streams.as_slice()[0];
        assert_eq!(
            stream.codec_identification(),
            CodecIdentification::DvbSubtitles
        );
        let view = stream
            .native_dvb_single_service_view(DvbNativeViewProfile::D0SingleServiceV1)
            .unwrap();
        assert_eq!(view.bytes, [0, 1, 0, 2, *kind]);
        assert_eq!(view.source.language_code, *b"eng");
        let packets: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Pes(pes) => Some(pes),
                _ => None,
            })
            .collect();
        assert_eq!(packets.len(), 1);
        let pes = packets[0];
        assert_eq!(pes.header.stream_id, 0xbd);
        assert!(pes.header.data_alignment_indicator);
        assert_eq!(pes.header.pts_raw33, Some(90000));
        assert_eq!(pes.header.dts_raw33, None);
        assert_eq!(pes.epochs.configuration, maps[0].1);
        assert_eq!(pes.epochs.clock, 0);
        assert_eq!(
            pes.payload().flatten().copied().collect::<Vec<_>>(),
            *original_pes
        );
        assert_eq!(&original_pes[..2], &[0x20, 0]);
        assert_eq!(original_pes.last(), Some(&0xff));
        assert_eq!(
            pes.source_ranges().map(|r| r.bytes).sum::<usize>(),
            original_pes.len()
        );
        assert!(pes.source_ranges().all(|r| r.entity == SourceId(31)));
        for span in pes.spans() {
            let range = span.source_range();
            assert_eq!(
                span.as_ref(),
                &source[range.offset as usize..range.offset as usize + range.bytes]
            );
        }
        // Even malformed inner segments are opaque here: the subtitle decoder
        // owns their diagnosis.
    }
}
