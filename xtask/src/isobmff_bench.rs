//! ISO BMFF open and native-index comparisons over the complete authored corpus.
//!
//! Every label gets its own ordered trace comparison. Only successful operations
//! are measured; explicit aliases share measurements after their traces match.
//! The 52-operation selection changes measurement membership only.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::checks::read_json;
use crate::codecs::correctness::file_sha;
use crate::harness::diff::equal;
use crate::harness::timing::{self, Operation};
use crate::harness::worker::{self, Finished};
use crate::harness::{acceptance, build, comparison, gate_build, memory, provenance, report};
use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

pub const USAGE: &str = "usage: cargo xtask isobmff-bench [--benchmark] [--memory]
    [--subset52] [--output DIR] [--pairs N] [--memory-pairs N] [--seed N]
    [--require-acceptance]

Always compare all 1020 traces: 972 output labels and 48 explicit refusals.
The full measurement set has 956 physical operations and 16 explicit aliases.
--subset52 measures the separate 52-operation manifest, retaining full parity.
Workers must belong to the current recorded gate build. Acceptance also requires
the complete runtime and memory sets on native AArch64 Linux. For combined
qualification, keep the RSS kernel module unloaded and
TRIERGON_RSS_KERNEL_BOUND unset or 0, using the conservative RSS observer.";

pub const CANDIDATE: &str = "isobmff-worker";
pub const REFERENCE: &str = "ffmpeg-isobmff-native";
const FEATURE: &str = "harness/features/isobmff-open-index";
const FIXTURES: &str = "harness/fixtures/isobmff-open-index";
const CONTRACT_SHA: &str = "0419a8ea026d84e1aa98d4896faa79ec2c3f2261bd356a6b04a65e22b8d9b760";
const CORPUS_SHA: &str = "03761df0849a8d60a3cc2222e35dd8fe02308c0fe774b3e442b6850d99704e58";
const SUBSET_SHA: &str = "d5efa68f708ffe403018a8c34aaeb10aa693cb4e5abcf9a0b571339c69d40b61";
const WORK_CONTRACT: &str = "0a924396626871e84b2f91ec612a0c60b09258f0f016afe0753cf7c75fdecf8a";
const ITERATIONS: u32 = 50;
const MINIMUM_PAIRS: usize = 30;
const PARITY_CHECKS: usize = 1020;
const TIMABLE_OUTPUTS: usize = 972;
const PHYSICAL_OPERATIONS: usize = 956;
const EXPLICIT_REFUSALS: usize = 48;
const REFERENCE_REFUSAL: i128 = -1_094_995_529;

fn text<'a>(value: &'a Value, key: &str) -> R<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}"))
}

fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::Number(number) => number.as_i128(),
        _ => None,
    }
}

fn number(value: &Value, key: &str) -> R<i128> {
    value
        .get(key)
        .and_then(integer)
        .ok_or_else(|| format!("missing integer {key}"))
}

fn strings(value: &Value, key: &str) -> R<Vec<String>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array {key}"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("non-string {key}"))
        })
        .collect()
}

fn field(value: &mut Value, key: &str, data: Value) {
    if let Value::Object(fields) = value {
        if let Some((_, slot)) = fields.iter_mut().find(|(k, _)| k == key) {
            *slot = data;
        } else {
            fields.push((key.to_owned(), data));
        }
    }
}

fn counts() -> Value {
    Value::object([
        ("input_labels", Value::int(255)),
        ("distinct_inputs", Value::int(251)),
        ("input_bytes", Value::int(254_641)),
        ("maximum_input_bytes", Value::int(1859)),
        ("parity_checks", Value::int(PARITY_CHECKS as i128)),
        ("timable_outputs", Value::int(TIMABLE_OUTPUTS as i128)),
        (
            "physical_operations",
            Value::int(PHYSICAL_OPERATIONS as i128),
        ),
        ("aliases", Value::int(16)),
        ("explicit_refusals", Value::int(EXPLICIT_REFUSALS as i128)),
    ])
}

fn document(root: &Path, name: &str, expected: &str) -> R<Value> {
    let path = format!("{FEATURE}/{name}.json");
    let (_, actual) = provenance::file_identity(root, Path::new(&path))?;
    if actual != expected {
        return Err(format!("{path}: contract or membership changed"));
    }
    read_json(&root.join(path))
}

#[derive(Clone)]
struct Case {
    index: usize,
    label: String,
    schedule: String,
    mode: String,
    input: String,
    input_sha: String,
    input_bytes: usize,
    refusal: bool,
    arguments: Vec<String>,
    candidate_options: Vec<String>,
    alias: Option<usize>,
    time_outputs: Value,
    memory_outputs: Value,
}

fn compatibility_options(label: &str) -> Vec<String> {
    if matches!(
        label,
        "bad-zero-timescale" | "bad-stsc-description-zero" | "bad-chunk-offset-outside-source"
    ) {
        vec!["--recover-regular-tables".into()]
    } else {
        [
            "--bounded-timing",
            "--bounded-sync",
            "--terminal-media-prefix",
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

impl Case {
    fn parse(value: &Value, index: usize) -> R<Self> {
        if number(value, "index")? != index as i128 {
            return Err("case indices must be complete and ordered".into());
        }
        let input = value.get("input").ok_or("missing input")?;
        let alias = match value.get("alias_of") {
            Some(Value::Null) => None,
            Some(v) => Some(
                usize::try_from(integer(v).ok_or("invalid alias")?).map_err(|e| e.to_string())?,
            ),
            None => return Err("missing alias disposition".into()),
        };
        let refusal = match text(value, "outcome")? {
            "output" => false,
            "refusal" => true,
            _ => return Err("unknown outcome".into()),
        };
        if number(value, "reference_exit_code_in_trace")? != i128::from(refusal) {
            return Err("inconsistent refusal exit code".into());
        }
        let case = Self {
            index,
            label: text(value, "case")?.into(),
            schedule: text(value, "schedule")?.into(),
            mode: text(value, "mode")?.into(),
            input: text(input, "path")?.into(),
            input_sha: text(input, "sha256")?.into(),
            input_bytes: usize::try_from(number(input, "bytes")?).map_err(|e| e.to_string())?,
            refusal,
            arguments: strings(value, "arguments")?,
            candidate_options: strings(value, "candidate_leading_arguments")?,
            alias,
            time_outputs: value
                .get("expected_time_outputs")
                .cloned()
                .ok_or("missing time work")?,
            memory_outputs: value
                .get("expected_memory_outputs")
                .cloned()
                .ok_or("missing memory work")?,
        };
        let expected_arguments = match case.schedule.as_str() {
            "ordinary" => ["65536", "0", "memory", "4096"],
            "tiny" => ["1", "0", "memory", "3"],
            _ => return Err("unknown schedule".into()),
        };
        if !matches!(case.mode.as_str(), "open" | "native-index")
            || case.arguments != expected_arguments
            || case.candidate_options != compatibility_options(&case.label)
            || case.input_sha.len() != 64
            || !case
                .input_sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || case.input != format!("{}.mov", case.input_sha)
            || case.input_bytes == 0
            || case.input_bytes > 1859
        {
            return Err(format!("invalid operation at {index}"));
        }
        if text(value, "id")? != case.id()
            || text(value, "profile")? != case.id()
            || text(value, "sha256")? != case.input_sha
            || text(value, "file")? != case.input
            || value.get("operation") != Some(&case.operation_record())
        {
            return Err(format!("inconsistent registered case at {index}"));
        }
        for (timed, iterations) in [(true, ITERATIONS), (false, 1)] {
            let work = case.work(false, timed);
            if number(&work, "schema")? != 1
                || number(&work, "elapsed_ns")? != 0
                || number(&work, "iterations")? != i128::from(iterations)
                || text(&work, "input_sha256")? != case.input_sha
                || number(&work, "input_bytes")? != case.input_bytes as i128
                || text(&work, "contract_sha256")? != WORK_CONTRACT
            {
                return Err(format!("invalid work metadata at {index}"));
            }
            for key in ["packets", "bytes", "digest"] {
                u64::try_from(number(&work, key)?).map_err(|e| e.to_string())?;
            }
            let Value::Object(fields) = &work else {
                return Err("expected reference work object".into());
            };
            let candidate = Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| {
                        [
                            "schema",
                            "elapsed_ns",
                            "iterations",
                            "packets",
                            "bytes",
                            "digest",
                        ]
                        .contains(&key.as_str())
                    })
                    .cloned()
                    .collect(),
            );
            if case.work(true, timed) != candidate {
                return Err(format!("invalid candidate work projection at {index}"));
            }
            if case.refusal
                && (number(&work, "open_refusal_code")? != REFERENCE_REFUSAL
                    || number(&work, "open_refusals")? != i128::from(iterations)
                    || number(&work, "packets")? != 0
                    || number(&work, "bytes")? != 0)
            {
                return Err(format!("invalid refusal at {index}"));
            }
        }
        Ok(case)
    }

    fn operation(&self) -> Operation {
        Operation {
            prefix: format!("{}-", self.mode),
            arguments: self.arguments.iter().map(OsString::from).collect(),
            candidate_options: self.candidate_options.iter().map(OsString::from).collect(),
            work_fields: vec![],
        }
    }

    fn id(&self) -> String {
        format!("{}-{}-{}", self.label, self.mode, self.schedule)
    }

    fn operation_record(&self) -> Value {
        Value::object([
            ("name", Value::str("isobmff-open-index")),
            ("view", Value::str("ordered-trace-and-packet-fold")),
            ("prefix", Value::str(format!("{}-", self.mode))),
            ("arguments", Value::strings(self.arguments.clone())),
            (
                "candidate_options",
                Value::strings(self.candidate_options.clone()),
            ),
        ])
    }

    fn path(&self, root: &Path) -> PathBuf {
        root.join(FIXTURES).join(&self.input)
    }

    fn work(&self, candidate: bool, timed: bool) -> Value {
        let outputs = if timed {
            &self.time_outputs
        } else {
            &self.memory_outputs
        };
        outputs
            .get(if candidate { "candidate" } else { "reference" })
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[cfg(test)]
    fn evidence(&self, kind: &str, raw: Value) -> Value {
        Value::object([
            ("input_sha256", Value::str(&self.input_sha)),
            (
                "parity",
                Value::object([("expected", self.work(true, false))]),
            ),
            (kind, raw),
        ])
    }

    fn description(&self) -> Value {
        Value::object([
            ("id", Value::str(self.id())),
            ("profile", Value::str(self.id())),
            ("candidate_worker", Value::str(CANDIDATE)),
            ("reference_worker", Value::str(REFERENCE)),
            ("operation", self.operation_record()),
            ("expected_time_outputs", self.time_outputs.clone()),
            ("expected_memory_outputs", self.memory_outputs.clone()),
            ("index", Value::int(self.index as i128)),
            ("case", Value::str(&self.label)),
            ("mode", Value::str(&self.mode)),
            ("schedule", Value::str(&self.schedule)),
            ("input_sha256", Value::str(&self.input_sha)),
            (
                "outcome",
                Value::str(if self.refusal { "refusal" } else { "output" }),
            ),
            (
                "alias_of",
                self.alias.map_or(Value::Null, |i| Value::int(i as i128)),
            ),
        ])
    }

    fn physical_key(&self) -> String {
        json::canonical(&Value::object([
            ("input", Value::str(&self.input_sha)),
            ("mode", Value::str(&self.mode)),
            ("arguments", Value::strings(self.arguments.clone())),
            (
                "candidate_options",
                Value::strings(self.candidate_options.clone()),
            ),
            ("outcome", Value::Bool(self.refusal)),
        ]))
    }
}

struct Suite {
    contract: Value,
    cases: Vec<Case>,
    subset: Vec<usize>,
}

impl Suite {
    fn load(root: &Path) -> R<Self> {
        let contract = document(root, "contract", CONTRACT_SHA)?;
        let corpus = document(root, "corpus", CORPUS_SHA)?;
        let subset = document(root, "subset52", SUBSET_SHA)?;
        if contract.get("counts") != Some(&counts()) || corpus.get("counts") != Some(&counts()) {
            return Err("inconsistent corpus counts".into());
        }
        let cases: Vec<_> = corpus
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("missing cases")?
            .iter()
            .enumerate()
            .map(|(i, v)| Case::parse(v, i))
            .collect::<R<_>>()?;
        let subset = subset
            .get("indices")
            .and_then(Value::as_array)
            .ok_or("missing subset indices")?
            .iter()
            .map(|v| {
                usize::try_from(integer(v).ok_or("invalid subset index")?)
                    .map_err(|e| e.to_string())
            })
            .collect::<R<_>>()?;
        let suite = Self {
            contract,
            cases,
            subset,
        };
        suite.validate()?;
        Ok(suite)
    }

    fn validate(&self) -> R<()> {
        let profiles: Vec<_> = self
            .cases
            .iter()
            .map(|case| Value::str(case.id()))
            .collect();
        let refusals: Vec<_> = self
            .cases
            .iter()
            .filter(|case| case.refusal)
            .map(|case| {
                Value::object([
                    ("id", Value::str(case.id())),
                    ("profile", Value::str(case.id())),
                    ("sha256", Value::str(&case.input_sha)),
                ])
            })
            .collect();
        if self.contract.get("required_profiles") != Some(&Value::Array(profiles))
            || self.contract.get("required_refusals") != Some(&Value::Array(refusals))
            || self.contract.get("case_operations") != Some(&Value::Bool(true))
        {
            return Err("contract outcomes differ from the complete corpus".into());
        }
        let (mut labels, mut inputs, mut operations) = (
            BTreeMap::<&str, Vec<&Case>>::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let (mut outputs, mut refusals, mut aliases) = (0, 0, 0);
        for case in &self.cases {
            labels.entry(&case.label).or_default().push(case);
            if let Some(prior) = inputs.insert(&case.input_sha, case.input_bytes)
                && prior != case.input_bytes
            {
                return Err("inconsistent input length".into());
            }
            if case.refusal {
                refusals += 1;
            } else {
                outputs += 1;
            }
            if let Some(prior) = operations.get(&case.physical_key()) {
                let prior: &&Case = prior;
                if case.alias != Some(prior.index)
                    || case.refusal
                    || case.time_outputs != prior.time_outputs
                    || case.memory_outputs != prior.memory_outputs
                {
                    return Err(format!("invalid alias at {}", case.index));
                }
                aliases += 1;
            } else {
                if case.alias.is_some() {
                    return Err(format!("unexpected alias at {}", case.index));
                }
                operations.insert(case.physical_key(), case);
            }
        }
        let expected: BTreeSet<_> = [
            ("open", "ordinary"),
            ("native-index", "ordinary"),
            ("open", "tiny"),
            ("native-index", "tiny"),
        ]
        .into();
        if labels.values().any(|cases| {
            cases.len() != 4
                || cases
                    .iter()
                    .map(|c| (c.mode.as_str(), c.schedule.as_str()))
                    .collect::<BTreeSet<_>>()
                    != expected
                || cases
                    .iter()
                    .any(|c| c.input_sha != cases[0].input_sha || c.refusal != cases[0].refusal)
        }) || self.cases.len() != PARITY_CHECKS
            || labels.len() != 255
            || inputs.len() != 251
            || inputs.values().sum::<usize>() != 254_641
            || outputs != TIMABLE_OUTPUTS
            || refusals != EXPLICIT_REFUSALS
            || aliases != 16
            || outputs - aliases != PHYSICAL_OPERATIONS
            || self.subset.len() != 52
            || self.subset.iter().collect::<BTreeSet<_>>().len() != 52
            || self.subset.iter().any(|&i| {
                self.cases
                    .get(i)
                    .is_none_or(|c| c.refusal || c.alias.is_some())
            })
        {
            return Err("incomplete ISO BMFF operation membership".into());
        }
        Ok(())
    }

    fn selection(&self, subset: bool) -> Vec<usize> {
        if subset {
            self.subset.clone()
        } else {
            self.cases
                .iter()
                .filter(|c| !c.refusal)
                .map(|c| c.index)
                .collect()
        }
    }
}

/// ISO BMFF harness inputs to include in the shared build's source bindings.
pub fn source_inputs(root: &Path) -> R<Vec<String>> {
    let suite = Suite::load(root)?;
    let mut files: Vec<_> = [
        "xtask/src/isobmff_bench.rs",
        "xtask/src/harness/gate_build.rs",
        "xtask/src/stats.rs",
        "xtask/src/sha256.rs",
        "xtask/src/json.rs",
        "xtask/src/pyrandom.rs",
        "harness/policy.json",
        "harness/reference_isobmff.c",
        "harness/reference_isobmff_hash.h",
    ]
    .map(str::to_owned)
    .to_vec();
    files.extend(["contract", "corpus", "subset52"].map(|name| format!("{FEATURE}/{name}.json")));
    files.extend(
        suite
            .cases
            .iter()
            .map(|case| format!("{FIXTURES}/{}", case.input)),
    );
    files.sort();
    files.dedup();
    Ok(files)
}

fn verify_input(root: &Path, case: &Case) -> R<()> {
    let (_, actual) = provenance::file_identity(root, &case.path(root))?;
    let length = fs::metadata(case.path(root))
        .map_err(|e| e.to_string())?
        .len();
    if actual != case.input_sha || u128::from(length) != case.input_bytes as u128 {
        return Err(format!("input changed: {}", case.input));
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> R<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> R<String> {
    let data = format!("{}\n", json::canonical(value));
    write_new(path, data.as_bytes())?;
    Ok(sha256::hex(data.as_bytes()))
}

fn refusal_matches(candidate: &Finished, reference: &Finished) -> bool {
    candidate.code == 1
        && reference.code == 1
        && candidate.stdout.is_empty()
        && reference.stdout.is_empty()
        && reference
            .stderr
            .windows(b"(-1094995529)".len())
            .any(|w| w == b"(-1094995529)")
        && [
            b"isobmff-consumer: invalid data:".as_slice(),
            b"isobmff-consumer: truncated",
            b"isobmff-consumer: arithmetic overflow",
        ]
        .iter()
        .any(|prefix| candidate.stderr.starts_with(prefix))
}

fn output_matches(case: &Case, candidate: &Finished, reference: &Finished) -> bool {
    if candidate.code != 0
        || reference.code != 0
        || candidate.stdout.is_empty()
        || candidate.stdout != reference.stdout
    {
        return false;
    }
    let Ok(trace) = std::str::from_utf8(&reference.stdout)
        .map_err(|e| e.to_string())
        .and_then(|text| json::parse(text).map_err(|e| e.to_string()))
    else {
        return false;
    };
    trace.get("work").is_some_and(|work| {
        ["packets", "bytes", "digest"]
            .iter()
            .all(|k| work.get(k) == case.work(true, false).get(k))
    })
}

fn trace(
    root: &Path,
    candidate: &Path,
    reference: &Path,
    case: &Case,
    directory: &Path,
) -> R<Value> {
    let operation = case.operation();
    let input = case.path(root);
    let mut row = case.description();
    if let Err(error) = verify_input(root, case) {
        field(&mut row, "status", Value::str("invalid-input"));
        field(&mut row, "error", Value::str(error));
        field(&mut row, "match", Value::Bool(false));
        field(
            &mut row,
            "parity",
            Value::object([("status", Value::str("fail"))]),
        );
        return Ok(row);
    }
    let mut results = Vec::new();
    for (role, path, is_candidate) in [
        ("candidate", candidate, true),
        ("reference", reference, false),
    ] {
        let mut argv = vec![path.as_os_str().to_owned()];
        argv.extend(operation.worker_arguments(is_candidate, "trace", &input, None));
        let result = worker::run(&argv, &[], worker::DEFAULT_TIMEOUT);
        let record = match &result {
            Ok(Some(done)) => {
                write_new(&directory.join(format!("{role}.stdout")), &done.stdout)?;
                write_new(&directory.join(format!("{role}.stderr")), &done.stderr)?;
                Value::object([
                    ("status", Value::str("finished")),
                    ("exit_code", Value::int(done.code)),
                    ("stdout_sha256", Value::str(sha256::hex(&done.stdout))),
                    ("stderr_sha256", Value::str(sha256::hex(&done.stderr))),
                    ("stderr", Value::str(String::from_utf8_lossy(&done.stderr))),
                ])
            }
            Ok(None) => Value::object([("status", Value::str("timeout"))]),
            Err(error) => Value::object([
                ("status", Value::str("launch-failed")),
                ("error", Value::str(error)),
            ]),
        };
        field(&mut row, &format!("{role}_trace"), record);
        results.push(result);
    }
    let matched = match (&results[0], &results[1]) {
        (Ok(Some(a)), Ok(Some(b))) if case.refusal => refusal_matches(a, b),
        (Ok(Some(a)), Ok(Some(b))) => output_matches(case, a, b),
        _ => false,
    } && verify_input(root, case).is_ok();
    field(
        &mut row,
        "status",
        Value::str(if matched { "matched" } else { "mismatch" }),
    );
    field(&mut row, "match", Value::Bool(matched));
    field(
        &mut row,
        "parity",
        Value::object([
            ("status", Value::str(if matched { "pass" } else { "fail" })),
            (
                "source_timing_status",
                Value::str(if matched { "pass" } else { "fail" }),
            ),
            (
                "comparison",
                Value::str(if case.refusal { "refusal" } else { "output" }),
            ),
            ("expected", case.work(true, false)),
            ("candidate", case.work(true, false)),
            ("reference", case.work(true, false)),
        ]),
    );
    Ok(row)
}

fn native() -> bool {
    cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    ))
}

fn sampling_matches(raw: &Value, pairs: usize, seed: u64) -> bool {
    number(raw, "pairs").ok() == Some(pairs as i128)
        && number(raw, "seed").ok() == Some(i128::from(seed))
}

struct Options {
    benchmark: bool,
    memory: bool,
    subset: bool,
    require_acceptance: bool,
    output: PathBuf,
    pairs: usize,
    memory_pairs: usize,
    seed: u64,
}

impl Options {
    fn parse(root: &Path, args: &[String]) -> R<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let mut options = Self {
            benchmark: false,
            memory: false,
            subset: false,
            require_acceptance: false,
            output: root.join(format!(
                "target/isobmff-open-index/{stamp}-{}",
                std::process::id()
            )),
            pairs: MINIMUM_PAIRS,
            memory_pairs: MINIMUM_PAIRS,
            seed: 37,
        };
        let mut args = args.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--benchmark" => options.benchmark = true,
                "--memory" => options.memory = true,
                "--subset52" => options.subset = true,
                "--require-acceptance" => options.require_acceptance = true,
                "--output" => {
                    options.output = root.join(args.next().ok_or("--output needs a directory")?);
                }
                "--pairs" => {
                    options.pairs = args
                        .next()
                        .ok_or("--pairs needs a count")?
                        .parse()
                        .map_err(|e| format!("pairs: {e}"))?;
                }
                "--memory-pairs" => {
                    options.memory_pairs = args
                        .next()
                        .ok_or("--memory-pairs needs a count")?
                        .parse()
                        .map_err(|e| format!("memory pairs: {e}"))?;
                }
                "--seed" => {
                    options.seed = args
                        .next()
                        .ok_or("--seed needs an integer")?
                        .parse()
                        .map_err(|e| format!("seed: {e}"))?;
                }
                _ => return Err(format!("unknown option {argument}\n{USAGE}")),
            }
        }
        if options.pairs < MINIMUM_PAIRS
            || options.memory_pairs < MINIMUM_PAIRS
            || options.seed > i64::MAX as u64 - 2
        {
            return Err("need at least 30 pairs and a seed below the signed 64-bit limit".into());
        }
        if options.require_acceptance && (options.subset || !options.benchmark || !options.memory) {
            return Err("acceptance requires the full runtime and memory sets".into());
        }
        Ok(options)
    }
}

struct Build {
    candidate: PathBuf,
    reference: PathBuf,
    workers: Value,
    identity: Value,
    graph: Value,
}

impl Build {
    fn load(root: &Path) -> R<Self> {
        let identity = read_json(&root.join("target/harness/identity.json"))?;
        let candidate_record = identity
            .get("additional_candidates")
            .and_then(|v| v.get(CANDIDATE))
            .ok_or("ISO BMFF candidate is not registered; rebuild workers")?;
        let reference_record = comparison::reference_named(&identity, REFERENCE)?;
        let candidate = build::candidate_worker(root, &identity, CANDIDATE)?;
        let reference = root
            .join(text(&reference_record, "path")?)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let refusals = gate_build::pair_refusals(root, &candidate, &reference);
        if !refusals.is_empty() {
            return Err(format!("not the gate build: {}", refusals.join("; ")));
        }
        let graph = candidate_record
            .get("selected_graph")
            .cloned()
            .ok_or("missing ISO BMFF selected graph")?;
        let bindings = graph
            .get("shared_files")
            .ok_or("missing ISO BMFF harness bindings")?;
        for path in source_inputs(root)? {
            let (name, digest) = provenance::file_identity(root, Path::new(&path))?;
            if bindings.get(&name).and_then(Value::as_str) != Some(digest.as_str()) {
                return Err(format!(
                    "ISO BMFF harness input is not bound to the build: {name}; rebuild workers"
                ));
            }
        }
        let workers = Value::object([
            ("candidate", Value::str(file_sha(&candidate)?)),
            ("reference", Value::str(file_sha(&reference)?)),
        ]);
        if workers.get("candidate").and_then(Value::as_str)
            != Some(text(candidate_record, "sha256")?)
            || workers.get("reference").and_then(Value::as_str)
                != Some(text(&reference_record, "sha256")?)
        {
            return Err("selected worker changed since build identity".into());
        }
        Ok(Self {
            candidate,
            reference,
            workers,
            identity,
            graph,
        })
    }

    fn unchanged(&self, root: &Path) -> R<bool> {
        Ok(Self::load(root).is_ok_and(|current| {
            current.workers == self.workers
                && current.identity == self.identity
                && equal(&current.graph, &self.graph)
        }))
    }
}

fn case_directory(output: &Path, index: usize) -> PathBuf {
    output.join(format!("case-{index:04}"))
}

fn measurement_error(error: String) -> Value {
    Value::object([
        ("status", Value::str("runner-error")),
        ("error", Value::str(error)),
    ])
}

fn measure(
    root: &Path,
    build: &Build,
    case: &Case,
    options: &Options,
    contract: &Value,
    row: &Value,
) -> R<(Value, Value)> {
    let directory = case_directory(&options.output, case.index);
    let operation = case.operation();
    let input = case.path(root);
    let mut time = Value::Null;
    let mut memory_result = Value::Null;
    if options.benchmark {
        let raw = timing::benchmark(
            &build.candidate,
            &build.reference,
            &input,
            options.pairs,
            ITERATIONS,
            options.seed,
            &operation,
        )
        .unwrap_or_else(measurement_error);
        let mut evidence = row.clone();
        field(&mut evidence, "timing", raw.clone());
        let verdict = acceptance::grade_case(
            root,
            contract,
            &evidence,
            &build.identity,
            &report::host_target(),
        );
        let pass = verdict.timing_pass && sampling_matches(&raw, options.pairs, options.seed);
        let digest = write_json(&directory.join("timing.json"), &raw)?;
        time = Value::object([
            (
                "status",
                Value::str(text(&raw, "status").unwrap_or("invalid")),
            ),
            ("pass", Value::Bool(pass)),
            ("ratio", raw.get("ratio").cloned().unwrap_or(Value::Null)),
            (
                "per_iteration_ns",
                raw.get("per_iteration_ns").cloned().unwrap_or(Value::Null),
            ),
            (
                "raw",
                Value::str(format!("case-{:04}/timing.json", case.index)),
            ),
            ("raw_sha256", Value::str(digest)),
        ]);
    }
    if options.memory {
        let arguments = |candidate| {
            operation
                .worker_arguments(candidate, "memory", &input, None)
                .into_iter()
                .map(|s| {
                    s.into_string()
                        .map_err(|_| "worker arguments must be UTF-8".to_owned())
                })
                .collect::<R<Vec<_>>>()
        };
        let candidate = arguments(true)?;
        let reference = arguments(false)?;
        let expected = Value::object([
            ("candidate", case.work(true, false)),
            ("reference", case.work(false, false)),
        ]);
        let raw = memory::collect_commands(
            root,
            &build.candidate,
            &build.reference,
            &input,
            options.memory_pairs,
            options.seed,
            memory::Commands {
                candidate: &candidate,
                reference: &reference,
                expected_outputs: &expected,
            },
        )
        .unwrap_or_else(measurement_error);
        let mut evidence = row.clone();
        field(&mut evidence, "memory", raw.clone());
        let verdict = acceptance::grade_case(
            root,
            contract,
            &evidence,
            &build.identity,
            &report::host_target(),
        );
        let pass =
            verdict.memory_pass && sampling_matches(&raw, options.memory_pairs, options.seed);
        let digest = write_json(&directory.join("memory.json"), &raw)?;
        memory_result = Value::object([
            (
                "status",
                Value::str(text(&raw, "status").unwrap_or("invalid")),
            ),
            ("pass", Value::Bool(pass)),
            (
                "heap_ratio_upper_95",
                raw.get("heap_ratio_upper_95")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            (
                "total_rss_ratio_upper_95",
                raw.get("total_rss_ratio_upper_95")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            (
                "raw",
                Value::str(format!("case-{:04}/memory.json", case.index)),
            ),
            ("raw_sha256", Value::str(digest)),
        ]);
    }
    Ok((time, memory_result))
}

fn alias_traces_match(output: &Path, case: usize, prior: usize) -> R<bool> {
    for role in ["candidate", "reference"] {
        let file = format!("{role}.stdout");
        let a = fs::read(case_directory(output, case).join(&file)).map_err(|e| e.to_string())?;
        let b = fs::read(case_directory(output, prior).join(&file)).map_err(|e| e.to_string())?;
        if a != b {
            return Ok(false);
        }
    }
    Ok(true)
}

fn passed(value: &Value) -> bool {
    value.get("pass") == Some(&Value::Bool(true))
}

fn summary(rows: &[Value], selection: &[usize], options: &Options, unchanged: bool) -> Value {
    let parity_completed = rows.iter().filter(|r| r.get("match").is_some()).count();
    let parity_matches = rows
        .iter()
        .filter(|r| r.get("match") == Some(&Value::Bool(true)))
        .count();
    let refusal_matches = rows
        .iter()
        .filter(|r| {
            r.get("match") == Some(&Value::Bool(true))
                && r.get("outcome").and_then(Value::as_str) == Some("refusal")
        })
        .count();
    let time = selection
        .iter()
        .filter(|&&i| rows[i].get("timing").is_some_and(passed))
        .count();
    let memory = selection
        .iter()
        .filter(|&&i| rows[i].get("memory").is_some_and(passed))
        .count();
    let collected = |kind: &str| {
        selection
            .iter()
            .filter(|&&i| rows[i].get(kind).is_some_and(|v| *v != Value::Null))
            .count()
    };
    let collection_complete = parity_completed == PARITY_CHECKS
        && (!options.benchmark || collected("timing") == selection.len())
        && (!options.memory || collected("memory") == selection.len());
    let parity_complete = rows.len() == PARITY_CHECKS && parity_matches == PARITY_CHECKS;
    let all_requested = parity_complete
        && unchanged
        && (!options.benchmark || time == selection.len())
        && (!options.memory || memory == selection.len());
    Value::object([
        ("parity_matches", Value::int(parity_matches as i128)),
        (
            "parity_checks_completed",
            Value::int(parity_completed as i128),
        ),
        ("collection_complete", Value::Bool(collection_complete)),
        ("parity_complete", Value::Bool(parity_complete)),
        (
            "output_matches",
            Value::int((parity_matches - refusal_matches) as i128),
        ),
        ("refusal_matches", Value::int(refusal_matches as i128)),
        ("timable_outputs", Value::int(TIMABLE_OUTPUTS as i128)),
        ("explicit_refusals", Value::int(EXPLICIT_REFUSALS as i128)),
        (
            "selected_output_labels",
            Value::int(selection.len() as i128),
        ),
        (
            "selected_physical_operations",
            Value::int(if options.subset {
                52
            } else {
                PHYSICAL_OPERATIONS as i128
            }),
        ),
        ("timing_passes", Value::int(time as i128)),
        ("memory_passes", Value::int(memory as i128)),
        ("source_and_workers_unchanged", Value::Bool(unchanged)),
        ("all_requested_checks_pass", Value::Bool(all_requested)),
    ])
}

fn checkpoint(
    output: &Path,
    base: &Value,
    rows: &[Value],
    selection: &[usize],
    options: &Options,
    unchanged: bool,
) -> R<Value> {
    let mut report = base.clone();
    let summary = summary(rows, selection, options, unchanged);
    field(&mut report, "summary", summary.clone());
    field(&mut report, "cases", Value::Array(rows.to_vec()));
    field(&mut report, "case_storage", Value::str("sha256-files"));
    field(
        &mut report,
        "collection_status",
        Value::str(
            if summary.get("collection_complete") == Some(&Value::Bool(true)) {
                "complete"
            } else {
                "partial"
            },
        ),
    );
    fs::write(
        output.join("partial.json"),
        format!("{}\n", json::canonical(&report)),
    )
    .map_err(|e| e.to_string())?;
    Ok(summary)
}

/// Expand only this case, including explicit aliases, from its checked raw files.
fn report_case(output: &Path, row: &Value) -> R<Value> {
    crate::harness::validation::expand_measurements(output, row)
}

/// Write canonical report JSON with at most one case's raw observations in memory.
/// Publish only a complete document and never replace an existing report.
fn final_report(
    output: &Path,
    base: &Value,
    rows: &[Value],
    summary: &Value,
    verdict: &Value,
) -> R<()> {
    let mut report = base.clone();
    field(&mut report, "summary", summary.clone());
    field(
        &mut report,
        "collection_status",
        Value::str(
            if summary.get("collection_complete") == Some(&Value::Bool(true)) {
                "complete"
            } else {
                "partial"
            },
        ),
    );
    field(&mut report, "cases", Value::Null);
    field(&mut report, "acceptance", verdict.clone());
    field(&mut report, "case_storage", Value::str("inline"));
    report::write_case_report(
        output,
        &report,
        rows.iter().map(|row| report_case(output, row)),
    )?;
    field(&mut report, "case_storage", Value::str("sha256-files"));
    field(&mut report, "cases", Value::Array(rows.to_vec()));
    write_json(&output.join("collection.json"), &report)?;
    Ok(())
}

/// Compare the full corpus and optionally measure the full or 52-operation set.
pub fn main(root: &Path, args: &[String]) -> R<i32> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let options = Options::parse(&root, args)?;
    let suite = Suite::load(&root)?;
    let policy = read_json(&root.join("harness/policy.json"))?;
    let build = Build::load(&root)?;
    if let Some(parent) = options.output.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::create_dir(&options.output).map_err(|e| {
        format!(
            "{}: {e}; use a new output directory",
            options.output.display()
        )
    })?;
    let selection = suite.selection(options.subset);
    let base = Value::object([
        ("schema", Value::int(1)),
        ("contract", Value::str("isobmff-open-index")),
        ("contract_id", Value::str("isobmff-open-index")),
        ("target", Value::str(report::host_target())),
        ("counts", counts()),
        ("contract_sha256", Value::str(CONTRACT_SHA)),
        ("corpus_sha256", Value::str(CORPUS_SHA)),
        ("corpus_manifest_sha256", Value::str(CORPUS_SHA)),
        ("work_contract_sha256", Value::str(WORK_CONTRACT)),
        ("subset52_sha256", Value::str(SUBSET_SHA)),
        (
            "selection",
            Value::Array(selection.iter().map(|&i| Value::int(i as i128)).collect()),
        ),
        ("identity", build.identity.clone()),
        ("selected_graph", build.graph.clone()),
        ("workers", build.workers.clone()),
        ("policy", policy),
        ("cpus", gate_build::cpus()),
        (
            "environment",
            Value::object([
                (
                    "platform",
                    Value::str(if cfg!(target_os = "linux") {
                        "Linux"
                    } else {
                        "Darwin"
                    }),
                ),
                ("load", report::load_average()),
                ("cpus", gate_build::cpus()),
            ]),
        ),
        ("native_aarch64_linux", Value::Bool(native())),
        ("benchmark_requested", Value::Bool(options.benchmark)),
        ("memory_requested", Value::Bool(options.memory)),
        ("pairs", Value::int(options.pairs as i128)),
        ("memory_pairs", Value::int(options.memory_pairs as i128)),
        ("iterations", Value::int(ITERATIONS)),
        ("seed", Value::int(options.seed)),
    ]);
    write_json(&options.output.join("identity.json"), &base)?;
    let mut rows: Vec<_> = suite
        .cases
        .iter()
        .map(|case| {
            let mut row = case.description();
            field(&mut row, "status", Value::str("not-run"));
            row
        })
        .collect();
    checkpoint(&options.output, &base, &rows, &selection, &options, true)?;
    for case in &suite.cases {
        let directory = case_directory(&options.output, case.index);
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let mut row = trace(&root, &build.candidate, &build.reference, case, &directory)?;
        field(
            &mut row,
            "reference",
            comparison::reference_summary(&comparison::reference_named(
                &build.identity,
                REFERENCE,
            )?),
        );
        write_json(&directory.join("parity.json"), &row)?;
        rows[case.index] = row;
    }
    let all_parity = rows
        .iter()
        .all(|r| r.get("match") == Some(&Value::Bool(true)));
    let mut unchanged = build.unchanged(&root)?;
    checkpoint(
        &options.output,
        &base,
        &rows,
        &selection,
        &options,
        unchanged,
    )?;
    if all_parity && unchanged && (options.benchmark || options.memory) {
        for &index in &selection {
            let case = &suite.cases[index];
            let (time, memory) = if let Some(prior) = case.alias {
                if !selection.contains(&prior)
                    || !alias_traces_match(&options.output, index, prior)?
                {
                    return Err(format!(
                        "alias {index} has no identical selected operation {prior}"
                    ));
                }
                field(&mut rows[index], "sampled_as", Value::int(prior as i128));
                field(
                    &mut rows[index],
                    "sampled_profile",
                    suite.cases[prior]
                        .description()
                        .get("profile")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                (
                    rows[prior].get("timing").cloned().unwrap_or(Value::Null),
                    rows[prior].get("memory").cloned().unwrap_or(Value::Null),
                )
            } else {
                if let Err(error) = verify_input(&root, case) {
                    unchanged = false;
                    field(&mut rows[index], "measurement_error", Value::str(error));
                    checkpoint(&options.output, &base, &rows, &selection, &options, false)?;
                    break;
                }
                measure(&root, &build, case, &options, &suite.contract, &rows[index])?
            };
            if time != Value::Null {
                field(&mut rows[index], "timing", time);
            }
            if memory != Value::Null {
                field(&mut rows[index], "memory", memory);
            }
            write_json(
                &case_directory(&options.output, index).join("result.json"),
                &rows[index],
            )?;
            checkpoint(
                &options.output,
                &base,
                &rows,
                &selection,
                &options,
                unchanged,
            )?;
            println!("{}: {} {} {}", index, case.label, case.mode, case.schedule);
        }
    }
    unchanged &= build.unchanged(&root)?;
    let mut summary = checkpoint(
        &options.output,
        &base,
        &rows,
        &selection,
        &options,
        unchanged,
    )?;
    let observations = rows.iter().map(|row| report_case(&options.output, row));
    let errors = (!unchanged).then(|| Err("source or worker changed during collection".to_owned()));
    let verdict = acceptance::acceptance_iter(
        &root,
        &suite.contract,
        observations.chain(errors),
        &build.identity,
        &report::host_target(),
        &[],
        &[],
    );
    field(
        &mut summary,
        "qualified",
        verdict
            .get("slice_accepted")
            .cloned()
            .unwrap_or(Value::Bool(false)),
    );
    final_report(&options.output, &base, &rows, &summary, &verdict)?;
    println!("{}", json::canonical(&summary));
    println!("report: {}", options.output.join("report.json").display());
    let ok = if options.require_acceptance {
        verdict.get("slice_accepted")
    } else {
        summary.get("all_requested_checks_pass")
    } == Some(&Value::Bool(true));
    Ok(i32::from(!ok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::acceptance::{
        memory_outputs_match, output_matches as work_matches, timing_outputs_match,
    };

    fn final_report(output: &Path, base: &Value, rows: &[Value], summary: &Value) -> R<()> {
        let verdict = Value::object([
            ("accepted", Value::Bool(false)),
            ("slice_accepted", Value::Bool(false)),
            ("reasons", Value::strings(["missing observations"])),
        ]);
        super::final_report(output, base, rows, summary, &verdict)
    }

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn complete_membership_keeps_every_operation_and_refusal() {
        let suite = Suite::load(&root()).unwrap();
        assert_eq!(suite.cases.len(), 1020);
        assert_eq!(suite.selection(false).len(), 972);
        assert_eq!(suite.cases.iter().filter(|c| c.refusal).count(), 48);
        assert_eq!(
            suite
                .cases
                .iter()
                .filter(|c| !c.refusal && c.alias.is_none())
                .count(),
            956
        );
        let mut broken = suite;
        broken.cases.remove(100);
        assert!(broken.validate().is_err());
    }

    #[test]
    fn subset52_is_exact_and_does_not_replace_parity() {
        let suite = Suite::load(&root()).unwrap();
        let expected: Vec<_> = (180..188)
            .chain(192..196)
            .chain(164..168)
            .chain(176..180)
            .chain(188..192)
            .chain(196..200)
            .chain(0..8)
            .chain(136..140)
            .chain(148..152)
            .chain(220..224)
            .chain(360..364)
            .collect();
        assert_eq!(suite.selection(true), expected);
        assert_eq!(suite.cases.len(), 1020);
        for i in expected {
            assert!(!suite.cases[i].refusal);
            assert!(suite.cases[i].alias.is_none());
        }
    }

    #[test]
    fn aliases_keep_the_original_membership_and_work() {
        let mut suite = Suite::load(&root()).unwrap();
        let actual: Vec<_> = suite
            .cases
            .iter()
            .filter_map(|c| c.alias.map(|a| (c.index, a)))
            .collect();
        assert_eq!(
            actual,
            [
                (248, 56),
                (249, 57),
                (250, 58),
                (251, 59),
                (364, 0),
                (365, 1),
                (366, 2),
                (367, 3),
                (588, 432),
                (589, 433),
                (590, 434),
                (591, 435),
                (668, 0),
                (669, 1),
                (670, 2),
                (671, 3)
            ]
        );
        suite.cases[248].alias = Some(0);
        assert!(suite.validate().is_err());
    }

    #[test]
    fn all_small_authored_inputs_match_their_source_hashes() {
        let root = root();
        let suite = Suite::load(&root).unwrap();
        let mut seen = BTreeSet::new();
        let mut bytes = 0;
        for case in &suite.cases {
            if seen.insert(&case.input_sha) {
                verify_input(&root, case).unwrap();
                bytes += case.input_bytes;
            }
        }
        assert_eq!(seen.len(), 251);
        assert_eq!(bytes, 254_641);
    }

    #[test]
    fn recovery_options_precede_the_mode_for_each_role() {
        let suite = Suite::load(&root()).unwrap();
        for i in [0, 100, 176, 188, 196] {
            let case = &suite.cases[i];
            let operation = case.operation();
            for (mode, iterations) in [("trace", None), ("bench", Some(50)), ("memory", None)] {
                let reference =
                    operation.worker_arguments(false, mode, Path::new("x.mov"), iterations);
                let candidate =
                    operation.worker_arguments(true, mode, Path::new("x.mov"), iterations);
                assert_eq!(
                    candidate[..case.candidate_options.len()],
                    case.candidate_options
                        .iter()
                        .map(OsString::from)
                        .collect::<Vec<_>>()
                );
                assert_eq!(candidate[case.candidate_options.len()..], reference);
                assert_eq!(
                    reference[0],
                    OsString::from(format!("{}-{mode}", case.mode))
                );
            }
        }
        assert_eq!(
            suite.cases[176].candidate_options,
            ["--recover-regular-tables"]
        );
    }

    #[test]
    fn expected_work_preserves_proofs_and_exact_integer_types() {
        let suite = Suite::load(&root()).unwrap();
        let case = &suite.cases[0];
        let candidate = case.work(true, false);
        let reference = case.work(false, false);
        assert!(candidate.get("input_sha256").is_none());
        assert_eq!(text(&reference, "input_sha256").unwrap(), case.input_sha);
        assert_eq!(text(&reference, "contract_sha256").unwrap(), WORK_CONTRACT);
        assert!(work_matches(&reference, &reference, false));
        assert!(!work_matches(&candidate, &reference, false));
        let mut altered = reference.clone();
        field(
            &mut altered,
            "digest",
            Value::float(number(&reference, "digest").unwrap() as f64),
        );
        assert!(!work_matches(&altered, &reference, false));
        field(
            &mut altered,
            "digest",
            reference.get("digest").unwrap().clone(),
        );
        field(&mut altered, "elapsed_ns", Value::int(1));
        assert!(!work_matches(&altered, &reference, false));
        assert!(work_matches(&altered, &reference, true));
    }

    fn value_at<'a>(value: &'a mut Value, path: &[&str]) -> &'a mut Value {
        let mut current = value;
        for key in path {
            current = match current {
                Value::Object(fields) => &mut fields.iter_mut().find(|(k, _)| k == key).unwrap().1,
                Value::Array(values) => &mut values[key.parse::<usize>().unwrap()],
                _ => panic!("expected a container at {key}"),
            };
        }
        current
    }

    #[test]
    fn timing_role_proofs_are_required_in_warmups_and_every_pair() {
        let suite = Suite::load(&root()).unwrap();
        let case = &suite.cases[0];
        let result = |candidate| {
            let mut work = case.work(candidate, true);
            field(&mut work, "elapsed_ns", Value::int(100));
            Value::object([("status", Value::str("ok")), ("data", work)])
        };
        let raw = Value::object([
            (
                "warmups",
                Value::Array(vec![
                    Value::object([
                        ("worker", Value::str("candidate")),
                        ("result", result(true)),
                    ]),
                    Value::object([
                        ("worker", Value::str("reference")),
                        ("result", result(false)),
                    ]),
                ]),
            ),
            (
                "samples",
                Value::Array(vec![Value::object([
                    ("candidate", result(true)),
                    ("reference", result(false)),
                ])]),
            ),
        ]);
        assert!(timing_outputs_match(&raw, &case.time_outputs));
        for path in [
            vec!["warmups", "1", "result", "data", "input_sha256"],
            vec!["warmups", "1", "result", "data", "contract_sha256"],
            vec!["samples", "0", "reference", "data", "input_sha256"],
            vec!["samples", "0", "reference", "data", "contract_sha256"],
        ] {
            let mut changed = raw.clone();
            *value_at(&mut changed, &path) = Value::str("changed");
            assert!(
                !timing_outputs_match(&changed, &case.time_outputs),
                "{path:?}"
            );
        }
        for path in [
            vec!["warmups", "0", "result", "data", "elapsed_ns"],
            vec!["samples", "0", "candidate", "data", "elapsed_ns"],
        ] {
            let mut changed = raw.clone();
            *value_at(&mut changed, &path) = Value::int(0);
            assert!(
                !timing_outputs_match(&changed, &case.time_outputs),
                "{path:?}"
            );
        }
    }

    #[test]
    fn memory_role_proofs_preserve_exact_outputs_and_all_four_reports() {
        let suite = Suite::load(&root()).unwrap();
        let case = &suite.cases[0];
        let expected = Value::object([
            ("candidate", case.work(true, false)),
            ("reference", case.work(false, false)),
        ]);
        let raw = Value::object([(
            "samples",
            Value::Array(vec![Value::object([
                (
                    "candidate_heap",
                    Value::object([("work", case.work(true, false))]),
                ),
                (
                    "candidate_rss",
                    Value::object([("work", case.work(true, false))]),
                ),
                (
                    "reference_heap",
                    Value::object([("work", case.work(false, false))]),
                ),
                (
                    "reference_rss",
                    Value::object([("work", case.work(false, false))]),
                ),
            ])]),
        )]);
        assert!(memory_outputs_match(&raw, &expected));
        for role in ["reference_heap", "reference_rss"] {
            let mut changed = raw.clone();
            *value_at(
                &mut changed,
                &["samples", "0", role, "work", "contract_sha256"],
            ) = Value::str("changed");
            assert!(!memory_outputs_match(&changed, &expected));
        }
        let mut changed = raw.clone();
        *value_at(&mut changed, &["samples", "0", "candidate_heap", "work"]) =
            case.work(false, false);
        assert!(!memory_outputs_match(&changed, &expected));
        let mut changed = raw.clone();
        let Value::Object(reports) = value_at(&mut changed, &["samples", "0"]) else {
            panic!("expected the four reports");
        };
        reports.pop();
        assert!(!memory_outputs_match(&changed, &expected));
        let evidence = case.evidence("memory", raw.clone());
        assert_eq!(evidence.get("memory"), Some(&raw));
        assert_eq!(
            evidence.get("parity").unwrap().get("expected"),
            expected.get("candidate")
        );
        assert_eq!(text(&evidence, "input_sha256").unwrap(), case.input_sha);
    }

    #[test]
    fn recorded_sampling_must_match_the_selected_integer_counts_and_seed() {
        let mut raw = Value::object([("pairs", Value::int(30)), ("seed", Value::int(280))]);
        assert!(sampling_matches(&raw, 30, 280));
        assert!(!sampling_matches(&raw, 31, 280));
        assert!(!sampling_matches(&raw, 30, 281));
        field(&mut raw, "pairs", Value::float(30.0));
        assert!(!sampling_matches(&raw, 30, 280));
        field(&mut raw, "pairs", Value::int(30));
        field(&mut raw, "seed", Value::Bool(true));
        assert!(!sampling_matches(&raw, 30, 1));
    }

    #[test]
    fn every_fifty_iteration_count_and_digest_composes_from_single_operation_work() {
        let suite = Suite::load(&root()).unwrap();
        for case in &suite.cases {
            let one = u64::try_from(number(&case.work(false, false), "digest").unwrap()).unwrap();
            let digest =
                (0..ITERATIONS).fold(0u64, |total, _| total.wrapping_mul(31).wrapping_add(one));
            assert_eq!(
                number(&case.work(false, true), "digest").unwrap(),
                i128::from(digest)
            );
            for key in ["packets", "bytes"] {
                assert_eq!(
                    number(&case.work(false, true), key).unwrap(),
                    number(&case.work(false, false), key).unwrap() * i128::from(ITERATIONS)
                );
            }
        }
    }

    #[test]
    fn refusal_needs_both_explicit_failures_and_no_output() {
        let candidate = Finished {
            code: 1,
            stdout: vec![],
            stderr: b"isobmff-consumer: invalid data: box".to_vec(),
        };
        let mut reference = Finished {
            code: 1,
            stdout: vec![],
            stderr: b"mov-consumer: Invalid data (-1094995529)\n".to_vec(),
        };
        assert!(refusal_matches(&candidate, &reference));
        reference.code = 0;
        assert!(!refusal_matches(&candidate, &reference));
        reference.code = 1;
        reference.stdout.push(b' ');
        assert!(!refusal_matches(&candidate, &reference));
        reference.stdout.clear();
        reference.stderr.clear();
        assert!(!refusal_matches(&candidate, &reference));
    }

    #[test]
    fn partial_measurement_sets_cannot_request_acceptance() {
        let suite = Suite::load(&root()).unwrap();
        let options = Options::parse(
            &root(),
            &["--subset52".into(), "--benchmark".into(), "--memory".into()],
        )
        .unwrap();
        let rows: Vec<_> = suite
            .cases
            .iter()
            .map(|_| {
                Value::object([
                    ("match", Value::Bool(true)),
                    ("timing", Value::object([("pass", Value::Bool(true))])),
                    ("memory", Value::object([("pass", Value::Bool(true))])),
                ])
            })
            .collect();
        let result = summary(&rows, &suite.selection(true), &options, true);
        assert_eq!(result.get("parity_matches"), Some(&Value::int(1020)));
        assert!(result.get("qualified").is_none());
        let mut failed = rows;
        field(&mut failed[100], "match", Value::Bool(false));
        assert_eq!(
            summary(&failed, &suite.selection(true), &options, true)
                .get("all_requested_checks_pass"),
            Some(&Value::Bool(false))
        );
        assert!(
            Options::parse(
                &root(),
                &["--subset52".into(), "--require-acceptance".into()]
            )
            .is_err()
        );
    }

    #[test]
    fn streamed_report_retains_raw_samples_aliases_and_proof_fields() {
        let out = crate::walk::scratch("isobmff-report");
        fs::create_dir_all(out.join("case-0000")).unwrap();
        let raw = Value::object([
            ("status", Value::str("measured")),
            (
                "samples",
                Value::Array(vec![Value::object([("digest", Value::int(u64::MAX))])]),
            ),
            ("input_sha256", Value::str("input-proof")),
        ]);
        let digest = write_json(&out.join("case-0000/timing.json"), &raw).unwrap();
        let grade = Value::object([
            ("raw", Value::str("case-0000/timing.json")),
            ("raw_sha256", Value::str(digest)),
            ("pass", Value::Bool(false)),
        ]);
        let memory = Value::object([
            ("status", Value::str("qualified")),
            (
                "certificate",
                Value::object([("kernel_rss_required", Value::Bool(false))]),
            ),
            (
                "samples",
                Value::Array(vec![Value::object([
                    (
                        "candidate_heap",
                        Value::object([(
                            "work",
                            Value::object([
                                ("packets", Value::int(6)),
                                ("digest", Value::int(u64::MAX)),
                            ]),
                        )]),
                    ),
                    (
                        "reference_heap",
                        Value::object([(
                            "work",
                            Value::object([
                                ("packets", Value::int(6)),
                                ("digest", Value::int(u64::MAX)),
                                ("input_sha256", Value::str("input-proof")),
                                ("contract_sha256", Value::str(WORK_CONTRACT)),
                            ]),
                        )]),
                    ),
                ])]),
            ),
        ]);
        let memory_digest = write_json(&out.join("case-0000/memory.json"), &memory).unwrap();
        let memory_grade = Value::object([
            ("raw", Value::str("case-0000/memory.json")),
            ("raw_sha256", Value::str(memory_digest)),
            ("pass", Value::Bool(false)),
        ]);
        let rows = vec![
            Value::object([
                ("timing", grade.clone()),
                ("memory", memory_grade.clone()),
                ("profile", Value::str("original")),
            ]),
            Value::object([
                ("timing", grade),
                ("memory", memory_grade),
                ("profile", Value::str("alias")),
                ("sampled_as", Value::int(0)),
            ]),
            Value::object([
                ("profile", Value::str("refusal")),
                ("outcome", Value::str("refusal")),
                ("memory", Value::Null),
            ]),
        ];
        let summary = Value::object([
            ("qualified", Value::Bool(false)),
            ("collection_complete", Value::Bool(false)),
        ]);
        let base = Value::object([
            ("schema", Value::int(1)),
            ("counts", counts()),
            (
                "identity",
                Value::object([("cases", Value::strings(["nested \"quote\"\\\n"]))]),
            ),
        ]);
        final_report(&out, &base, &rows, &summary).unwrap();
        let report = read_json(&out.join("report.json")).unwrap();
        assert_eq!(report.get("schema"), Some(&Value::int(1)));
        assert!(
            report
                .get("identity")
                .is_some_and(|v| equal(v, base.get("identity").unwrap()))
        );
        assert!(report.get("counts").is_some_and(|v| equal(v, &counts())));
        assert!(report.get("summary").is_some_and(|v| equal(v, &summary)));
        assert_eq!(
            report.get("acceptance").unwrap().get("accepted"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            fs::read_to_string(out.join("report.json")).unwrap(),
            format!("{}\n", json::canonical(&report))
        );
        let cases = report.get("cases").unwrap().as_array().unwrap();
        assert_eq!(cases.len(), 3);
        assert_eq!(
            cases
                .iter()
                .map(|c| text(c, "profile").unwrap())
                .collect::<Vec<_>>(),
            ["original", "alias", "refusal"]
        );
        assert!(
            cases[0]
                .get("timing")
                .is_some_and(|value| equal(value, &raw))
        );
        assert!(
            cases[1]
                .get("timing")
                .is_some_and(|value| equal(value, &raw))
        );
        assert_eq!(cases[1].get("sampled_as"), Some(&Value::int(0)));
        for case in &cases[..2] {
            assert!(case.get("memory").is_some_and(|v| equal(v, &memory)));
            assert!(case.get("memory_grade").is_none());
            assert!(case.get("timing_grade").is_none());
        }
        assert!(cases[2].get("timing").is_none());
        assert_eq!(cases[2].get("memory"), Some(&Value::Null));
        assert!(cases[2].get("memory_grade").is_none());
        assert_eq!(
            report.get("collection_status"),
            Some(&Value::str("partial"))
        );
        assert!(!out.join("report.json.tmp").exists());
        let published = fs::read(out.join("report.json")).unwrap();
        assert!(final_report(&out, &base, &rows, &summary).is_err());
        assert_eq!(fs::read(out.join("report.json")).unwrap(), published);
        fs::remove_file(out.join("report.json")).unwrap();
        fs::write(out.join("case-0000/timing.json"), "{}").unwrap();
        let error = final_report(&out, &base, &rows, &summary).unwrap_err();
        assert!(error.contains("raw measurement changed"));
        assert!(!out.join("report.json").exists());
        assert!(!out.join("report.json.tmp").exists());
        fs::remove_dir_all(out).unwrap();
    }

    #[test]
    fn streamed_report_keeps_the_complete_ordered_membership() {
        let suite = Suite::load(&root()).unwrap();
        let rows: Vec<_> = suite.cases.iter().map(Case::description).collect();
        let out = crate::walk::scratch("isobmff-report-membership");
        let summary = Value::object([
            ("qualified", Value::Bool(false)),
            ("collection_complete", Value::Bool(false)),
        ]);
        final_report(
            &out,
            &Value::object([("schema", Value::int(1))]),
            &rows,
            &summary,
        )
        .unwrap();
        let report = read_json(&out.join("report.json")).unwrap();
        assert!(
            report
                .get("cases")
                .is_some_and(|v| equal(v, &Value::Array(rows)))
        );
        assert_eq!(report.get("cases").unwrap().as_array().unwrap().len(), 1020);
        fs::remove_dir_all(out).unwrap();
    }

    #[test]
    #[ignore = "requires the built ISO BMFF example and public-API reference"]
    fn installed_workers_match_all_1020_traces() {
        let root = root();
        let suite = Suite::load(&root).unwrap();
        let out = crate::walk::scratch("isobmff-parity");
        fs::create_dir_all(&out).unwrap();
        let mut mismatches = Vec::new();
        for case in &suite.cases {
            let directory = case_directory(&out, case.index);
            fs::create_dir(&directory).unwrap();
            let result = trace(
                &root,
                &root.join("target/release/isobmff-worker"),
                &root.join("target/harness/ffmpeg-isobmff-worker"),
                case,
                &directory,
            )
            .unwrap();
            if result.get("match") != Some(&Value::Bool(true)) {
                mismatches.push(case.index);
            }
        }
        assert!(
            mismatches.is_empty(),
            "mismatching traces {mismatches:?}; outputs: {}",
            out.display()
        );
        fs::remove_dir_all(out).unwrap();
    }
}
