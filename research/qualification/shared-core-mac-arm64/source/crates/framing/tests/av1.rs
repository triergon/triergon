use safe_media_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use safe_media_framing::{
    Av1ConfigLimits, Av1ConfigParser, Av1Configuration, Av1Obu, Av1ObuFraming, Av1ObuLimits,
    Av1ObuReader, BuildStep, Vp9ConfigContext, Vp9ConfigLimits, Vp9ConfigParser, Vp9Configuration,
};
use std::sync::atomic::{AtomicBool, Ordering};

fn limits() -> Av1ObuLimits {
    Av1ObuLimits {
        maximum_obu_bytes: 1024,
        maximum_obus: 64,
        maximum_input_window: 17,
    }
}

fn config_limits() -> Av1ConfigLimits {
    Av1ConfigLimits {
        maximum_record_bytes: 4096,
        obu: limits(),
    }
}

fn hex(value: &str) -> Vec<u8> {
    if value == "~" {
        return Vec::new();
    }
    assert_eq!(value.len() % 2, 0);
    (0..value.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&value[at..at + 2], 16).unwrap())
        .collect()
}

fn error_class(error: Error) -> &'static str {
    match error {
        Error::Truncated => "truncated",
        Error::Unsupported(_) => "unsupported",
        Error::Invalid(_) => "invalid",
        Error::Limit { .. } => "limit",
        Error::Overflow => "overflow",
        _ => panic!("unexpected fixture error {error:?}"),
    }
}

fn drive(
    pool: &Budget,
    framing: Av1ObuFraming,
    input: &[u8],
    chunk: usize,
    units: u64,
) -> Result<Vec<Av1Obu>, Error> {
    let source = Bytes::copy_from_slice(pool, input)?;
    let mut reader = Av1ObuReader::new(
        pool,
        SourceId(741),
        SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: false,
        },
        framing,
        limits(),
    )?;
    let generation = reader.generation();
    let mut output = Vec::new();
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(units);
        match reader.step(&mut work, &AtomicBool::new(false))? {
            Step::Ready(obu) => {
                assert_eq!(obu.facts().generation, generation);
                output.push(obu);
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).unwrap();
                if start == input.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    let end = (start + chunk.min(request.maximum)).min(input.len());
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        source.slice(start..end)?,
                    )?)?;
                }
            }
            Step::End(location) => {
                assert_eq!(location.offset, input.len() as u64);
                return Ok(output);
            }
        }
        assert!(work.spent() <= units);
    }
    panic!("OBU reader made no bounded progress");
}

#[test]
fn independent_obu_corpus_survives_all_chunk_sizes_and_tiny_work() {
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/av1-obu-v1.tsv").lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 12);
        let input = hex(row[2]);
        let framing = if row[1] == "low" {
            Av1ObuFraming::LowOverhead
        } else {
            Av1ObuFraming::OneObu
        };
        for chunk in 1..=input.len().max(1) {
            for work in [1, 2, 7, 64] {
                let result = drive(&pool, framing, &input, chunk, work);
                if row[11] != "ok" {
                    assert_eq!(
                        error_class(result.err().unwrap()),
                        row[11],
                        "{} chunk={chunk} work={work}",
                        row[0]
                    );
                    continue;
                }
                let actual = result.unwrap();
                let wires: Vec<_> = if row[3] == "-" {
                    Vec::new()
                } else {
                    row[3].split(',').map(hex).collect()
                };
                let payloads: Vec<_> = if row[4] == "-" {
                    Vec::new()
                } else {
                    row[4].split(',').map(hex).collect()
                };
                assert_eq!(actual.len(), wires.len(), "{}", row[0]);
                for (index, obu) in actual.iter().enumerate() {
                    let item = |column: usize| row[column].split(',').nth(index).unwrap();
                    let facts = obu.facts();
                    assert_eq!(obu.wire_bytes().as_ref(), wires[index].as_slice());
                    assert_eq!(
                        obu.payload_bytes().unwrap().as_ref(),
                        payloads[index].as_slice()
                    );
                    assert!(obu.wire_bytes().belongs_to(&pool));
                    assert_eq!(facts.source, SourceId(741));
                    assert_eq!(facts.header.obu_type(), item(5).parse::<u8>().unwrap());
                    assert_eq!(facts.header.raw_byte, wires[index][0]);
                    assert_eq!(
                        facts.header.extension.map(|ext| ext.raw_byte),
                        if item(6) == "-" {
                            None
                        } else {
                            Some(u8::from_str_radix(item(6), 16).unwrap())
                        }
                    );
                    if let Some(extension) = facts.header.extension {
                        let raw = u8::from_str_radix(item(6), 16).unwrap();
                        assert_eq!(extension.temporal_id(), raw / 32);
                        assert_eq!(extension.spatial_id(), (raw / 8) % 4);
                        assert_eq!(extension.reserved_bits(), raw % 8);
                    }
                    match facts.size_field {
                        Some(leb) => {
                            assert_eq!(leb.raw_bytes(), hex(item(7)).as_slice());
                            assert_eq!(leb.width() as usize, hex(item(7)).len());
                            assert_eq!(leb.value() as usize, payloads[index].len());
                        }
                        None => assert_eq!(item(7), "-"),
                    }
                    assert_eq!(facts.offset, item(8).parse::<u64>().unwrap());
                    assert_eq!(facts.payload_offset, item(9).parse::<u64>().unwrap());
                    assert_eq!(facts.payload_length, payloads[index].len());
                    assert_eq!(facts.wire_length, wires[index].len());
                    assert_eq!(
                        facts.header.syntax_issues().bits(),
                        item(10).parse::<u8>().unwrap()
                    );
                    assert_eq!(
                        facts.external_length,
                        if row[1] == "one" {
                            Some(input.len() as u64)
                        } else {
                            None
                        }
                    );
                }
            }
        }
    }
}

fn parse_config(pool: &Budget, input: &[u8], units: u64) -> Result<Av1Configuration, Error> {
    let source = Bytes::copy_from_slice(pool, input)?;
    let mut parser = Av1ConfigParser::new(pool, SourceId(742), source.clone(), config_limits())?;
    for _ in 0..100_000 {
        match parser.step(&mut WorkBudget::new(units), &AtomicBool::new(false))? {
            BuildStep::Yield => {}
            BuildStep::Ready(config) => {
                assert!(config.raw_record().shares_storage_with(&source));
                return Ok(config);
            }
            BuildStep::Complete => panic!("configuration omitted output"),
        }
    }
    panic!("configuration parser made no bounded progress");
}

#[test]
fn independent_av1c_records_preserve_fields_presence_reserved_bits_and_obus() {
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/av1-config-v1.tsv")
        .lines()
        .skip(1)
    {
        let row: Vec<_> = line.split('\t').collect();
        let input = hex(row[1]);
        for units in [1, 3, 64] {
            let result = parse_config(&pool, &input, units);
            if row[5] != "ok" {
                assert_eq!(error_class(result.err().unwrap()), row[5], "{}", row[0]);
                continue;
            }
            let config = result.unwrap();
            assert_eq!(config.raw_record().as_ref(), input.as_slice());
            let f = config.fields();
            let expected: Vec<_> = row[2]
                .split(',')
                .map(|cell| cell.parse::<u8>().unwrap())
                .collect();
            assert_eq!(
                vec![
                    u8::from(f.marker()),
                    f.version(),
                    f.seq_profile(),
                    f.seq_level_idx_0(),
                    u8::from(f.seq_tier_0()),
                    u8::from(f.high_bitdepth()),
                    u8::from(f.twelve_bit()),
                    u8::from(f.monochrome()),
                    u8::from(f.chroma_subsampling_x()),
                    u8::from(f.chroma_subsampling_y()),
                    f.chroma_sample_position(),
                    f.reserved_three_bits(),
                    u8::from(f.initial_presentation_delay_present()),
                    f.delay_or_reserved_bits()
                ],
                expected
            );
            assert_eq!(f.syntax_issues().bits(), row[3].parse::<u8>().unwrap());
            assert_eq!(
                f.initial_presentation_delay_minus_one(),
                if expected[12] == 1 {
                    Some(expected[13])
                } else {
                    None
                }
            );
            assert_eq!(
                f.absent_delay_reserved_bits(),
                if expected[12] == 0 {
                    Some(expected[13])
                } else {
                    None
                }
            );
            let types: Vec<_> = if row[4] == "-" {
                Vec::new()
            } else {
                row[4]
                    .split(',')
                    .map(|cell| cell.parse::<u8>().unwrap())
                    .collect()
            };
            assert_eq!(
                config
                    .config_obus()
                    .iter()
                    .map(|obu| obu.facts().header.obu_type())
                    .collect::<Vec<_>>(),
                types
            );
            assert_eq!(config.source(), SourceId(742));
            for obu in config.config_obus() {
                assert_eq!(obu.facts().generation, config.generation());
                assert!(obu.wire_bytes().shares_storage_with(config.raw_record()));
                let start = obu.facts().offset as usize;
                assert_eq!(
                    obu.wire_bytes().as_ref(),
                    &input[start..start + obu.facts().wire_length]
                );
            }
        }
    }
}

fn parse_vp9(
    pool: &Budget,
    source: Bytes,
    context: Vp9ConfigContext,
    work: u64,
) -> Result<Vp9Configuration, Error> {
    let mut parser = Vp9ConfigParser::new(
        pool,
        SourceId(743),
        source,
        context,
        Vp9ConfigLimits {
            maximum_record_bytes: 4096,
            maximum_input_window: 3,
        },
    )?;
    for _ in 0..10_000 {
        match parser.step(&mut WorkBudget::new(work), &AtomicBool::new(false))? {
            BuildStep::Yield => {}
            BuildStep::Ready(config) => return Ok(config),
            BuildStep::Complete => panic!("VP9 record omitted output"),
        }
    }
    panic!("VP9 record made no bounded progress");
}

#[test]
fn independent_vp9_records_keep_context_fields_and_opaque_initialization() {
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/vp9-config-v1.tsv")
        .lines()
        .skip(1)
    {
        let row: Vec<_> = line.split('\t').collect();
        let input = hex(row[1]);
        let context = Vp9ConfigContext {
            version: row[2].parse().unwrap(),
            flags: row[3].parse().unwrap(),
        };
        for work in [1, 2, 64] {
            let source = Bytes::copy_from_slice(&pool, &input).unwrap();
            let result = parse_vp9(&pool, source.clone(), context, work);
            if row[6] != "ok" {
                assert_eq!(error_class(result.err().unwrap()), row[6], "{}", row[0]);
                continue;
            }
            let config = result.unwrap();
            let f = config.fields();
            let expected: Vec<_> = row[4]
                .split(',')
                .map(|cell| cell.parse::<u16>().unwrap())
                .collect();
            assert_eq!(
                vec![
                    u16::from(f.profile()),
                    u16::from(f.level()),
                    u16::from(f.bit_depth()),
                    u16::from(f.chroma_subsampling()),
                    u16::from(u8::from(f.video_full_range())),
                    u16::from(f.colour_primaries()),
                    u16::from(f.transfer_characteristics()),
                    u16::from(f.matrix_coefficients()),
                    f.initialization_size()
                ],
                expected
            );
            assert_eq!(config.context(), context);
            assert_eq!(config.source(), SourceId(743));
            assert_eq!(config.syntax_issues().bits(), row[5].parse::<u8>().unwrap());
            assert!(config.raw_record().shares_storage_with(&source));
            assert_eq!(config.raw_record().as_ref(), input.as_slice());
            assert_eq!(config.initialization_bytes().unwrap().as_ref(), &input[8..]);
        }
    }
}

#[test]
fn obu_input_identity_starvation_and_zero_work_are_not_eof() {
    let pool = Budget::new(8192).unwrap();
    let other = Budget::new(8192).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(3),
        random_access: false,
    };
    let mut reader = Av1ObuReader::new(
        &pool,
        SourceId(1),
        capabilities,
        Av1ObuFraming::LowOverhead,
        limits(),
    )
    .unwrap();
    let stale = Av1ObuReader::new(
        &pool,
        SourceId(1),
        capabilities,
        Av1ObuFraming::LowOverhead,
        limits(),
    )
    .unwrap();
    let request = reader.request();
    let source = Bytes::copy_from_slice(&pool, &[0x2a, 1, 0x80]).unwrap();
    assert_eq!(
        reader.provide(
            &InputWindow::new(
                request.source,
                request.token.clone(),
                0,
                Bytes::copy_from_slice(&other, &[0x2a, 1, 0x80]).unwrap()
            )
            .unwrap()
        ),
        Err(Error::ForeignPool)
    );
    assert!(matches!(
        reader
            .provide(&InputWindow::new(request.source, stale.token(), 0, source.clone()).unwrap()),
        Err(Error::StaleGeneration { .. })
    ));
    reader
        .provide(
            &InputWindow::new(
                request.source,
                request.token.clone(),
                0,
                source.slice(0..0).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::NeedInput(_)
    ));
    assert_eq!(reader.position(), 0);
    reader
        .provide(&InputWindow::new(request.source, request.token, 0, source).unwrap())
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), 0);
}

#[test]
fn obu_reserve_and_freeze_pressure_preserve_exact_prefix_and_payload() {
    let pool = Budget::new(8192).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0x2a, 2, 0x12, 0x80]).unwrap();
    let mut reader = Av1ObuReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(4),
            random_access: false,
        },
        Av1ObuFraming::LowOverhead,
        limits(),
    )
    .unwrap();
    let request = reader.request();
    reader
        .provide(&InputWindow::new(request.source, request.token, 0, source).unwrap())
        .unwrap();
    let cancel = AtomicBool::new(false);
    // Two input bytes plus resolve; reservation has not yet happened.
    assert!(matches!(
        reader.step(&mut WorkBudget::new(3), &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), 2);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(reader.position(), 2);
    drop(blocker);
    // Reserve + copy two prefix bytes + read/copy two payload bytes.
    assert!(matches!(
        reader.step(&mut WorkBudget::new(5), &cancel).unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), 4);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(reader.position(), 4);
    drop(blocker);
    let output = match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
        Step::Ready(obu) => obu,
        _ => panic!("completed OBU was not retained for freeze retry"),
    };
    assert_eq!(output.wire_bytes().as_ref(), &[0x2a, 2, 0x12, 0x80]);
    assert_eq!(output.payload_bytes().unwrap().as_ref(), &[0x12, 0x80]);
}

#[test]
fn obu_unknown_length_needs_explicit_finish_and_failures_stay_sticky() {
    let pool = Budget::new(8192).unwrap();
    for failure in [SourceFailure::Transport, SourceFailure::Cancelled] {
        let mut reader = Av1ObuReader::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            Av1ObuFraming::LowOverhead,
            limits(),
        )
        .unwrap();
        let request = reader.request();
        reader
            .provide(
                &InputWindow::new(
                    request.source,
                    request.token,
                    0,
                    Bytes::copy_from_slice(&pool, &[0x2a, 2, 0x80]).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            reader
                .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                .unwrap(),
            Step::NeedInput(_)
        ));
        let request = reader.request();
        reader.fail(request.source, request.token, failure).unwrap();
        let expected = if failure == SourceFailure::Transport {
            Error::Transport
        } else {
            Error::Cancelled
        };
        assert_eq!(
            reader
                .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                .unwrap_err(),
            expected
        );
        assert_eq!(
            reader
                .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
                .unwrap_err(),
            expected
        );
    }
    let mut reader = Av1ObuReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Av1ObuFraming::LowOverhead,
        limits(),
    )
    .unwrap();
    let request = reader.request();
    reader
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                0,
                Bytes::copy_from_slice(&pool, &[0x12, 0]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(_)
    ));
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap(),
        Step::NeedInput(_)
    ));
    let request = reader.request();
    reader.finish(request.source, request.token).unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Step::End(_)
    ));
}

#[test]
fn explicit_single_obu_requires_known_nonempty_boundary() {
    let pool = Budget::new(8192).unwrap();
    assert!(matches!(
        Av1ObuReader::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            Av1ObuFraming::OneObu,
            limits()
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(
        Av1ObuReader::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: Some(0),
                random_access: false,
            },
            Av1ObuFraming::OneObu,
            limits()
        )
        .is_err()
    );
    let one = drive(&pool, Av1ObuFraming::OneObu, &[0x28, 0, 0x80], 1, 1).unwrap();
    assert!(one[0].facts().size_field.is_none());
    assert_eq!(one[0].facts().external_length, Some(3));
    assert_eq!(one[0].payload_bytes().unwrap().as_ref(), &[0, 0x80]);
}

#[test]
fn config_descriptor_pressure_is_retryable_and_cancel_drops_private_ownership() {
    let pool = Budget::new(1 << 20).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0x81, 0, 0x0c, 0, 0x0a, 1, 0x80]).unwrap();
    let baseline = pool.used();
    let mut parser =
        Av1ConfigParser::new(&pool, SourceId(1), source.clone(), config_limits()).unwrap();
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
    assert_eq!(pool.used(), baseline);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn owned_obu_and_config_views_keep_charges_after_teardown_and_thread_release() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let config = parse_config(&pool, &[0x81, 0, 0x0c, 0, 0x0a, 1, 0x80, 0x2a, 1, 0x12], 1).unwrap();
    let held = config.config_obus()[1].clone();
    assert!(held.wire_bytes().shares_storage_with(config.raw_record()));
    drop(config);
    assert!(pool.used() > baseline);
    assert_eq!(held.wire_bytes().as_ref(), &[0x2a, 1, 0x12]);
    std::thread::spawn(move || drop(held)).join().unwrap();
    assert_eq!(pool.used(), baseline);
    let held = drive(&pool, Av1ObuFraming::LowOverhead, &[0x2a, 1, 0x80], 1, 1)
        .unwrap()
        .remove(0);
    let clone = held.clone();
    drop(held);
    assert!(pool.used() > baseline);
    drop(clone);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn record_pool_source_and_version_boundaries_are_explicit() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    assert!(matches!(
        Av1ConfigParser::new(
            &pool,
            SourceId(1),
            Bytes::copy_from_slice(&foreign, &[0x81, 0, 0x0c, 0]).unwrap(),
            config_limits()
        ),
        Err(Error::ForeignPool)
    ));
    let mut policy = config_limits();
    policy.maximum_record_bytes = 3;
    assert!(matches!(
        Av1ConfigParser::new(
            &pool,
            SourceId(1),
            Bytes::copy_from_slice(&pool, &[0x81, 0, 0x0c, 0]).unwrap(),
            policy
        ),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        Vp9ConfigParser::new(
            &pool,
            SourceId(1),
            Bytes::copy_from_slice(&foreign, &[0; 8]).unwrap(),
            Vp9ConfigContext {
                version: 1,
                flags: 0
            },
            Vp9ConfigLimits {
                maximum_record_bytes: 32,
                maximum_input_window: 4
            }
        ),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        parse_vp9(
            &pool,
            Bytes::copy_from_slice(&pool, &[0; 8]).unwrap(),
            Vp9ConfigContext {
                version: 0,
                flags: 0
            },
            1
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn obu_count_failure_preserves_prior_owned_output() {
    let pool = Budget::new(8192).unwrap();
    let mut policy = limits();
    policy.maximum_obus = 1;
    let mut reader = Av1ObuReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(4),
            random_access: false,
        },
        Av1ObuFraming::LowOverhead,
        policy,
    )
    .unwrap();
    let request = reader.request();
    reader
        .provide(
            &InputWindow::new(
                request.source,
                request.token,
                0,
                Bytes::copy_from_slice(&pool, &[0x12, 0, 0x7a, 0]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let first = match reader
        .step(&mut WorkBudget::new(100), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(obu) => obu,
        _ => panic!("first OBU omitted"),
    };
    assert!(matches!(
        reader.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    assert_eq!(first.wire_bytes().as_ref(), &[0x12, 0]);
}

#[test]
fn zero_work_and_cancellation_apply_to_owned_record_parsers() {
    let pool = Budget::new(8192).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0x81, 0, 0x0c, 0]).unwrap();
    let mut av1 = Av1ConfigParser::new(&pool, SourceId(1), source, config_limits()).unwrap();
    assert!(matches!(
        av1.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    let source = Bytes::copy_from_slice(&pool, &[0, 10, 0x80, 1, 1, 1, 0, 0]).unwrap();
    let mut vp9 = Vp9ConfigParser::new(
        &pool,
        SourceId(1),
        source,
        Vp9ConfigContext {
            version: 1,
            flags: 0,
        },
        Vp9ConfigLimits {
            maximum_record_bytes: 64,
            maximum_input_window: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        vp9.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        vp9.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        vp9.step(&mut WorkBudget::new(100), &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn atomic_obu_cancel_releases_scratch_and_retained_input_without_resuming() {
    let pool = Budget::new(8192).unwrap();
    let source = Bytes::copy_from_slice(&pool, &[0x2a, 2, 0x12, 0x80]).unwrap();
    let before = pool.used();
    let mut reader = Av1ObuReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(4),
            random_access: false,
        },
        Av1ObuFraming::LowOverhead,
        limits(),
    )
    .unwrap();
    let request = reader.request();
    reader
        .provide(&InputWindow::new(request.source, request.token, 0, source.clone()).unwrap())
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(4), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert!(pool.used() > before);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        reader.step(&mut WorkBudget::new(0), &cancel).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(pool.used(), before);
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        reader.step(&mut WorkBudget::new(100), &cancel).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(source.as_ref(), &[0x2a, 2, 0x12, 0x80]);
}
