// Additive runtime regressions staged before the selected field implementation.
// Original primary-derived metadata vectors and independent bit/Fraction proof
// are hash-bound beside these fixtures. Parent owns compiler/test execution.
mod common;
use std::{fs, path::Path};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Limits,
    Sample, SampleStep, SampleTable, SampleTableBuilder, Trex,
};

#[derive(Debug)]
struct Index {
    table: Option<SampleTable>,
    fragment: Option<FragmentIndex>,
}

fn build(
    kind: &str,
    bytes: &[u8],
    caller: Option<i128>,
    budget: &Budget,
    quantum: u64,
) -> Result<Index, Error> {
    let raw = BoxView::new(common::bytes(budget, bytes), 0)?;
    if kind == "stbl" {
        let mut builder = SampleTableBuilder::new(raw, budget.clone(), Limits::default())?;
        assert_eq!(builder.poll(&mut WorkBudget::new(0))?, BuildStatus::Yield);
        for _ in 0..10_000 {
            match builder.poll(&mut WorkBudget::new(quantum)) {
                Ok(BuildStatus::Yield) => {}
                Ok(BuildStatus::Ready) => {
                    return builder.finish().map(|table| Index {
                        table: Some(table),
                        fragment: None,
                    });
                }
                Err(error) => {
                    assert_eq!(builder.raw().bytes().as_ref(), bytes);
                    assert_eq!(builder.poll(&mut WorkBudget::new(quantum)), Err(error));
                    return Err(error);
                }
            }
        }
    } else {
        assert_eq!(kind, "traf");
        let mut builder = FragmentBuilder::new(
            raw,
            Trex {
                track_id: 1,
                description_index: 1,
                duration: 2,
                size: 3,
                flags: 0x02000000,
            },
            FragmentContext {
                moof_start: 1 << 40,
                implicit_base: ImplicitBase::FirstTraf,
                decode_origin: caller,
                first_ordinal: 1,
                description_count: 1,
            },
            budget.clone(),
            Limits::default(),
        )?;
        assert_eq!(builder.poll(&mut WorkBudget::new(0))?, BuildStatus::Yield);
        for _ in 0..10_000 {
            match builder.poll(&mut WorkBudget::new(quantum)) {
                Ok(BuildStatus::Yield) => {}
                Ok(BuildStatus::Ready) => {
                    return builder.finish().map(|fragment| Index {
                        table: None,
                        fragment: Some(fragment),
                    });
                }
                Err(error) => {
                    assert_eq!(builder.raw().bytes().as_ref(), bytes);
                    assert_eq!(builder.poll(&mut WorkBudget::new(quantum)), Err(error));
                    return Err(error);
                }
            }
        }
    }
    panic!("bounded metadata builder stalled");
}

fn value<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "null".to_owned(), |v| v.to_string())
}
fn sample_row(sample: &Sample) -> Vec<String> {
    vec![
        sample.ordinal.to_string(),
        sample.range.start.to_string(),
        sample.range.end.to_string(),
        value(sample.dts_ticks),
        value(sample.cts_ticks),
        sample.duration_ticks.to_string(),
        value(sample.composition_offset.map(|v| v.version())),
        value(sample.composition_offset.map(|v| v.raw_bits())),
        value(sample.composition_offset.map(|v| v.ticks())),
        value(sample.composition_offset.and_then(|v| v.signed_value())),
        value(sample.fragment_flags),
        sample.is_container_sync().to_string(),
    ]
}
fn sample(
    mut poll: impl FnMut(&mut WorkBudget) -> Result<SampleStep, Error>,
    quantum: u64,
) -> Sample {
    for _ in 0..10_000 {
        match poll(&mut WorkBudget::new(quantum)).unwrap() {
            SampleStep::Sample(sample) => return sample,
            SampleStep::Yield => {}
            SampleStep::End => panic!("missing independently expected sample"),
        }
    }
    panic!("bounded source sample lookup stalled");
}

fn exercise(group: &str, expected_cases: usize) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/selected-fields18");
    let text = fs::read_to_string(root.join(format!("{group}.txt"))).unwrap();
    let mut cases = 0;
    for block in text.split("\n\n") {
        let Some(header) = block
            .lines()
            .find(|line| line.starts_with("stbl ") || line.starts_with("traf "))
        else {
            continue;
        };
        cases += 1;
        let fields: Vec<_> = header.split_whitespace().collect();
        let kind = fields[0];
        let name = fields[1];
        let expected = fields[2];
        let count: u64 = fields[3].parse().unwrap();
        let caller = if fields[4] == "null" {
            None
        } else {
            Some(fields[4].parse::<i128>().unwrap())
        };
        let bytes = fs::read(root.join(name)).unwrap();
        let rows: Vec<Vec<String>> = block
            .lines()
            .filter_map(|line| line.strip_prefix("S "))
            .map(|line| line.split_whitespace().map(str::to_owned).collect())
            .collect();
        for quantum in [1, 7] {
            let budget = Budget::new(2 * 1024 * 1024).unwrap();
            let result = build(kind, &bytes, caller, &budget, quantum);
            if expected != "ready" {
                let error = result.expect_err("bounded source refusal expected");
                let matches = match expected {
                    "unsupported" => matches!(error, Error::Unsupported(_)),
                    "truncated" => error == Error::Truncated,
                    "overflow" => error == Error::Overflow,
                    "invalid" => matches!(error, Error::Invalid(_)),
                    _ => panic!("unknown authored outcome"),
                };
                assert!(
                    matches,
                    "{name} {quantum}: expected {expected}, got {error:?}"
                );
                continue;
            }
            let index =
                result.unwrap_or_else(|error| panic!("required {name} {quantum}: {error:?}"));
            let raw = if let Some(table) = &index.table {
                table.raw()
            } else {
                index.fragment.as_ref().unwrap().raw()
            };
            assert_eq!(
                raw.bytes().as_ref(),
                bytes,
                "raw metadata must remain intact"
            );
            let actual_count = if let Some(table) = &index.table {
                table.sample_count()
            } else {
                index.fragment.as_ref().unwrap().sample_count()
            };
            assert_eq!(actual_count, count);
            for wanted in &rows {
                let ordinal = wanted[0].parse().unwrap();
                let actual = if let Some(table) = &index.table {
                    let mut locate = table.locate(ordinal).unwrap();
                    sample(|work| locate.poll(work), quantum)
                } else {
                    let fragment = index.fragment.as_ref().unwrap();
                    let mut locate = fragment.locate(ordinal).unwrap();
                    sample(|work| locate.poll(work), quantum)
                };
                assert_eq!(sample_row(&actual), *wanted, "{name} {quantum}");
            }
            // The UINT32_MAX default-run vector probes first/last by locator,
            // never expands or iterates billions of samples.
            if count <= u64::try_from(rows.len()).unwrap() {
                if let Some(table) = &index.table {
                    let mut cursor = table.cursor();
                    for wanted in &rows {
                        assert_eq!(
                            sample_row(&sample(|work| cursor.poll(work), quantum)),
                            *wanted,
                            "{name}"
                        );
                    }
                    assert_eq!(
                        cursor.poll(&mut WorkBudget::new(quantum)).unwrap(),
                        SampleStep::End
                    );
                } else {
                    let fragment = index.fragment.as_ref().unwrap();
                    let mut cursor = fragment.cursor();
                    for wanted in &rows {
                        assert_eq!(
                            sample_row(&sample(|work| cursor.poll(work), quantum)),
                            *wanted,
                            "{name}"
                        );
                    }
                    assert_eq!(
                        cursor.poll(&mut WorkBudget::new(quantum)).unwrap(),
                        SampleStep::End
                    );
                }
            }
        }
    }
    assert_eq!(cases, expected_cases);
}

#[test]
fn required_stz2_widths_and_opaque_odd_nibbles_preserve_source_ranges() {
    exercise("sizes", 26);
}

#[test]
fn required_signed_trun_fields_preserve_version_bits_and_composed_clocks() {
    exercise("trun", 10);
}

#[test]
fn selected_field_unknown_and_truncated_inputs_have_bounded_explicit_outcomes() {
    exercise("refusals", 24);
}
