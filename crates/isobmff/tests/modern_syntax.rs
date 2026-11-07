// Tiny wire fixtures and hand-authored facts, checked by an independent
// Python wire oracle. Full brand/codec/presentation conformance and demux
// parity are not covered here.
mod common;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Limits,
    SampleStep, SampleTable, SampleTableBuilder, Trex,
};

#[derive(Clone, Copy, Debug)]
enum Failure {
    Invalid,
    Unsupported,
    Overflow,
}
fn assert_failure(error: Error, expected: Failure, name: &str) {
    assert!(
        matches!(
            (error, expected),
            (Error::Invalid(_), Failure::Invalid)
                | (Error::Unsupported(_), Failure::Unsupported)
                | (Error::Overflow, Failure::Overflow)
        ),
        "{name}: {error:?}, expected {expected:?}"
    );
}
struct ExpectedFragment {
    ordinal: u64,
    range: (u64, u64),
    dts: Option<i128>,
    cts: Option<i128>,
    duration: u32,
}
struct FragmentCase {
    name: &'static str,
    raw: &'static [u8],
    moof: u64,
    previous: Option<u64>,
    origin: Option<i128>,
    size: u32,
    duration: u32,
    expected: Result<&'static [ExpectedFragment], Failure>,
    tfdt: Option<(u8, u64)>,
}
struct ExpectedComposition {
    ordinal: u64,
    version: u8,
    raw_bits: u32,
    offset: i128,
    dts: i128,
    cts: i128,
}
struct CompositionCase {
    name: &'static str,
    raw: &'static [u8],
    sample_count: u32,
    expected: Result<&'static [ExpectedComposition], Failure>,
}

const FRAGMENTS: &[FragmentCase] = &[
    FragmentCase {
        name: "moof-flag-first",
        raw: include_bytes!("fixtures/modern-syntax/moof-flag-first.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "moof-flag-does-not-inherit-previous",
        raw: include_bytes!("fixtures/modern-syntax/moof-flag-does-not-inherit-previous.bin"),
        moof: 4294967296u64,
        previous: Some(1099511627776u64),
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "explicit-base-overrides-moof-condition",
        raw: include_bytes!("fixtures/modern-syntax/explicit-base-overrides-moof-condition.bin"),
        moof: 4294967296u64,
        previous: Some(55u64),
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (1099511627792u64, 1099511627796u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "legacy-base-still-inherits",
        raw: include_bytes!("fixtures/modern-syntax/legacy-base-still-inherits.bin"),
        moof: 4294967296u64,
        previous: Some(1000u64),
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (1016u64, 1020u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "moof-negative-run-offset",
        raw: include_bytes!("fixtures/modern-syntax/moof-negative-run-offset.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967280u64, 4294967284u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "negative-run-offset-underflows",
        raw: include_bytes!("fixtures/modern-syntax/negative-run-offset-underflows.bin"),
        moof: 15u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Overflow),
        tfdt: None,
    },
    FragmentCase {
        name: "sample-end-overflows-u64",
        raw: include_bytes!("fixtures/modern-syntax/sample-end-overflows-u64.bin"),
        moof: 18446744073709551612u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Overflow),
        tfdt: None,
    },
    FragmentCase {
        name: "sample-end-exact-u64-max",
        raw: include_bytes!("fixtures/modern-syntax/sample-end-exact-u64-max.bin"),
        moof: 18446744073709551611u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (18446744073709551611u64, 18446744073709551615u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "second-run-omits-offset",
        raw: include_bytes!("fixtures/modern-syntax/second-run-omits-offset.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[
            ExpectedFragment {
                ordinal: 1,
                range: (4294967312u64, 4294967316u64),
                dts: Some(7i128),
                cts: Some(7i128),
                duration: 2,
            },
            ExpectedFragment {
                ordinal: 2,
                range: (4294967316u64, 4294967320u64),
                dts: Some(9i128),
                cts: Some(9i128),
                duration: 2,
            },
        ]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "absent-tfdt-keeps-clock-unknown",
        raw: include_bytes!("fixtures/modern-syntax/absent-tfdt-keeps-clock-unknown.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: None,
            cts: None,
            duration: 2,
        }]),
        tfdt: None,
    },
    FragmentCase {
        name: "absent-tfdt-explicit-caller-origin",
        raw: include_bytes!("fixtures/modern-syntax/absent-tfdt-explicit-caller-origin.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: Some(-9i128),
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: Some(-9i128),
            cts: Some(-9i128),
            duration: 2,
        }]),
        tfdt: None,
    },
    FragmentCase {
        name: "tfdt-v0-unsigned-max",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-v0-unsigned-max.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: Some(4294967295i128),
            cts: Some(4294967295i128),
            duration: 2,
        }]),
        tfdt: Some((0, 4294967295u64)),
    },
    FragmentCase {
        name: "tfdt-v1-above-i64",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-v1-above-i64.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: Some(9223372036854775815i128),
            cts: Some(9223372036854775815i128),
            duration: 2,
        }]),
        tfdt: Some((1, 9223372036854775815u64)),
    },
    FragmentCase {
        name: "tfdt-v1-max-and-wide-decode-end",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-v1-max-and-wide-decode-end.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Ok(&[
            ExpectedFragment {
                ordinal: 1,
                range: (4294967312u64, 4294967316u64),
                dts: Some(18446744073709551615i128),
                cts: Some(18446744073709551615i128),
                duration: 2,
            },
            ExpectedFragment {
                ordinal: 2,
                range: (4294967316u64, 4294967320u64),
                dts: Some(18446744073709551617i128),
                cts: Some(18446744073709551617i128),
                duration: 2,
            },
        ]),
        tfdt: Some((1, 18446744073709551615u64)),
    },
    FragmentCase {
        name: "tfdt-matching-caller-origin",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-matching-caller-origin.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: Some(7i128),
        size: 4,
        duration: 2,
        expected: Ok(&[ExpectedFragment {
            ordinal: 1,
            range: (4294967312u64, 4294967316u64),
            dts: Some(7i128),
            cts: Some(7i128),
            duration: 2,
        }]),
        tfdt: Some((1, 7u64)),
    },
    FragmentCase {
        name: "tfdt-disagrees-with-caller-origin",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-disagrees-with-caller-origin.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: Some(8i128),
        size: 4,
        duration: 2,
        expected: Err(Failure::Invalid),
        tfdt: None,
    },
    FragmentCase {
        name: "tfdt-v0-extra-byte",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-v0-extra-byte.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Invalid),
        tfdt: None,
    },
    FragmentCase {
        name: "tfdt-unknown-version",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-unknown-version.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Unsupported),
        tfdt: None,
    },
    FragmentCase {
        name: "tfdt-unknown-flags",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-unknown-flags.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Unsupported),
        tfdt: None,
    },
    FragmentCase {
        name: "tfdt-duplicate",
        raw: include_bytes!("fixtures/modern-syntax/tfdt-duplicate.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Invalid),
        tfdt: None,
    },
    FragmentCase {
        name: "tfhd-reserved-bit-stays-refused",
        raw: include_bytes!("fixtures/modern-syntax/tfhd-reserved-bit-stays-refused.bin"),
        moof: 4294967296u64,
        previous: None,
        origin: None,
        size: 4,
        duration: 2,
        expected: Err(Failure::Unsupported),
        tfdt: None,
    },
];

const COMPOSITION: &[CompositionCase] = &[
    CompositionCase {
        name: "ctts-v0-high-bit-is-unsigned",
        raw: include_bytes!("fixtures/modern-syntax/ctts-v0-high-bit-is-unsigned.bin"),
        sample_count: 1u32,
        expected: Ok(&[ExpectedComposition {
            ordinal: 1u64,
            version: 0,
            raw_bits: 4294967295u32,
            offset: 4294967295i128,
            dts: 0i128,
            cts: 4294967295i128,
        }]),
    },
    CompositionCase {
        name: "ctts-v1-signed-min",
        raw: include_bytes!("fixtures/modern-syntax/ctts-v1-signed-min.bin"),
        sample_count: 1u32,
        expected: Ok(&[ExpectedComposition {
            ordinal: 1u64,
            version: 1,
            raw_bits: 2147483648u32,
            offset: -2147483648i128,
            dts: 0i128,
            cts: -2147483648i128,
        }]),
    },
    CompositionCase {
        name: "ctts-v1-signed-max",
        raw: include_bytes!("fixtures/modern-syntax/ctts-v1-signed-max.bin"),
        sample_count: 1u32,
        expected: Ok(&[ExpectedComposition {
            ordinal: 1u64,
            version: 1,
            raw_bits: 2147483647u32,
            offset: 2147483647i128,
            dts: 0i128,
            cts: 2147483647i128,
        }]),
    },
    CompositionCase {
        name: "ctts-v1-explicit-zero",
        raw: include_bytes!("fixtures/modern-syntax/ctts-v1-explicit-zero.bin"),
        sample_count: 1u32,
        expected: Ok(&[ExpectedComposition {
            ordinal: 1u64,
            version: 1,
            raw_bits: 0u32,
            offset: 0i128,
            dts: 0i128,
            cts: 0i128,
        }]),
    },
    CompositionCase {
        name: "ctts-v1-compact-u32-max-run",
        raw: include_bytes!("fixtures/modern-syntax/ctts-v1-compact-u32-max-run.bin"),
        sample_count: 4294967295u32,
        expected: Ok(&[
            ExpectedComposition {
                ordinal: 1u64,
                version: 1,
                raw_bits: 4294967295u32,
                offset: -1i128,
                dts: 0i128,
                cts: -1i128,
            },
            ExpectedComposition {
                ordinal: 4294967295u64,
                version: 1,
                raw_bits: 4294967295u32,
                offset: -1i128,
                dts: 8589934588i128,
                cts: 8589934587i128,
            },
        ]),
    },
    CompositionCase {
        name: "ctts-unknown-version",
        raw: include_bytes!("fixtures/modern-syntax/ctts-unknown-version.bin"),
        sample_count: 1u32,
        expected: Err(Failure::Unsupported),
    },
    CompositionCase {
        name: "ctts-unknown-flags",
        raw: include_bytes!("fixtures/modern-syntax/ctts-unknown-flags.bin"),
        sample_count: 1u32,
        expected: Err(Failure::Unsupported),
    },
    CompositionCase {
        name: "ctts-extra-byte",
        raw: include_bytes!("fixtures/modern-syntax/ctts-extra-byte.bin"),
        sample_count: 1u32,
        expected: Err(Failure::Invalid),
    },
];

fn fragment_index(
    case: &FragmentCase,
    budget: &Budget,
    quantum: u64,
) -> Result<FragmentIndex, Error> {
    let raw = BoxView::new(common::bytes(budget, case.raw), 0)?;
    let mut builder = FragmentBuilder::new(
        raw,
        Trex {
            track_id: 1,
            description_index: 1,
            duration: case.duration,
            size: case.size,
            flags: 0x02000000,
        },
        FragmentContext {
            moof_start: case.moof,
            implicit_base: case
                .previous
                .map_or(ImplicitBase::FirstTraf, ImplicitBase::PreviousTraf),
            decode_origin: case.origin,
            first_ordinal: 1,
            description_count: 1,
        },
        budget.clone(),
        Limits::default(),
    )?;
    assert_eq!(builder.poll(&mut WorkBudget::new(0))?, BuildStatus::Yield);
    for _ in 0..1000 {
        match builder.poll(&mut WorkBudget::new(quantum)) {
            Ok(BuildStatus::Yield) => {}
            Ok(BuildStatus::Ready) => return builder.finish(),
            Err(error) => {
                assert_eq!(builder.poll(&mut WorkBudget::new(quantum)), Err(error));
                return Err(error);
            }
        }
    }
    panic!("bounded fragment build stalled: {}", case.name)
}

#[test]
fn independent_fragment_boundaries_preserve_source_clocks_and_ranges() {
    for case in FRAGMENTS {
        for quantum in [1, 4096] {
            let budget = Budget::new(64 * 1024).unwrap();
            let baseline = budget.used();
            {
                let outcome = fragment_index(case, &budget, quantum);
                match (&case.expected, outcome) {
                    (Err(wanted), Err(error)) => assert_failure(error, *wanted, case.name),
                    (Ok(wanted), Ok(index)) => {
                        assert_eq!(index.raw().bytes().as_ref(), case.raw, "{}", case.name);
                        assert_eq!(
                            index.decode_time().map(|t| (t.version(), t.ticks())),
                            case.tfdt
                        );
                        assert_eq!(index.sample_count(), wanted.len() as u64);
                        let mut cursor = index.cursor();
                        for expected in *wanted {
                            let mut found = None;
                            for _ in 0..1000 {
                                match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                                    SampleStep::Yield => {}
                                    SampleStep::Sample(value) => {
                                        found = Some(value);
                                        break;
                                    }
                                    SampleStep::End => {
                                        panic!("missing boundary sample: {}", case.name)
                                    }
                                }
                            }
                            let sample = found.expect("bounded fragment cursor stalled");
                            assert_eq!(sample.ordinal, expected.ordinal);
                            assert_eq!(
                                sample.range,
                                expected.range.0..expected.range.1,
                                "{}",
                                case.name
                            );
                            assert_eq!(sample.dts_ticks, expected.dts);
                            assert_eq!(sample.cts_ticks, expected.cts);
                            assert_eq!(sample.duration_ticks, expected.duration);
                            assert_eq!(sample.composition_offset, None);
                            let mut locator = index.locate(sample.ordinal).unwrap();
                            let mut located = None;
                            for _ in 0..1000 {
                                match locator.poll(&mut WorkBudget::new(quantum)).unwrap() {
                                    SampleStep::Yield => {}
                                    SampleStep::Sample(value) => {
                                        located = Some(value);
                                        break;
                                    }
                                    SampleStep::End => panic!("missing located fragment sample"),
                                }
                            }
                            assert_eq!(located, Some(sample));
                        }
                        let mut ended = false;
                        for _ in 0..1000 {
                            match cursor.poll(&mut WorkBudget::new(quantum)).unwrap() {
                                SampleStep::Yield => {}
                                SampleStep::End => {
                                    ended = true;
                                    break;
                                }
                                SampleStep::Sample(_) => panic!("extra fragment sample"),
                            }
                        }
                        assert!(ended, "bounded fragment cursor failed to finish");
                        let last = wanted.last().unwrap();
                        assert_eq!(index.data_end(), last.range.1);
                        assert_eq!(
                            index.decode_end(),
                            last.dts.map(|d| d + i128::from(last.duration))
                        );
                    }
                    (_, actual) => panic!("{}: unexpected boundary result {actual:?}", case.name),
                }
            }
            assert_eq!(
                budget.used(),
                baseline,
                "retained fragment charge: {}",
                case.name
            );
        }
    }
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut result = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    result.extend_from_slice(kind);
    result.extend_from_slice(body);
    result
}
fn full_words(kind: &[u8; 4], version: u8, words: &[u32]) -> Vec<u8> {
    let mut result = vec![version, 0, 0, 0];
    for word in words {
        result.extend_from_slice(&word.to_be_bytes());
    }
    atom(kind, &result)
}
fn composition_table(
    case: &CompositionCase,
    budget: &Budget,
    quantum: u64,
    stts_version: u8,
) -> Result<SampleTable, Error> {
    // One opaque original sample entry, constant-size samples, one sparse co64
    // chunk and one compact timing row. No per-sample fixture or index expansion.
    let entry = atom(b"test", &[0, 0, 0, 0, 0, 0, 0, 1]);
    let mut descriptions = vec![0, 0, 0, 0, 0, 0, 0, 1];
    descriptions.extend_from_slice(&entry);
    let mut body = atom(b"stsd", &descriptions);
    body.extend_from_slice(&full_words(
        b"stts",
        stts_version,
        &[1, case.sample_count, 2],
    ));
    body.extend_from_slice(case.raw);
    body.extend_from_slice(&full_words(b"stsc", 0, &[1, 1, case.sample_count, 1]));
    body.extend_from_slice(&full_words(b"stsz", 0, &[4, case.sample_count]));
    body.extend_from_slice(&full_words(b"co64", 0, &[1, 1, 0]));
    let raw = BoxView::new(common::bytes(budget, &atom(b"stbl", &body)), 0)?;
    let mut builder = SampleTableBuilder::new(
        raw,
        budget.clone(),
        Limits {
            max_boxes: 32,
            max_records: 1,
            max_descriptions: 1,
        },
    )?;
    assert_eq!(builder.poll(&mut WorkBudget::new(0))?, BuildStatus::Yield);
    for _ in 0..1000 {
        match builder.poll(&mut WorkBudget::new(quantum)) {
            Ok(BuildStatus::Yield) => {}
            Ok(BuildStatus::Ready) => return builder.finish(),
            Err(error) => {
                assert_eq!(builder.poll(&mut WorkBudget::new(quantum)), Err(error));
                return Err(error);
            }
        }
    }
    panic!("bounded composition build stalled: {}", case.name)
}

#[test]
fn independent_ctts_boundaries_preserve_sign_bits_and_compact_runs() {
    for case in COMPOSITION {
        for quantum in [1, 4096] {
            let budget = Budget::new(64 * 1024).unwrap();
            let baseline = budget.used();
            {
                let result = composition_table(case, &budget, quantum, 0);
                match (&case.expected, result) {
                    (Err(wanted), Err(error)) => assert_failure(error, *wanted, case.name),
                    (Ok(wanted), Ok(table)) => {
                        assert_eq!(table.sample_count(), u64::from(case.sample_count));
                        assert!(
                            budget.used() < 16 * 1024,
                            "compact run expanded into samples"
                        );
                        for expected in *wanted {
                            let mut locator = table.locate(expected.ordinal).unwrap();
                            let mut found = None;
                            for _ in 0..1000 {
                                match locator.poll(&mut WorkBudget::new(quantum)).unwrap() {
                                    SampleStep::Yield => {}
                                    SampleStep::Sample(value) => {
                                        found = Some(value);
                                        break;
                                    }
                                    SampleStep::End => panic!("missing located composition sample"),
                                }
                            }
                            let sample = found.expect("bounded composition locator stalled");
                            let wire = sample.composition_offset.unwrap();
                            assert_eq!(sample.ordinal, expected.ordinal);
                            assert_eq!(wire.version(), expected.version);
                            assert_eq!(wire.raw_bits(), expected.raw_bits);
                            assert_eq!(wire.ticks(), expected.offset);
                            assert_eq!(
                                wire.signed_value(),
                                (expected.version == 1)
                                    .then(|| i32::try_from(expected.offset).unwrap())
                            );
                            assert_eq!(sample.dts_ticks, Some(expected.dts));
                            assert_eq!(sample.cts_ticks, Some(expected.cts));
                            let start = (1u64 << 32) + (expected.ordinal - 1) * 4;
                            assert_eq!(sample.range, start..start + 4);
                            if case.sample_count == 1 {
                                assert_eq!(
                                    table.cursor().poll(&mut WorkBudget::new(quantum)).unwrap(),
                                    SampleStep::Sample(sample)
                                );
                            }
                        }
                    }
                    (_, actual) => {
                        panic!("{}: unexpected composition result {actual:?}", case.name)
                    }
                }
            }
            assert_eq!(
                budget.used(),
                baseline,
                "retained table charge: {}",
                case.name
            );
        }
    }
}

#[test]
fn admitting_signed_ctts_does_not_admit_version_one_stts() {
    let case = COMPOSITION
        .iter()
        .find(|c| c.name == "ctts-v1-explicit-zero")
        .unwrap();
    let budget = Budget::new(64 * 1024).unwrap();
    assert!(matches!(
        composition_table(case, &budget, 1, 1),
        Err(Error::Unsupported(_))
    ));
}
