use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step, Time,
    WorkBudget,
};
use triergon_framing::{
    Ac3ConfigKind, Ac3ConfigLimits, Ac3ConfigParser, Ac3Configuration, Ac3Family, Ac3Frame,
    Ac3FrameDetails, Ac3FrameHeader, Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection, BuildStep,
    probe_ac3_header,
};
use triergon_harness_protocol::unhex;

fn limits() -> Ac3FrameLimits {
    Ac3FrameLimits {
        maximum_frame_bytes: 4096,
        maximum_frames: 32,
        maximum_input_window: 31,
    }
}

fn config_limits() -> Ac3ConfigLimits {
    Ac3ConfigLimits {
        maximum_record_bytes: 4096,
        maximum_independent_substreams: 8,
        maximum_input_window: 3,
    }
}

fn class(error: Error) -> &'static str {
    match error {
        Error::Truncated => "truncated",
        Error::Invalid(_) => "invalid",
        Error::Unsupported(_) => "unsupported",
        Error::Limit { .. } => "limit",
        _ => panic!("unexpected fixture error {error:?}"),
    }
}

fn optional(value: Option<u8>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn parameters(header: Ac3FrameHeader) -> String {
    match header.details {
        Ac3FrameDetails::Ac3 {
            fscod,
            frmsizecod,
            bsmod,
            crc1,
            center_mix,
            surround_mix,
            surround_mode,
            dialnorm,
            compression,
        } => [
            fscod.to_string(),
            frmsizecod.to_string(),
            bsmod.to_string(),
            crc1.to_string(),
            optional(center_mix),
            optional(surround_mix),
            optional(surround_mode),
            dialnorm.to_string(),
            optional(compression),
        ]
        .join(","),
        Ac3FrameDetails::Eac3 {
            stream_type,
            substream_id,
            frame_size_code,
            fscod,
            rate_or_blocks_code,
            dialnorm,
            compression,
            dual_mono,
            channel_map_present,
            channel_map,
        } => [
            stream_type.to_string(),
            substream_id.to_string(),
            frame_size_code.to_string(),
            fscod.to_string(),
            rate_or_blocks_code.to_string(),
            dialnorm.to_string(),
            optional(compression),
            optional(dual_mono.map(|value| value.dialnorm)),
            optional(dual_mono.and_then(|value| value.compression)),
            channel_map_present.map_or_else(|| "-".to_owned(), |value| u8::from(value).to_string()),
            channel_map.map_or_else(|| "-".to_owned(), |value| value.to_string()),
        ]
        .join(","),
    }
}

fn assert_header(header: Ac3FrameHeader, row: &[&str]) {
    assert_eq!(
        header.family,
        if row[2] == "ac3" {
            Ac3Family::Ac3
        } else {
            Ac3Family::Eac3
        }
    );
    assert_eq!(header.frame_bytes, row[3].parse::<usize>().unwrap());
    assert_eq!(header.sample_rate_hz, row[4].parse::<u32>().unwrap());
    assert_eq!(header.blocks, row[5].parse::<u8>().unwrap());
    assert_eq!(header.bsid, row[6].parse::<u8>().unwrap());
    assert_eq!(header.acmod, row[7].parse::<u8>().unwrap());
    assert_eq!(u8::from(header.lfe_on), row[8].parse::<u8>().unwrap());
    assert_eq!(header.probe_bits, row[9].parse::<u8>().unwrap());
    assert_eq!(
        header.syntax_issues().bits(),
        row[10].parse::<u8>().unwrap()
    );
    assert_eq!(
        header.sample_span().unwrap(),
        Time::new(row[11].parse().unwrap(), row[12].parse().unwrap()).unwrap()
    );
    assert_eq!(parameters(header), row[13]);
    assert_eq!(header.coded_samples(), u16::from(header.blocks) * 256);
    let (n, d) = header.bit_rate_ratio().unwrap();
    assert_eq!(
        n,
        header.frame_bytes as u64 * 8 * u64::from(header.sample_rate_hz)
    );
    assert_eq!(d, header.coded_samples());
}

#[test]
fn authored_header_matrix_matches_primary_tables_and_fraction_duration() {
    for line in include_str!("../fixtures/ac3-header-v1.tsv")
        .lines()
        .skip(1)
    {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 15);
        let input = unhex(row[1]);
        let result = probe_ac3_header(&input);
        if row[14] != "ok" {
            assert_eq!(class(result.err().unwrap()), row[14], "{}", row[0]);
        } else {
            assert_header(result.unwrap(), &row);
        }
    }
}

fn drive(pool: &Budget, input: &[u8], chunk: usize, units: u64) -> Result<Vec<Ac3Frame>, Error> {
    let source = Bytes::copy_from_slice(pool, input)?;
    let mut reader = Ac3FrameReader::new(
        pool,
        SourceId(901),
        SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: false,
        },
        Ac3StreamSelection::Any,
        limits(),
    )?;
    let generation = reader.generation();
    let mut frames = Vec::new();
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(units);
        match reader.step(&mut work, &AtomicBool::new(false))? {
            Step::Ready(frame) => {
                assert_eq!(frame.facts().generation, generation);
                frames.push(frame);
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = request.offset as usize;
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
            Step::End(end) => {
                assert_eq!(end.offset, input.len() as u64);
                return Ok(frames);
            }
        }
        assert!(work.spent() <= units);
    }
    panic!("audio reader failed bounded progress");
}

#[test]
fn authored_frames_preserve_all_bytes_with_split_sync_prefix_body_and_crc() {
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/ac3-frame-v1.tsv").lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        let input = unhex(row[1]);
        for chunk in (1..=12).chain([17, 31, 64, 4096]) {
            for units in [1, 2, 17, 4096] {
                let actual = drive(&pool, &input, chunk, units);
                if row[5] != "ok" {
                    assert_eq!(class(actual.err().unwrap()), row[5], "{}", row[0]);
                    continue;
                }
                let frames = actual.unwrap();
                let lengths: Vec<_> = if row[2] == "-" {
                    Vec::new()
                } else {
                    row[2]
                        .split(',')
                        .map(|v| v.parse::<usize>().unwrap())
                        .collect()
                };
                assert_eq!(frames.len(), lengths.len());
                let mut offset = 0;
                for (index, frame) in frames.iter().enumerate() {
                    assert_eq!(
                        frame.bytes().as_ref(),
                        &input[offset..offset + lengths[index]]
                    );
                    assert_eq!(frame.facts().source, SourceId(901));
                    assert_eq!(frame.facts().offset, offset as u64);
                    assert_eq!(frame.facts().header.frame_bytes, lengths[index]);
                    assert_eq!(
                        frame.facts().header.bsid.to_string(),
                        row[3].split(',').nth(index).unwrap()
                    );
                    assert_eq!(
                        frame.crc2_word().to_string(),
                        row[4].split(',').nth(index).unwrap()
                    );
                    assert!(frame.bytes().belongs_to(&pool));
                    offset += lengths[index];
                }
                assert_eq!(offset, input.len());
            }
        }
    }
}

fn config(
    pool: &Budget,
    bytes: Bytes,
    kind: Ac3ConfigKind,
    work: u64,
) -> Result<Ac3Configuration, Error> {
    let mut parser = Ac3ConfigParser::new(pool, SourceId(902), bytes, kind, config_limits())?;
    for _ in 0..10_000 {
        match parser.step(&mut WorkBudget::new(work), &AtomicBool::new(false))? {
            BuildStep::Ready(config) => return Ok(config),
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("audio config omitted output"),
        }
    }
    panic!("audio config failed bounded progress");
}

#[test]
fn independent_dac3_records_preserve_reserved_fields_and_declared_config() {
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/dac3-v1.tsv").lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        let input = unhex(row[1]);
        for work in [1, 2, 64] {
            let source = Bytes::copy_from_slice(&pool, &input).unwrap();
            let actual = config(&pool, source.clone(), Ac3ConfigKind::Dac3, work);
            if row[4] != "ok" {
                assert_eq!(class(actual.err().unwrap()), row[4]);
                continue;
            }
            let actual = actual.unwrap();
            assert_eq!(actual.source(), SourceId(902));
            assert!(actual.raw_record().shares_storage_with(&source));
            let Ac3Configuration::Dac3(actual) = actual else {
                panic!("wrong audio config kind");
            };
            let f = actual.fields();
            let fields = [
                f.fscod(),
                f.bsid(),
                f.bsmod(),
                f.acmod(),
                u8::from(f.lfe_on()),
                f.bit_rate_code(),
                f.reserved_bits(),
            ];
            assert_eq!(
                fields
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                row[2]
            );
            assert_eq!(f.syntax_issues().bits().to_string(), row[3]);
            assert_eq!(actual.raw_record().as_ref(), input.as_slice());
        }
    }
}

#[test]
fn independent_dec3_records_preserve_all_substreams_presence_and_reserved_tail() {
    let pool = Budget::new(1 << 20).unwrap();
    for line in include_str!("../fixtures/dec3-v1.tsv").lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        let input = unhex(row[1]);
        for work in [1, 3, 64] {
            let source = Bytes::copy_from_slice(&pool, &input).unwrap();
            let actual = config(&pool, source.clone(), Ac3ConfigKind::Dec3, work);
            if row[7] != "ok" {
                assert_eq!(class(actual.err().unwrap()), row[7], "{}", row[0]);
                continue;
            }
            let Ac3Configuration::Dec3(actual) = actual.unwrap() else {
                panic!("wrong audio config kind");
            };
            assert!(actual.raw_record().shares_storage_with(&source));
            assert_eq!(actual.data_rate_kbps().to_string(), row[2]);
            assert_eq!(actual.substreams().len().to_string(), row[3]);
            let observed: Vec<_> = actual
                .substreams()
                .iter()
                .map(|s| {
                    [
                        s.fscod().to_string(),
                        s.bsid().to_string(),
                        u8::from(s.asvc()).to_string(),
                        s.bsmod().to_string(),
                        s.acmod().to_string(),
                        u8::from(s.lfe_on()).to_string(),
                        s.reserved_one_bit().to_string(),
                        s.reserved_three_bits().to_string(),
                        s.num_dependent().to_string(),
                        s.channel_location()
                            .map_or_else(|| "-".to_owned(), |v| v.to_string()),
                        s.no_dependent_reserved_bit()
                            .map_or_else(|| "-".to_owned(), |v| v.to_string()),
                    ]
                    .join(",")
                })
                .collect();
            assert_eq!(observed.join(";"), row[4]);
            assert_eq!(
                actual.trailing_reserved_bytes().unwrap().as_ref(),
                unhex(row[5]).as_slice()
            );
            assert_eq!(actual.syntax_issues().bits().to_string(), row[6]);
            for (index, descriptor) in actual.substreams().iter().enumerate() {
                assert_eq!(
                    actual.substream_bytes(index).unwrap().as_ref(),
                    descriptor.raw_bytes()
                );
                assert!(
                    actual
                        .substream_bytes(index)
                        .unwrap()
                        .shares_storage_with(&source)
                );
            }
        }
    }
}

#[test]
fn audio_source_pool_generation_starvation_and_zero_work_are_explicit() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    let caps = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let mut reader =
        Ac3FrameReader::new(&pool, SourceId(1), caps, Ac3StreamSelection::Any, limits()).unwrap();
    let stale =
        Ac3FrameReader::new(&pool, SourceId(1), caps, Ac3StreamSelection::Any, limits()).unwrap();
    let request = reader.request();
    let source = Bytes::copy_from_slice(&pool, &[0x0b, 0x77]).unwrap();
    assert_eq!(
        reader.provide(
            &InputWindow::new(
                request.source,
                request.token.clone(),
                0,
                Bytes::copy_from_slice(&foreign, &[0x0b, 0x77]).unwrap()
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

fn first_frame_bytes() -> Vec<u8> {
    let line = include_str!("../fixtures/ac3-frame-v1.tsv")
        .lines()
        .nth(1)
        .unwrap();
    unhex(line.split('\t').nth(1).unwrap())
}

#[test]
fn audio_reserve_and_freeze_pressure_never_reparse_or_lose_input() {
    let pool = Budget::new(8192).unwrap();
    let input = first_frame_bytes();
    let source = Bytes::copy_from_slice(&pool, &input).unwrap();
    let mut policy = limits();
    policy.maximum_input_window = input.len();
    let mut reader = Ac3FrameReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: false,
        },
        Ac3StreamSelection::Any,
        policy,
    )
    .unwrap();
    let request = reader.request();
    reader
        .provide(&InputWindow::new(request.source, request.token, 0, source).unwrap())
        .unwrap();
    // 12 input-prefix bytes, one length inspection and one full prefix probe.
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(14), &AtomicBool::new(false))
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), 12);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(reader.position(), 12);
    drop(blocker);
    // One reserve plus prefix copy and remaining body copy; freeze is separate.
    assert!(matches!(
        reader
            .step(
                &mut WorkBudget::new(input.len() as u64 + 1),
                &AtomicBool::new(false)
            )
            .unwrap(),
        Step::Yield
    ));
    assert_eq!(reader.position(), input.len() as u64);
    let blocker = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(reader.position(), input.len() as u64);
    drop(blocker);
    let output = match reader
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(frame) => frame,
        _ => panic!("frame lost at freeze retry"),
    };
    assert_eq!(output.bytes().as_ref(), input.as_slice());
}

#[test]
fn incomplete_audio_never_becomes_eof_on_transport_failure_or_cancel() {
    let pool = Budget::new(8192).unwrap();
    let data = first_frame_bytes();
    for failure in [SourceFailure::Transport, SourceFailure::Cancelled] {
        let mut reader = Ac3FrameReader::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            Ac3StreamSelection::Any,
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
                    Bytes::copy_from_slice(&pool, &data[..12]).unwrap(),
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
}

#[test]
fn audio_owned_output_and_config_views_survive_reader_drop_and_thread_release() {
    let pool = Budget::new(8192).unwrap();
    let baseline = pool.used();
    let frame = drive(&pool, &first_frame_bytes(), 1, 1).unwrap().remove(0);
    let held = frame.clone();
    drop(frame);
    assert!(pool.used() > baseline);
    std::thread::spawn(move || drop(held)).join().unwrap();
    assert_eq!(pool.used(), baseline);
    let row = include_str!("../fixtures/dec3-v1.tsv")
        .lines()
        .nth(1)
        .unwrap();
    let source = Bytes::copy_from_slice(&pool, &unhex(row.split('\t').nth(1).unwrap())).unwrap();
    let result = config(&pool, source, Ac3ConfigKind::Dec3, 1).unwrap();
    let Ac3Configuration::Dec3(result) = result else {
        panic!("wrong config");
    };
    let held = result.substream_bytes(0).unwrap();
    drop(result);
    assert!(pool.used() > baseline);
    drop(held);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn atomic_audio_cancel_releases_internal_scratch_and_stays_sticky() {
    let pool = Budget::new(8192).unwrap();
    let data = first_frame_bytes();
    let source = Bytes::copy_from_slice(&pool, &data[..12]).unwrap();
    let before = pool.used();
    let mut reader = Ac3FrameReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Ac3StreamSelection::Any,
        limits(),
    )
    .unwrap();
    let request = reader.request();
    reader
        .provide(&InputWindow::new(request.source, request.token, 0, source.clone()).unwrap())
        .unwrap();
    assert!(matches!(
        reader
            .step(&mut WorkBudget::new(15), &AtomicBool::new(false))
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
}

#[test]
fn config_limits_pool_and_cancel_have_no_hidden_allocations_or_partial_results() {
    let pool = Budget::new(8192).unwrap();
    let foreign = Budget::new(8192).unwrap();
    let source = Bytes::copy_from_slice(&foreign, &[0, 0, 0]).unwrap();
    assert!(matches!(
        Ac3ConfigParser::new(
            &pool,
            SourceId(1),
            source,
            Ac3ConfigKind::Dac3,
            config_limits()
        ),
        Err(Error::ForeignPool)
    ));
    let source = Bytes::copy_from_slice(&pool, &[0, 0, 0]).unwrap();
    let mut parser = Ac3ConfigParser::new(
        &pool,
        SourceId(1),
        source.clone(),
        Ac3ConfigKind::Dac3,
        config_limits(),
    )
    .unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    let before = pool.used();
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), before);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    let mut policy = config_limits();
    policy.maximum_record_bytes = 2;
    assert!(matches!(
        Ac3ConfigParser::new(
            &pool,
            SourceId(1),
            source.clone(),
            Ac3ConfigKind::Dac3,
            policy
        ),
        Err(Error::Limit {
            requested: 3,
            maximum: 2
        })
    ));
    policy = config_limits();
    policy.maximum_independent_substreams = 1;
    // num_ind_sub = 1 declares two descriptors; this fails before either body.
    let two = Bytes::copy_from_slice(&pool, &[0, 1]).unwrap();
    let mut bounded =
        Ac3ConfigParser::new(&pool, SourceId(1), two, Ac3ConfigKind::Dec3, policy).unwrap();
    assert!(matches!(
        bounded.step(&mut WorkBudget::new(3), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    assert!(matches!(
        bounded.step(&mut WorkBudget::new(3), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    policy = config_limits();
    policy.maximum_independent_substreams = 9;
    assert!(matches!(
        Ac3ConfigParser::new(&pool, SourceId(1), source, Ac3ConfigKind::Dac3, policy),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn declared_family_and_frame_size_limits_reject_before_body_allocation() {
    let pool = Budget::new(8192).unwrap();
    let input = first_frame_bytes();
    for selection in [Ac3StreamSelection::Eac3Only, Ac3StreamSelection::Any] {
        let mut policy = limits();
        if selection == Ac3StreamSelection::Any {
            policy.maximum_frame_bytes = 127;
        }
        let mut reader = Ac3FrameReader::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: Some(input.len() as u64),
                random_access: false,
            },
            selection,
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
                    Bytes::copy_from_slice(&pool, &input[..12]).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let result = reader.step(&mut WorkBudget::new(100), &AtomicBool::new(false));
        if selection == Ac3StreamSelection::Any {
            assert!(matches!(
                result,
                Err(Error::Limit {
                    requested: 128,
                    maximum: 127
                })
            ));
        } else {
            assert!(matches!(result, Err(Error::Unsupported(_))));
        }
        assert_eq!(reader.position(), 6);
    }
}

#[test]
fn audio_count_limit_preserves_previously_emitted_frame() {
    let pool = Budget::new(8192).unwrap();
    let one = first_frame_bytes();
    let mut data = one.clone();
    data.extend_from_slice(&one);
    let mut policy = limits();
    policy.maximum_frames = 1;
    policy.maximum_input_window = data.len();
    let mut reader = Ac3FrameReader::new(
        &pool,
        SourceId(1),
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: false,
        },
        Ac3StreamSelection::Any,
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
                Bytes::copy_from_slice(&pool, &data).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let first = match reader
        .step(&mut WorkBudget::new(10000), &AtomicBool::new(false))
        .unwrap()
    {
        Step::Ready(frame) => frame,
        _ => panic!("first frame missing"),
    };
    assert!(matches!(
        reader.step(&mut WorkBudget::new(10000), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
    drop(reader);
    assert_eq!(first.bytes().as_ref(), one.as_slice());
}

#[test]
fn reduced_rate_field_presence_and_dependent_sample_span_are_source_facts() {
    let table = include_str!("../fixtures/ac3-header-v1.tsv");
    let find = |id: &str| {
        let row = table
            .lines()
            .skip(1)
            .find(|line| line.split('\t').next() == Some(id))
            .unwrap();
        probe_ac3_header(&unhex(row.split('\t').nth(1).unwrap())).unwrap()
    };
    let reduced = find("eac3-reduced-1");
    assert_eq!(reduced.eac3_fscod2(), Some(1));
    assert_eq!(reduced.eac3_numblkscod(), None);
    assert_eq!(reduced.blocks, 6);
    assert_eq!(
        reduced.sample_span().unwrap(),
        Time::new(1536, 22050).unwrap()
    );
    let independent = find("eac3-fs-0-blocks-3-stream-0");
    let dependent = find("eac3-fs-0-blocks-3-stream-1");
    assert_eq!(dependent.eac3_fscod2(), None);
    assert_eq!(dependent.eac3_numblkscod(), Some(3));
    assert_eq!(
        dependent.sample_span().unwrap(),
        independent.sample_span().unwrap()
    );
}
