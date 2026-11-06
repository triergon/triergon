use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::{
    Ac3Frame, Ac3FrameDetails, Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection, BuildStep,
    Eac3BitRange, Eac3Bsi, Eac3BsiLimits, Eac3BsiParser,
};
use triergon_harness_protocol::unhex;

fn class(error: Error) -> &'static str {
    match error {
        Error::Invalid(_) => "invalid",
        Error::Truncated => "truncated",
        Error::Unsupported(_) => "unsupported",
        Error::Limit { .. } => "limit",
        _ => panic!("unexpected BSI error {error:?}"),
    }
}

fn policy() -> Eac3BsiLimits {
    Eac3BsiLimits {
        maximum_frame_bytes: 4096,
        maximum_bsi_bits: 4096,
    }
}

fn fixture(id: &str) -> Vec<u8> {
    let line = include_str!("../fixtures/eac3-bsi-v1.tsv")
        .lines()
        .skip(1)
        .find(|line| line.split('\t').next() == Some(id))
        .unwrap();
    unhex(line.split('\t').nth(1).unwrap())
}

// These controls declare a complete source entity containing one frame. Framing
// may emit a complete frame before discovering a later malformed source suffix.
fn framed(pool: &Budget, input: &[u8], chunk: usize) -> Result<Ac3Frame, Error> {
    let source = Bytes::copy_from_slice(pool, input)?;
    let mut reader = Ac3FrameReader::new(
        pool,
        SourceId(1701),
        SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: false,
        },
        Ac3StreamSelection::Any,
        Ac3FrameLimits {
            maximum_frame_bytes: 4096,
            maximum_frames: 2,
            maximum_input_window: 31,
        },
    )?;
    let generation = reader.generation();
    let mut output: Option<Ac3Frame> = None;
    for _ in 0..100_000 {
        match reader.step(&mut WorkBudget::new(64), &AtomicBool::new(false))? {
            Step::Ready(frame) => {
                if output.is_some() {
                    return Err(Error::Invalid("one-frame control entity"));
                }
                assert_eq!(frame.facts().source, SourceId(1701));
                assert_eq!(frame.facts().generation, generation);
                output = Some(frame);
            }
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).unwrap();
                if start == input.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    let end = input.len().min(start + chunk.min(request.maximum));
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
                return output.ok_or(Error::Truncated);
            }
        }
    }
    panic!("bounded frame control did not finish");
}

fn inspect(pool: &Budget, frame: Ac3Frame, units: u64) -> Result<Eac3Bsi, Error> {
    let mut parser = Eac3BsiParser::new(pool, frame, policy())?;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(units);
        match parser.step(&mut work, &AtomicBool::new(false))? {
            BuildStep::Yield => {}
            BuildStep::Ready(output) => {
                assert!(work.spent() <= units);
                assert!(matches!(
                    parser.step(&mut WorkBudget::new(0), &AtomicBool::new(false))?,
                    BuildStep::Complete
                ));
                return Ok(output);
            }
            BuildStep::Complete => panic!("BSI omitted its output"),
        }
        assert!(work.spent() <= units);
    }
    panic!("bounded BSI control did not finish");
}

fn opt<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}
fn flag(value: Option<bool>) -> String {
    opt(value.map(u8::from))
}
fn range(value: Option<Eac3BitRange>) -> String {
    value.map_or_else(
        || "-".to_owned(),
        |r| format!("{}:{}", r.start_bit(), r.bit_count()),
    )
}

fn fields(output: &Eac3Bsi) -> Vec<String> {
    let f = output.facts();
    vec![
        f.end_bit.to_string(),
        f.prefix_end_bit.to_string(),
        range(f.mixing_metadata),
        opt(f.mixing_definition),
        range(f.informational_metadata),
        opt(f.bitstream_mode),
        flag(f.copyright),
        flag(f.original),
        opt(f.surround_mode),
        opt(f.headphone_mode),
        opt(f.surround_ex_mode),
        opt(f.production),
        opt(f.second_production),
        flag(f.source_fscod),
        flag(f.converter_sync),
        flag(f.block_id),
        flag(f.effective_block_id),
        opt(f.converted_frame_size_code),
        range(f.additional_bsi),
        u8::from(f.trailer.auxiliary_data_present).to_string(),
        opt(f.trailer.auxiliary_length_code),
        u8::from(f.trailer.encoder_info).to_string(),
        f.trailer.crc2_word.to_string(),
        opt(output.channel_map_channels()),
        flag(output.channel_map_matches_coded_count()),
    ]
}

#[test]
fn independent_complete_bsi_corpus_survives_chunks_and_bit_grants() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut count = 0;
    for line in include_str!("../fixtures/eac3-bsi-v1.tsv").lines().skip(1) {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 28);
        let input = unhex(row[1]);
        count += 1;
        for chunk in [1, 2, 7, 31] {
            for units in [1, 2, 17, 4096] {
                let actual =
                    framed(&pool, &input, chunk).and_then(|frame| inspect(&pool, frame, units));
                if row[27] != "ok" {
                    assert_eq!(class(actual.err().unwrap()), row[27], "{}", row[0]);
                    continue;
                }
                let output = actual.unwrap();
                assert_eq!(
                    fields(&output)
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    row[2..27],
                    "{}",
                    row[0]
                );
                assert_eq!(output.frame().bytes().as_ref(), input.as_slice());
                assert!(output.frame().bytes().belongs_to(&pool));
                for bits in [
                    output.facts().mixing_metadata,
                    output.facts().informational_metadata,
                    output.facts().additional_bsi,
                ]
                .into_iter()
                .flatten()
                {
                    let span = output.bit_span(bits).unwrap();
                    assert!(
                        span.covering_bytes()
                            .unwrap()
                            .shares_storage_with(output.frame().bytes())
                    );
                    assert_eq!(span.frame().facts(), output.frame().facts());
                }
            }
        }
    }
    assert_eq!(count, 228);
}

#[test]
fn sid_and_type_controls_preserve_source_ids_without_native_outcome_assumptions() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut sid_seen = [false; 8];
    let mut count = 0;
    for line in include_str!("../fixtures/eac3-diagnosis-v2.tsv")
        .lines()
        .skip(1)
    {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 11);
        count += 1;
        assert_eq!(row[10], "unassigned");
        let input = unhex(row[1]);
        let actual = framed(&pool, &input, 1).and_then(|frame| inspect(&pool, frame, 1));
        let expected = if row[0].ends_with("--full") {
            row[9]
        } else {
            "truncated"
        };
        if expected != "ok" {
            assert_eq!(class(actual.err().unwrap()), expected, "{}", row[0]);
            continue;
        }
        let output = actual.unwrap();
        let Ac3FrameDetails::Eac3 {
            stream_type,
            substream_id,
            ..
        } = output.frame().facts().header.details
        else {
            panic!("EAC3 expected");
        };
        assert_eq!(stream_type.to_string(), row[5]);
        assert_eq!(substream_id.to_string(), row[4]);
        assert_eq!(output.facts().end_bit.to_string(), row[8]);
        if row[3] == "sid-only" {
            sid_seen[usize::from(substream_id)] = true;
        }
    }
    assert_eq!(count, 69);
    assert!(sid_seen.into_iter().all(|seen| seen));
}

#[test]
fn additional_bits_are_unaligned_original_owned_source_with_identity() {
    let pool = Budget::new(8192).unwrap();
    let before = pool.used();
    let output = inspect(
        &pool,
        framed(&pool, &fixture("additional-2-aux--"), 1).unwrap(),
        1,
    )
    .unwrap();
    let range = output.facts().additional_bsi.unwrap();
    assert_ne!(range.start_bit() % 8, 0);
    let span = output.bit_span(range).unwrap();
    let actual: String = (0..range.bit_count())
        .map(|n| if span.bit(n).unwrap() { '1' } else { '0' })
        .collect();
    assert_eq!(actual, "0000001100010100");
    assert_eq!(span.bit(16), Err(Error::Invalid("E-AC-3 bit span index")));
    let source = output.frame().facts();
    drop(output);
    assert!(pool.used() > before);
    assert_eq!(span.frame().facts(), source);
    std::thread::spawn(move || drop(span)).join().unwrap();
    assert_eq!(pool.used(), before);
}

#[test]
fn physical_block_and_rate_presence_are_not_inferred_field_values() {
    let pool = Budget::new(8192).unwrap();
    let get = |id: &str| inspect(&pool, framed(&pool, &fixture(id), 2).unwrap(), 1).unwrap();
    let six = get("converted-blocks-6-flag-0");
    assert_eq!(six.facts().block_id, None);
    assert_eq!(six.facts().effective_block_id, Some(true));
    assert_eq!(six.facts().converted_frame_size_code, Some(37));
    let coded = get("converted-blocks-1-flag-0");
    assert_eq!(coded.facts().block_id, Some(false));
    assert_eq!(coded.facts().effective_block_id, Some(false));
    assert_eq!(coded.facts().converted_frame_size_code, None);
    let reduced = get("reduced-rate-1");
    assert_eq!(reduced.facts().source_fscod, None);
    assert_eq!(reduced.facts().production, Some(0));
    let ordinary = get("information-mode-2");
    assert_eq!(ordinary.facts().source_fscod, Some(true));
    assert_eq!(ordinary.facts().copyright, Some(true));
    assert_eq!(ordinary.facts().original, Some(false));
}

#[test]
fn weighted_channel_locations_report_inconsistency_without_rewriting_prefix_facts() {
    let pool = Budget::new(8192).unwrap();
    for (id, channels, consistent) in [
        ("mode-2-map-zero", 0, false),
        ("mode-2-map-all", 22, false),
        ("mode-2-map-consistent", 3, true),
    ] {
        let output = inspect(&pool, framed(&pool, &fixture(id), 7).unwrap(), 1).unwrap();
        assert_eq!(output.channel_map_channels(), Some(channels));
        assert_eq!(output.channel_map_matches_coded_count(), Some(consistent));
        assert_eq!(output.frame().facts().header.syntax_issues().bits(), 0);
    }
    let absent = inspect(
        &pool,
        framed(&pool, &fixture("mode-2-map-absent"), 1).unwrap(),
        1,
    )
    .unwrap();
    assert_eq!(absent.channel_map_channels(), None);
    assert_eq!(absent.channel_map_matches_coded_count(), None);
}

#[test]
fn zero_and_one_bit_work_preserve_partial_field_progress() {
    let pool = Budget::new(8192).unwrap();
    let frame = framed(&pool, &fixture("information-mode-7"), 1).unwrap();
    let start = usize::from(frame.facts().header.probe_bits);
    let mut parser = Eac3BsiParser::new(&pool, frame, policy()).unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    assert_eq!(parser.position_bits(), start);
    for bit in 1..=3 {
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            parser.step(&mut work, &AtomicBool::new(false)).unwrap(),
            BuildStep::Yield
        ));
        assert_eq!(work.spent(), 1);
        assert_eq!(parser.position_bits(), start + bit);
    }
    for _ in 0..1000 {
        if matches!(
            parser
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap(),
            BuildStep::Ready(_)
        ) {
            return;
        }
    }
    panic!("partial field never completed");
}

#[test]
fn bsi_limits_foreign_pool_and_cancel_are_sticky_without_partial_output() {
    let pool = Budget::new(8192).unwrap();
    let other = Budget::new(8192).unwrap();
    let frame = framed(&pool, &fixture("information-mode-2"), 1).unwrap();
    assert!(matches!(
        Eac3BsiParser::new(&other, frame.clone(), policy()),
        Err(Error::ForeignPool)
    ));
    let mut limit = policy();
    limit.maximum_frame_bytes = 31;
    assert!(matches!(
        Eac3BsiParser::new(&pool, frame.clone(), limit),
        Err(Error::Limit {
            requested: 32,
            maximum: 31
        })
    ));
    limit = policy();
    limit.maximum_bsi_bits = usize::from(frame.facts().header.probe_bits) - 16 + 2;
    let mut bounded = Eac3BsiParser::new(&pool, frame.clone(), limit).unwrap();
    assert!(matches!(
        bounded.step(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        bounded.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::Limit { .. })
    ));
    drop(bounded);
    drop(frame);
    let baseline = pool.used();
    let mut parser = Eac3BsiParser::new(
        &pool,
        framed(&pool, &fixture("additional-64-aux-31"), 1).unwrap(),
        policy(),
    )
    .unwrap();
    assert!(matches!(
        parser
            .step(&mut WorkBudget::new(3), &AtomicBool::new(false))
            .unwrap(),
        BuildStep::Yield
    ));
    assert!(pool.used() > baseline);
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        parser.step(&mut WorkBudget::new(1000), &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn mandatory_tail_overlap_sources_are_preserved_as_invalid_repros() {
    let pool = Budget::new(8192).unwrap();
    let mut count = 0;
    for line in include_str!("../fixtures/eac3-bsi-boundary-v1.tsv")
        .lines()
        .skip(1)
    {
        let row: Vec<_> = line.split('\t').collect();
        count += 1;
        let input = unhex(row[1]);
        let frame = framed(&pool, &input, 1).unwrap();
        let actual = inspect(&pool, frame, 1);
        assert_eq!(class(actual.err().unwrap()), row[2], "{}", row[0]);
    }
    assert_eq!(count, 4);
}

#[test]
fn bsi_inspection_and_span_publication_need_no_new_pool_allocation() {
    let pool = Budget::new(8192).unwrap();
    // The pool's own control storage is charged before any frame exists.
    let baseline = pool.used();
    let frame = framed(&pool, &fixture("mix-mode-7-definition-2"), 1).unwrap();
    let before = pool.used();
    let held = frame.bytes().clone();
    let blocker = pool.reserve(pool.available()).unwrap();
    let output = inspect(&pool, frame, 1).unwrap();
    let bits = output.facts().mixing_metadata.unwrap();
    let span = output.bit_span(bits).unwrap();
    assert!(span.covering_bytes().unwrap().shares_storage_with(&held));
    assert_eq!(pool.available(), 0);
    drop(blocker);
    assert_eq!(pool.used(), before);
    drop(output);
    drop(span);
    drop(held);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn reserved_type_and_flexible_mix_have_explicit_unsupported_boundaries() {
    let pool = Budget::new(8192).unwrap();
    for id in [
        "reserved-stream-type",
        "unsupported-flexible-mix",
        "unsupported-bsid-9",
        "unsupported-bsid-10",
    ] {
        let result = framed(&pool, &fixture(id), 1).and_then(|frame| inspect(&pool, frame, 1));
        assert!(matches!(result, Err(Error::Unsupported(_))), "{id}");
    }
    assert_eq!(
        include_str!("../fixtures/ac3-header-v1.tsv")
            .lines()
            .count()
            - 1,
        240
    );
}

#[test]
fn bsi_views_preserve_nonzero_frame_source_offsets_in_a_stream() {
    let pool = Budget::new(8192).unwrap();
    let one = fixture("carriage-base");
    let mut input = one.clone();
    input.extend_from_slice(&one);
    let source = Bytes::copy_from_slice(&pool, &input).unwrap();
    let mut reader = Ac3FrameReader::new(
        &pool,
        SourceId(1702),
        SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: false,
        },
        Ac3StreamSelection::Eac3Only,
        Ac3FrameLimits {
            maximum_frame_bytes: 4096,
            maximum_frames: 2,
            maximum_input_window: 7,
        },
    )
    .unwrap();
    let generation = reader.generation();
    let mut outputs = Vec::new();
    let mut finished = false;
    for _ in 0..10_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(frame) => outputs.push(inspect(&pool, frame, 1).unwrap()),
            Step::Yield => {}
            Step::NeedInput(request) => {
                let start = request.offset as usize;
                if start == input.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let end = input.len().min(start + request.maximum);
                    reader
                        .provide(
                            &InputWindow::new(
                                request.source,
                                request.token,
                                request.offset,
                                source.slice(start..end).unwrap(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::End(_) => {
                finished = true;
                break;
            }
        }
    }
    assert!(finished);
    assert_eq!(outputs.len(), 2);
    drop(reader);
    drop(source);
    for (index, output) in outputs.iter().enumerate() {
        assert_eq!(output.frame().facts().source, SourceId(1702));
        assert_eq!(output.frame().facts().generation, generation);
        assert_eq!(output.frame().facts().offset, (index * one.len()) as u64);
        let span = output
            .bit_span(output.facts().additional_bsi.unwrap())
            .unwrap();
        assert_eq!(span.frame().facts(), output.frame().facts());
        assert!(
            span.covering_bytes()
                .unwrap()
                .shares_storage_with(output.frame().bytes())
        );
    }
}
