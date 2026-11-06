use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_framing::{
    AnnexBConfigParser, AnnexBWriter, BuildStep, Codec, ConfigEncoding, ConfigInspection,
    ConfigLimits, Configuration, Framing, LengthSize, NalFraming, NalHeader, NalReader, NalUnit,
    ParameterSetKind, ReaderLimits,
};
use triergon_harness_protocol::unhex;

fn limits() -> ReaderLimits {
    ReaderLimits {
        maximum_nal_bytes: 256,
        maximum_padding_bytes: 16,
        maximum_nal_units: 32,
        maximum_input_window: 32,
    }
}

fn config_limits() -> ConfigLimits {
    ConfigLimits {
        maximum_source_bytes: 4096,
        maximum_parameter_sets: 8,
        reader: limits(),
    }
}

fn reader(pool: &Budget, codec: Codec, format: Framing, length: usize) -> NalReader {
    NalReader::new(
        pool,
        codec,
        format,
        SourceId(21),
        SourceCapabilities {
            length: Some(u64::try_from(length).unwrap()),
            random_access: false,
        },
        limits(),
    )
    .unwrap()
}

fn drive(
    pool: &Budget,
    codec: Codec,
    format: Framing,
    data: &[u8],
    chunk: usize,
    units: u64,
) -> Result<Vec<NalUnit>, Error> {
    let source = Bytes::copy_from_slice(pool, data)?;
    let mut parser = reader(pool, codec, format, data.len());
    let cancel = AtomicBool::new(false);
    let mut output = Vec::new();
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(units);
        match parser.step(&mut work, &cancel)? {
            Step::Ready(nal) => output.push(nal),
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).unwrap();
                if start == data.len() {
                    parser.finish(request.source, request.token)?;
                } else {
                    let end = (start + chunk.min(request.maximum)).min(data.len());
                    let window = InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        source.slice(start..end)?,
                    )?;
                    parser.provide(&window)?;
                }
            }
            Step::Yield => {}
            Step::End(location) => {
                assert_eq!(location.offset, data.len() as u64);
                return Ok(output);
            }
        }
        assert!(work.spent() <= units);
    }
    panic!("framing made no bounded progress");
}

#[test]
fn authored_corpus_all_chunk_boundaries_and_small_work() {
    // Each expected payload is bit-authored, not obtained from the Rust reader.
    // Columns: id, codec, framing, width, wire hex, comma-separated payload hex.
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/framing-v1.tsv").lines().skip(1) {
        let cells: Vec<_> = line.split('\t').collect();
        let codec = if cells[1] == "avc" {
            Codec::Avc
        } else {
            Codec::Hevc
        };
        let framing = if cells[2] == "annexb" {
            Framing::AnnexB
        } else {
            Framing::LengthPrefixed(LengthSize::new(cells[3].parse().unwrap()).unwrap())
        };
        let data = unhex(cells[4]);
        let expected: Vec<_> = if cells[5].is_empty() {
            Vec::new()
        } else {
            cells[5].split(',').map(unhex).collect()
        };
        for chunk in 1..=data.len().max(1) {
            for work in [1, 2, 5, 64] {
                let actual = drive(&pool, codec, framing, &data, chunk, work).unwrap();
                assert_eq!(
                    actual
                        .iter()
                        .map(|nal| nal.bytes().as_ref().to_vec())
                        .collect::<Vec<_>>(),
                    expected,
                    "{} chunk={chunk} work={work}",
                    cells[0]
                );
                for (index, nal) in actual.into_iter().enumerate() {
                    assert!(nal.bytes().belongs_to(&pool));
                    let facts = nal.facts();
                    let number = |column: usize| {
                        cells[column]
                            .split(',')
                            .nth(index)
                            .unwrap()
                            .parse::<usize>()
                            .unwrap()
                    };
                    assert_eq!(facts.payload_offset, number(6) as u64);
                    assert_eq!(facts.payload_length, expected[index].len());
                    assert_eq!(facts.source, SourceId(21));
                    assert_eq!(facts.trailing_zero_bytes, number(10));
                    let h: Vec<_> = cells[11]
                        .split(',')
                        .nth(index)
                        .unwrap()
                        .split('/')
                        .collect();
                    assert_eq!(
                        facts.header,
                        match codec {
                            Codec::Avc => NalHeader::Avc {
                                nal_unit_type: h[0].parse().unwrap(),
                                nal_ref_idc: h[1].parse().unwrap()
                            },
                            Codec::Hevc => NalHeader::Hevc {
                                nal_unit_type: h[0].parse().unwrap(),
                                layer_id: h[2].parse().unwrap(),
                                temporal_id_plus_one: h[3].parse().unwrap()
                            },
                        }
                    );
                    match facts.framing {
                        NalFraming::AnnexB {
                            prefix_offset,
                            start_code_bytes,
                            leading_zero_bytes,
                        } => {
                            assert_eq!(prefix_offset, number(7) as u64);
                            assert_eq!(start_code_bytes as usize, number(8));
                            assert_eq!(leading_zero_bytes, number(9));
                        }
                        NalFraming::LengthPrefixed {
                            prefix_offset,
                            length_size,
                            declared_length,
                        } => {
                            assert_eq!(prefix_offset, number(7) as u64);
                            assert_eq!(length_size.bytes() as usize, number(8));
                            assert_eq!(declared_length as usize, expected[index].len());
                        }
                    }
                    let original = nal.bytes().as_ref().to_vec();
                    let facts = nal.facts();
                    let mut adapter = AnnexBWriter::new(&pool, nal, 260).unwrap();
                    let mut seen = false;
                    for _ in 0..10_000 {
                        match adapter
                            .step(&mut WorkBudget::new(work), &AtomicBool::new(false))
                            .unwrap()
                        {
                            BuildStep::Yield => {}
                            BuildStep::Ready(result) => {
                                assert_eq!(result.source_facts(), facts);
                                assert_eq!(&result.bytes()[..4], &[0, 0, 0, 1]);
                                assert_eq!(&result.bytes()[4..], original.as_slice());
                                assert!(result.bytes().belongs_to(&pool));
                                seen = true;
                                break;
                            }
                            BuildStep::Complete => panic!("adapter omitted output"),
                        }
                    }
                    assert!(seen, "adapter made no bounded progress");
                }
            }
        }
    }
}

#[test]
fn prefix_and_padding_facts_retain_exact_offsets() {
    let pool = Budget::new(8192).unwrap();
    let nals = drive(
        &pool,
        Codec::Avc,
        Framing::AnnexB,
        &[
            0, 0, 0, 0, 1, 0x67, 0x80, 0, 0, 0, 0, 0, 1, 0x68, 0x80, 0, 0, 0,
        ],
        1,
        1,
    )
    .unwrap();
    assert_eq!(nals.len(), 2);
    assert_eq!(nals[0].facts().payload_offset, 5);
    assert_eq!(nals[0].facts().trailing_zero_bytes, 2);
    assert_eq!(
        nals[0].facts().framing,
        NalFraming::AnnexB {
            prefix_offset: 1,
            start_code_bytes: 4,
            leading_zero_bytes: 1,
        }
    );
    assert_eq!(nals[1].facts().payload_offset, 13);
    assert_eq!(nals[1].facts().trailing_zero_bytes, 3);
    assert_eq!(
        nals[1].facts().framing,
        NalFraming::AnnexB {
            prefix_offset: 9,
            start_code_bytes: 4,
            leading_zero_bytes: 0,
        }
    );
    let lp = drive(
        &pool,
        Codec::Avc,
        Framing::LengthPrefixed(LengthSize::new(3).unwrap()),
        &[0, 0, 2, 0x65, 0x80],
        1,
        1,
    )
    .unwrap();
    assert_eq!(
        lp[0].facts().framing,
        NalFraming::LengthPrefixed {
            prefix_offset: 0,
            length_size: LengthSize::new(3).unwrap(),
            declared_length: 2,
        }
    );
    assert_eq!(lp[0].facts().payload_offset, 3);
}

#[test]
fn malformed_and_truncated_inputs_are_not_empty_success() {
    let pool = Budget::new(8192).unwrap();
    let lp = Framing::LengthPrefixed(LengthSize::new(2).unwrap());
    for data in [&[0][..], &[0, 2, 0x65][..]] {
        assert_eq!(
            drive(&pool, Codec::Avc, lp, data, 1, 1).unwrap_err(),
            Error::Truncated
        );
    }
    assert!(matches!(
        drive(&pool, Codec::Avc, lp, &[0, 0], 1, 1),
        Err(Error::Invalid(_))
    ));
    for data in [
        &[7, 0, 0, 1, 0x65][..],
        &[0, 0, 1, 0, 0, 1, 0x65][..],
        &[0, 0, 1, 0x65, 0, 0, 0, 3][..],
        &[0, 0, 1, 0xe5][..],
    ] {
        assert!(matches!(
            drive(&pool, Codec::Avc, Framing::AnnexB, data, 1, 1),
            Err(Error::Invalid(_))
        ));
    }
    assert_eq!(
        drive(&pool, Codec::Avc, Framing::AnnexB, &[0, 0, 1], 1, 1).unwrap_err(),
        Error::Truncated
    );
    assert_eq!(
        drive(&pool, Codec::Hevc, lp, &[0, 1, 0x40], 1, 1).unwrap_err(),
        Error::Truncated
    );
    assert!(matches!(
        drive(&pool, Codec::Hevc, lp, &[0, 2, 0x40, 0], 1, 1),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        NalHeader::parse(Codec::Hevc, &[0x43, 0xff]).unwrap(),
        NalHeader::Hevc {
            nal_unit_type: 33,
            layer_id: 63,
            temporal_id_plus_one: 7,
        }
    );
    assert!(LengthSize::new(0).is_err());
    assert!(LengthSize::new(5).is_err());
}

#[test]
fn input_starvation_generation_pool_and_cancel_are_explicit() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    let lp = Framing::LengthPrefixed(LengthSize::new(1).unwrap());
    let mut parser = reader(&pool, Codec::Avc, lp, 3);
    let request = parser.request();
    let stale_parser = reader(&pool, Codec::Avc, lp, 3);
    let bytes = Bytes::copy_from_slice(&pool, &[2, 0x65, 0x80]).unwrap();
    let foreign_bytes = Bytes::copy_from_slice(&foreign, &[2, 0x65, 0x80]).unwrap();
    assert_eq!(
        parser.provide(
            &InputWindow::new(request.source, request.token.clone(), 0, foreign_bytes).unwrap()
        ),
        Err(Error::ForeignPool)
    );
    assert!(matches!(
        parser.provide(
            &InputWindow::new(request.source, stale_parser.token(), 0, bytes.clone()).unwrap()
        ),
        Err(Error::StaleGeneration { .. })
    ));
    let empty = InputWindow::new(
        request.source,
        request.token.clone(),
        0,
        bytes.slice(0..0).unwrap(),
    )
    .unwrap();
    parser.provide(&empty).unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::NeedInput(_)
    ));
    assert_eq!(parser.position(), 0);
    parser
        .provide(&InputWindow::new(request.source, request.token.clone(), 0, bytes).unwrap())
        .unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(parser.position(), 0);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        parser.step(&mut WorkBudget::new(10), &cancel).unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        parser.step(&mut WorkBudget::new(10), &cancel).unwrap_err(),
        Error::Cancelled
    );

    let mut failed = reader(&pool, Codec::Avc, lp, 3);
    let request = failed.request();
    failed
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        failed.step(&mut WorkBudget::new(10), &cancel).unwrap_err(),
        Error::Transport
    );
    let mut short = reader(&pool, Codec::Avc, lp, 3);
    let request = short.request();
    assert_eq!(
        short.finish(request.source, request.token),
        Err(Error::Truncated)
    );
    assert_eq!(
        short.step(&mut WorkBudget::new(10), &cancel).unwrap_err(),
        Error::Truncated
    );
}

#[test]
fn reserve_and_freeze_backpressure_do_not_consume_or_reparse_payload() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let source = Bytes::copy_from_slice(&pool, &[0, 2, 0x65, 0x80]).unwrap();
    let mut parser = reader(
        &pool,
        Codec::Avc,
        Framing::LengthPrefixed(LengthSize::new(2).unwrap()),
        4,
    );
    let request = parser.request();
    parser
        .provide(&InputWindow::new(request.source, request.token, 0, source.clone()).unwrap())
        .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(2), &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!(parser.position(), 2);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(parser.position(), 2);
    drop(blocker);
    // Reserve once and copy both payload bytes. Freeze has its own work unit.
    assert!(matches!(
        parser.step(&mut WorkBudget::new(3), &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!(parser.position(), 4);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    let retained = pool.used();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!(pool.used(), retained);
    assert_eq!(parser.position(), 4);
    drop(blocker);
    let Step::Ready(nal) = parser.step(&mut WorkBudget::new(1), &cancel).unwrap() else {
        panic!("freeze retry lost completed NAL")
    };
    assert_eq!(nal.bytes().as_ref(), &[0x65, 0x80]);
    let held = nal.clone();
    drop(parser);
    drop(source);
    drop(nal);
    assert!(pool.used() > baseline);
    assert_eq!(held.bytes().as_ref(), &[0x65, 0x80]);
    std::thread::spawn(move || drop(held)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}

#[test]
fn adapter_retries_completed_freeze_and_cancellation_is_sticky() {
    let pool = Budget::new(8192).unwrap();
    let nal = drive(
        &pool,
        Codec::Hevc,
        Framing::LengthPrefixed(LengthSize::new(1).unwrap()),
        &[4, 0x26, 1, 0, 0],
        1,
        1,
    )
    .unwrap()
    .remove(0);
    let mut writer = AnnexBWriter::new(&pool, nal.clone(), 8).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        writer.step(&mut WorkBudget::new(9), &cancel).unwrap(),
        BuildStep::Yield
    ));
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        writer.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    let BuildStep::Ready(output) = writer.step(&mut WorkBudget::new(1), &cancel).unwrap() else {
        panic!("adapter freeze retry lost output")
    };
    assert_eq!(output.bytes().as_ref(), &[0, 0, 0, 1, 0x26, 1, 0, 0]);
    assert!(matches!(
        writer.step(&mut WorkBudget::new(0), &cancel).unwrap(),
        BuildStep::Complete
    ));
    assert!(AnnexBWriter::new(&pool, nal.clone(), 7).is_err());
    assert!(AnnexBWriter::new(&Budget::new(8192).unwrap(), nal.clone(), 8).is_err());
    let mut cancelled = AnnexBWriter::new(&pool, nal, 8).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        cancelled
            .step(&mut WorkBudget::new(100), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        cancelled
            .step(&mut WorkBudget::new(100), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
}

#[test]
fn declared_records_preserve_unknown_source_without_claiming_to_parse_it() {
    let pool = Budget::new(8192).unwrap();
    for (codec, encoding) in [
        (Codec::Avc, ConfigEncoding::AvcDecoderConfigurationRecord),
        (Codec::Hevc, ConfigEncoding::HevcDecoderConfigurationRecord),
    ] {
        // Deliberately not a valid avcC/hvcC fixture. The opaque API must not
        // silently turn caller bytes into a parsed/configuration-valid claim.
        let source = Bytes::copy_from_slice(&pool, &[0xa5, 0x5a, 0, 0xff]).unwrap();
        let declared = Framing::LengthPrefixed(LengthSize::new(4).unwrap());
        let config = Configuration::declared_record(
            &pool,
            codec,
            encoding,
            source.clone(),
            declared,
            config_limits(),
        )
        .unwrap();
        assert_eq!(config.codec(), codec);
        assert_eq!(config.encoding(), encoding);
        assert_eq!(config.inspection(), ConfigInspection::DeclaredOpaque);
        assert!(config.source_bytes().shares_storage_with(&source));
        assert_eq!(config.source_bytes().as_ref(), &[0xa5, 0x5a, 0, 0xff]);
        assert_eq!(config.sample_framing(), declared);
        assert!(config.parameter_sets().is_empty());
    }
    let source = Bytes::copy_from_slice(&pool, &[1]).unwrap();
    assert!(
        Configuration::declared_record(
            &pool,
            Codec::Hevc,
            ConfigEncoding::AvcDecoderConfigurationRecord,
            source,
            Framing::AnnexB,
            config_limits()
        )
        .is_err()
    );
}

fn parse_config(
    pool: &Budget,
    codec: Codec,
    source: Bytes,
    policy: ConfigLimits,
) -> Result<Configuration, Error> {
    let mut parser = AnnexBConfigParser::new(
        pool,
        codec,
        SourceId(31),
        source,
        Framing::LengthPrefixed(LengthSize::new(4).unwrap()),
        policy,
    )?;
    for _ in 0..10_000 {
        match parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false))? {
            BuildStep::Yield => {}
            BuildStep::Ready(config) => return Ok(config),
            BuildStep::Complete => panic!("configuration parser lost output"),
        }
    }
    panic!("configuration parser made no bounded progress");
}

#[test]
fn annex_b_config_keeps_duplicates_order_source_and_classification() {
    let pool = Budget::new(16384).unwrap();
    for (codec, raw, kinds) in [
        (
            Codec::Avc,
            &b"\x00\x00\x00\x01\x67\x80\x00\x00\x01\x68\x80\x00\x00\x01\x67\x80"[..],
            vec![
                ParameterSetKind::AvcSps,
                ParameterSetKind::AvcPps,
                ParameterSetKind::AvcSps,
            ],
        ),
        (
            Codec::Hevc,
            &b"\x00\x00\x00\x01\x40\x01\x80\x00\x00\x01\x42\x01\x80\x00\x00\x01\x44\x01\x80"[..],
            vec![
                ParameterSetKind::HevcVps,
                ParameterSetKind::HevcSps,
                ParameterSetKind::HevcPps,
            ],
        ),
    ] {
        let source = Bytes::copy_from_slice(&pool, raw).unwrap();
        let config = parse_config(&pool, codec, source.clone(), config_limits()).unwrap();
        assert_eq!(config.inspection(), ConfigInspection::AnnexBFramed);
        assert_eq!(config.encoding(), ConfigEncoding::AnnexBParameterSets);
        assert!(config.source_bytes().shares_storage_with(&source));
        assert_eq!(config.source_bytes().as_ref(), raw);
        assert_eq!(
            config
                .parameter_sets()
                .iter()
                .map(triergon_framing::ParameterSet::kind)
                .collect::<Vec<_>>(),
            kinds
        );
        assert!(
            config
                .parameter_sets()
                .iter()
                .all(|p| p.unit().bytes().belongs_to(&pool))
        );
        assert!(
            config
                .parameter_sets()
                .iter()
                .all(|p| p.unit().bytes().shares_storage_with(&source))
        );
        let held = config.parameter_sets()[0].clone();
        drop(config);
        drop(source);
        assert_eq!(held.unit().facts().header.codec(), codec);
    }
    let source = Bytes::copy_from_slice(&pool, &[0, 0, 1, 0x65, 0x80]).unwrap();
    assert!(matches!(
        parse_config(&pool, Codec::Avc, source, config_limits()),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn size_padding_count_and_configuration_limits_are_hard() {
    let pool = Budget::new(16384).unwrap();
    let lp = Framing::LengthPrefixed(LengthSize::new(4).unwrap());
    assert_eq!(
        drive(&pool, Codec::Avc, lp, &[0xff, 0xff, 0xff, 0xff], 1, 1).unwrap_err(),
        Error::Limit {
            requested: u32::MAX as usize,
            maximum: 256
        }
    );
    let mut padded = vec![0, 0, 1, 0x65, 0x80];
    padded.extend_from_slice(&[0; 17]);
    assert!(matches!(
        drive(&pool, Codec::Avc, Framing::AnnexB, &padded, 1, 1),
        Err(Error::Limit { .. })
    ));
    let data = Bytes::copy_from_slice(&pool, &[0, 0, 1, 0x67, 0x80, 0, 0, 1, 0x68, 0x80]).unwrap();
    let mut policy = config_limits();
    policy.maximum_parameter_sets = 1;
    assert!(matches!(
        parse_config(&pool, Codec::Avc, data.clone(), policy),
        Err(Error::Limit { .. })
    ));
    policy = config_limits();
    policy.maximum_source_bytes = data.len() - 1;
    assert!(
        AnnexBConfigParser::new(
            &pool,
            Codec::Avc,
            SourceId(21),
            data,
            Framing::AnnexB,
            policy
        )
        .is_err()
    );
    let mut policy = limits();
    policy.maximum_padding_bytes = usize::MAX;
    assert!(
        NalReader::new(
            &pool,
            Codec::Avc,
            lp,
            SourceId(21),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            policy
        )
        .is_err()
    );
}

#[test]
fn length_field_byte_boundaries_and_annex_payload_limit() {
    let pool = Budget::new(16384).unwrap();
    for length in [1_usize, 2, 255, 256] {
        let width = if length <= 255 { 1 } else { 2 };
        let mut payload = vec![0x80; length];
        payload[0] = 0x65;
        let mut wire = if width == 1 {
            vec![length as u8]
        } else {
            (length as u16).to_be_bytes().to_vec()
        };
        wire.extend_from_slice(&payload);
        for chunk in [1, 7, 32] {
            for units in [1, 64] {
                let nals = drive(
                    &pool,
                    Codec::Avc,
                    Framing::LengthPrefixed(LengthSize::new(width).unwrap()),
                    &wire,
                    chunk,
                    units,
                )
                .unwrap();
                assert_eq!(nals.len(), 1);
                assert_eq!(nals[0].bytes().as_ref(), payload.as_slice());
            }
        }
    }
    let mut wire = vec![0, 0, 1, 0x65];
    wire.extend_from_slice(&[0x80; 256]);
    assert_eq!(
        drive(&pool, Codec::Avc, Framing::AnnexB, &wire, 1, 1).unwrap_err(),
        Error::Limit {
            requested: 257,
            maximum: 256
        }
    );
}

#[test]
fn unknown_length_completion_and_failure_never_publish_partial_nals() {
    let pool = Budget::new(8192).unwrap();
    for fail in [false, true] {
        let mut parser = NalReader::new(
            &pool,
            Codec::Hevc,
            Framing::AnnexB,
            SourceId(55),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            limits(),
        )
        .unwrap();
        let request = parser.request();
        let source = Bytes::copy_from_slice(&pool, &[0, 0, 0, 1, 0x42, 1, 0x80]).unwrap();
        parser
            .provide(&InputWindow::new(request.source, request.token, 0, source).unwrap())
            .unwrap();
        assert!(matches!(
            parser
                .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                .unwrap(),
            Step::NeedInput(_)
        ));
        let request = parser.request();
        if fail {
            parser
                .fail(request.source, request.token, SourceFailure::Transport)
                .unwrap();
            assert_eq!(
                parser
                    .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                    .unwrap_err(),
                Error::Transport
            );
        } else {
            parser.finish(request.source, request.token).unwrap();
            let Step::Ready(nal) = parser
                .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                .unwrap()
            else {
                panic!("explicit completion did not finish NAL")
            };
            assert_eq!(nal.bytes().as_ref(), &[0x42, 1, 0x80]);
            assert!(matches!(
                parser
                    .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                    .unwrap(),
                Step::End(_)
            ));
        }
    }
}

#[test]
fn unit_count_limit_does_not_erase_previously_emitted_output() {
    let pool = Budget::new(8192).unwrap();
    let mut policy = limits();
    policy.maximum_nal_units = 1;
    let mut parser = NalReader::new(
        &pool,
        Codec::Avc,
        Framing::AnnexB,
        SourceId(21),
        SourceCapabilities {
            length: Some(10),
            random_access: false,
        },
        policy,
    )
    .unwrap();
    let request = parser.request();
    let source =
        Bytes::copy_from_slice(&pool, &[0, 0, 1, 0x67, 0x80, 0, 0, 1, 0x68, 0x80]).unwrap();
    parser
        .provide(&InputWindow::new(request.source, request.token, 0, source).unwrap())
        .unwrap();
    let Step::Ready(first) = parser
        .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("first bounded NAL omitted")
    };
    assert_eq!(
        parser
            .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
    assert_eq!(first.bytes().as_ref(), &[0x67, 0x80]);
}

#[test]
fn configuration_allocation_retry_and_cancel_release_internal_ownership() {
    let pool = Budget::new(16384).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0, 0, 1, 0x67, 0x80]).unwrap();
    let before = pool.used();
    let mut parser = AnnexBConfigParser::new(
        &pool,
        Codec::Avc,
        SourceId(21),
        source.clone(),
        Framing::AnnexB,
        config_limits(),
    )
    .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    drop(blocker);
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), before);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
}
