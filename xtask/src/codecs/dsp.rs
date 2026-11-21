//! Numeric IMDCT comparison against the public transform and the cosine definition.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::{comparison, linux, report, validation, worker};
use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

const FEATURE: &str = "harness/features/dsp-imdct";
const RULES: &str = "harness/codecs/numeric-rules.json";
const IDENTITY: &str = "target/harness/identity.json";
const CANDIDATE: &str = "codec-views-worker";
const REFERENCE: &str = "ffmpeg-codec-views-native";

fn lengths() -> impl Iterator<Item = usize> {
    (4_usize..=8192).step_by(4).filter(|n| {
        let mut quarter = n / 4;
        for radix in [2, 3, 5] {
            while quarter.is_multiple_of(radix) {
                quarter /= radix;
            }
        }
        quarter == 1
    })
}

fn spec(n: usize, normalized: bool, pattern: &str) -> Vec<u8> {
    let scale = if normalized { 1.0 / n as f64 } else { 1.0 };
    let mut bytes = (n as u32).to_le_bytes().to_vec();
    bytes.extend(scale.to_le_bytes());
    let mut seed = 77_u32;
    for i in 0..n / 2 {
        let value = match pattern {
            "seeded" => {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                f32::from((seed >> 16) as u16) / 32768.0 - 1.0
            }
            "first" => f32::from(i == 0),
            _ => f32::from(i == n / 2 - 1),
        };
        bytes.extend(value.to_le_bytes());
    }
    bytes
}

struct Case {
    id: String,
    n: usize,
    specs: Vec<(String, Vec<u8>)>,
}

fn cases() -> Vec<Case> {
    lengths()
        .map(|n| Case {
            id: format!("n-{n:04}"),
            n,
            specs: [false, true]
                .into_iter()
                .flat_map(|normalized| {
                    ["seeded", "first", "last"].into_iter().map(move |pattern| {
                        let scale = if normalized { "scaled" } else { "unit" };
                        (format!("{scale}-{pattern}"), spec(n, normalized, pattern))
                    })
                })
                .collect(),
        })
        .collect()
}

fn listing(cases: &[Case]) -> Value {
    Value::object([
        ("schema", Value::int(1)),
        (
            "cases",
            Value::Array(
                cases
                    .iter()
                    .map(|case| {
                        Value::object([
                            ("id", Value::str(&case.id)),
                            ("profile", Value::str("imdct")),
                            ("length", Value::int(case.n as u64)),
                            (
                                "specs",
                                Value::object(case.specs.iter().map(|(name, bytes)| {
                                    (name.clone(), Value::str(sha256::hex(bytes)))
                                })),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn normalized_output(center: &[f64]) -> Vec<f64> {
    let (first, last) = center.split_at(center.len() / 2);
    first
        .iter()
        .rev()
        .copied()
        .chain(center.iter().map(|value| -value))
        .chain(last.iter().rev().map(|value| -value))
        .chain(center.iter().rev().copied())
        .collect()
}

fn floats(path: &Path, count: usize) -> R<Vec<f64>> {
    let data = read(path)?;
    if data.len() != count * 4 {
        return Err(format!("{}: expected {count} f32 samples", path.display()));
    }
    Ok(data
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(|bytes| f64::from(f32::from_le_bytes(bytes)))
        .collect())
}

fn read(path: &Path) -> R<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn run(program: &Path, input: &Path, output: &Path) -> R<i64> {
    let args = [
        program.into(),
        "dsp-raw".into(),
        input.into(),
        output.into(),
    ];
    let result = worker::run(&args, &[], Duration::from_secs(15))?
        .ok_or_else(|| format!("{} timed out on {}", program.display(), input.display()))?;
    if matches!(result.code, 0 | 3) {
        return Ok(result.code);
    }
    Err(format!(
        "{} failed ({}): {}",
        program.display(),
        result.code,
        String::from_utf8_lossy(&result.stderr)
    ))
}

struct Deviation {
    score: f64,
    index: usize,
    candidate: Option<f64>,
    reference: Option<f64>,
}

fn worst(candidate: &[f64], reference: &[f64], factor: f64, start: usize) -> Deviation {
    candidate
        .iter()
        .zip(reference)
        .enumerate()
        .map(|(i, (&a, &b))| {
            let finite = a.is_finite() && b.is_finite();
            let diff = if finite {
                (a - b).abs() / factor
            } else {
                f64::INFINITY
            };
            Deviation {
                score: diff,
                index: start + i,
                candidate: a.is_finite().then_some(a),
                reference: b.is_finite().then_some(b),
            }
        })
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .expect("nonempty transform")
}

impl Deviation {
    fn receipt(&self) -> Value {
        let number =
            |value: Option<f64>| value.map_or(Value::Null, |value| Value::str(value.to_string()));
        Value::object([
            ("normalized_error", Value::str(self.score.to_string())),
            ("sample_index", Value::int(self.index as u64)),
            ("candidate", number(self.candidate)),
            ("expected", number(self.reference)),
        ])
    }
}

/// Both sides use identical row labels and binary32 sample counts. Window
/// coefficients are nonnegative, so their bit words are ordered by value.
pub(crate) fn windows_match(actual: &[u8], expected: &[u8]) -> bool {
    fn row(line: &str) -> Option<(&str, usize, &str)> {
        let (name, bits) = line.rsplit_once(' ')?;
        let length = name.split_ascii_whitespace().nth(1)?.parse().ok()?;
        Some((name, length, bits))
    }
    fn f32_bits(bytes: &[u8]) -> Option<u32> {
        u32::from_str_radix(std::str::from_utf8(bytes).ok()?, 16)
            .ok()
            .filter(|word| *word <= 1.0_f32.to_bits() + 1)
    }
    let (Ok(actual), Ok(expected)) = (std::str::from_utf8(actual), std::str::from_utf8(expected))
    else {
        return false;
    };
    let mut actual = actual.lines();
    for expected in expected.lines() {
        let Some((label, len, want)) = row(expected) else {
            return false;
        };
        let Some((name, _, got)) = actual.next().and_then(row) else {
            return false;
        };
        if name != label || got.len() != len * 8 || want.len() != got.len() {
            return false;
        }
        for (sample, expected) in got
            .as_bytes()
            .chunks_exact(8)
            .zip(want.as_bytes().chunks_exact(8))
        {
            if !matches!((f32_bits(sample), f32_bits(expected)), (Some(a), Some(b)) if a.abs_diff(b) <= 1)
            {
                return false;
            }
        }
    }
    actual.next().is_none()
}

struct TransformComparison<'a> {
    maximum: f64,
    root: &'a Path,
    candidate: PathBuf,
    reference: PathBuf,
    output: &'a Path,
}

impl TransformComparison<'_> {
    fn case(&self, case: &Case) -> R<(Value, bool, bool)> {
        let directory = self.output.join(&case.id);
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let mut details = Vec::new();
        let mut definition_ok = true;
        let mut reference_ok = true;
        let mut refused = 0;
        let n = case.n;
        let mut definition_command = vec![
            crate::python::interpreter().into(),
            "-m".into(),
            "harness.codec_dsp_oracle".into(),
        ];
        for (name, bytes) in &case.specs {
            let input = directory.join(format!("{name}.spec"));
            fs::write(&input, bytes).map_err(|error| error.to_string())?;
            definition_command.push(input.into());
        }
        let result = worker::run_in(
            &definition_command,
            &[],
            Some(self.root),
            Duration::from_secs(90),
        )?
        .ok_or("cosine definition timed out")?;
        if result.code != 0 {
            return Err(String::from_utf8_lossy(&result.stderr).into_owned());
        }
        for (name, bytes) in &case.specs {
            let input = directory.join(format!("{name}.spec"));
            let actual = directory.join(format!("{name}.candidate"));
            let reference = directory.join(format!("{name}.reference"));
            if run(&self.candidate, &input, &actual)? != 0 {
                return Err(format!("candidate refused {} {name}", case.id));
            }
            let actual_samples = floats(&actual, n + n / 2)?;
            let definition = read(&input.with_extension("f64"))?;
            if definition.len() != (n + n / 2) * 8 {
                return Err(format!("incomplete cosine output for {} {name}", case.id));
            }
            let definition_samples: Vec<_> = definition
                .as_chunks::<8>()
                .0
                .iter()
                .copied()
                .map(f64::from_le_bytes)
                .collect();
            let peak = bytes[12..]
                .as_chunks::<4>()
                .0
                .iter()
                .copied()
                .map(|word| f64::from(f32::from_le_bytes(word)).abs())
                .fold(0.0_f64, f64::max);
            let scale = f64::from_le_bytes(bytes[4..12].try_into().map_err(|_| "missing scale")?);
            let factor = peak * n as f64 * scale.abs();
            let check = |expected: &[f64]| {
                let imdct = worst(&actual_samples[..n], &expected[..n], factor, 0);
                let dct4 = worst(&actual_samples[n..], &expected[n..], factor, 0);
                (
                    imdct.score <= self.maximum && dct4.score <= self.maximum,
                    Value::object([("imdct", imdct.receipt()), ("dct4", dct4.receipt())]),
                )
            };
            let (passed, model) = check(&definition_samples);
            definition_ok &= passed;
            let reference_result = if run(&self.reference, &input, &reference)? == 3 {
                refused += 1;
                Value::str("reference-refused")
            } else {
                let reference_samples = normalized_output(&floats(&reference, n / 2)?);
                let (passed, row) = check(&reference_samples);
                reference_ok &= passed;
                row
            };
            details.push(Value::object([
                ("spec", Value::str(name)),
                ("input_sha256", Value::str(sha256::hex(bytes))),
                ("definition", model),
                ("reference", reference_result),
            ]));
        }
        let status = if reference_ok && definition_ok {
            "pass"
        } else {
            "mismatch"
        };
        Ok((
            Value::object([
                ("id", Value::str(&case.id)),
                ("parity", Value::object([("status", Value::str(status))])),
                ("oracle_only_specs", Value::int(refused)),
                ("specs", Value::Array(details)),
            ]),
            definition_ok,
            reference_ok,
        ))
    }
}

pub(super) fn main(root: &Path, output: &Path, record: bool) -> R<i32> {
    let cases = cases();
    let manifest = root.join(FEATURE).join("corpus.json");
    let generated = listing(&cases);
    if record {
        json::write(&manifest, &generated)?;
        return Ok(0);
    }
    if json::read(&manifest)? != generated {
        return Err("IMDCT cases differ from the pinned manifest".into());
    }
    let contract = json::read(&root.join(FEATURE).join("contract.json"))?;
    let target = report::gate_target()?;
    let open = report::open_cases(&contract, &generated, &target)?;
    let rule = json::read(&root.join(RULES))?;
    let maximum = rule
        .get("imdct_float")
        .and_then(|rule| rule.get("max_normalized_error"))
        .and_then(Value::as_f64)
        .filter(|&x| x.is_finite() && x > 0.0)
        .ok_or("missing IMDCT numeric rule")?;
    let identity = json::read(&root.join(IDENTITY))?;
    let candidate = validation::candidate_worker(root, &identity, CANDIDATE)?;
    let ref_record = comparison::reference_named(&identity, REFERENCE)?;
    let reference = root.join(validation::text(&ref_record, "path")?);
    if linux::file_sha(&reference)? != validation::hash(&ref_record, "sha256")? {
        return Err(format!("{REFERENCE} changed since build identity; rebuild"));
    }

    report::create_output(output)?;
    let output = output.canonicalize().map_err(|error| error.to_string())?;
    let comparison = TransformComparison {
        maximum,
        root,
        candidate,
        reference,
        output: &output,
    };
    let mut rows = Vec::new();
    let mut definitions_ok = true;
    for case in &cases {
        let (row, definition, reference) = comparison.case(case)?;
        println!(
            "{} definition={} reference={}",
            case.id, definition, reference
        );
        definitions_ok &= definition;
        rows.push(row);
    }
    let reference_ok = report::ratchet("dsp-imdct", &rows, &open);
    let stable =
        linux::file_sha(&comparison.reference)? == validation::hash(&ref_record, "sha256")?;
    let _candidate = validation::candidate_worker(root, &identity, CANDIDATE)?;
    let passed = definitions_ok && reference_ok && stable;
    json::write(
        &output.join("parity.json"),
        &Value::object([
            ("schema", Value::int(1)),
            ("target", Value::str(target)),
            ("accepted", Value::Bool(passed)),
            (
                "rules_sha256",
                Value::str(linux::file_sha(&root.join(RULES))?),
            ),
            ("manifest_sha256", Value::str(linux::file_sha(&manifest)?)),
            (
                "candidate_sha256",
                Value::str(linux::file_sha(&comparison.candidate)?),
            ),
            (
                "reference_sha256",
                Value::str(validation::hash(&ref_record, "sha256")?),
            ),
            ("rows", Value::Array(rows)),
        ]),
    )?;
    Ok(i32::from(!passed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_rule_accepts_one_ulp_and_requires_every_ordered_sample() {
        let expected = b"3 2 9 3f0000003f7fffff\n";
        assert!(windows_match(b"3 2 9 3effffff3f800000\n", expected));
        assert!(!windows_match(b"3 2 9 3efffffe3f800000\n", expected));
        assert!(!windows_match(b"3 2 9 3f000000\n", expected));
        assert!(!windows_match(b"3 2 8 3f0000003f7fffff\n", expected));
        assert!(!windows_match(b"3 2 9 3f7fffff3f000000\n", expected));
    }
}
