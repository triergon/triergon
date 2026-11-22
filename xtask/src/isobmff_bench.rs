//! ISO BMFF open and native-index comparisons over the complete authored corpus.
//!
//! Every label gets its own ordered trace comparison. Only successful operations
//! are measured; explicit aliases share measurements after their traces match.
//! The 52-operation selection changes measurement membership only. The seek
//! suite ([`crate::isobmff_seek`]) runs its cases through the same comparison and
//! measurement.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codecs::correctness::file_sha;
use crate::harness::diff::equal;
use crate::harness::policy::{Memory, Runtime};
use crate::harness::timing::{self, Operation};
use crate::harness::worker::{self, Finished};
use crate::harness::{
    acceptance, build, comparison, gate_build, memory, provenance, report, reuse,
};
use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

pub const USAGE: &str = "usage: cargo xtask isobmff-bench [--benchmark] [--memory]
    [--subset52] [--part I/N] [--output DIR] [--pairs N] [--memory-pairs N] [--seed N]
    [--require-acceptance] [--full] [--no-early-stop]

Always compare all 1020 traces: 972 output labels and 48 explicit refusals.
The full measurement set has 956 physical operations and 16 explicit aliases.
--subset52 measures the separate 52-operation manifest, retaining full parity.
--part I/N measures every Nth physical operation of the set, from the Ith, with the
aliases of those operations, retaining full parity; the N parts together measure
the whole set once.
Workers must belong to the current recorded gate build. Acceptance also requires
the complete runtime and memory sets on native AArch64 Linux. For combined
qualification, keep the RSS kernel module unloaded and
TRIERGON_RSS_KERNEL_BOUND unset or 0, using the conservative RSS observer.
For kernel-bound memory, collect the phases separately and use cargo xtask grade.";

pub const CANDIDATE: &str = "isobmff-worker";
pub const REFERENCE: &str = "ffmpeg-isobmff-native";
const FEATURE: &str = "harness/features/isobmff-open-index";
const FIXTURES: &str = "harness/fixtures/isobmff-open-index";
const CONTRACT_SHA: &str = "783092be3f53c9a16fd1947ceca0e409435cabc9d4cc6761e0f18baf15d9b2cc";
const CORPUS_SHA: &str = "03761df0849a8d60a3cc2222e35dd8fe02308c0fe774b3e442b6850d99704e58";
const SUBSET_SHA: &str = "d5efa68f708ffe403018a8c34aaeb10aa693cb4e5abcf9a0b571339c69d40b61";
pub(crate) const WORK_CONTRACT: &str =
    "0a924396626871e84b2f91ec612a0c60b09258f0f016afe0753cf7c75fdecf8a";
pub(crate) const ITERATIONS: u32 = 50;
const PARITY_CHECKS: usize = 1020;
const TIMABLE_OUTPUTS: usize = 972;
const PHYSICAL_OPERATIONS: usize = 956;
const EXPLICIT_REFUSALS: usize = 48;
const REFERENCE_REFUSAL: i128 = -1_094_995_529;

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> R<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}"))
}

pub(crate) fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::Number(number) => number.as_i128(),
        _ => None,
    }
}

pub(crate) fn number(value: &Value, key: &str) -> R<i128> {
    value
        .get(key)
        .and_then(integer)
        .ok_or_else(|| format!("missing integer {key}"))
}

pub(crate) fn strings(value: &Value, key: &str) -> R<Vec<String>> {
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

pub(crate) fn field(value: &mut Value, key: &str, data: Value) {
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
    json::read(&root.join(path))
}

#[derive(Clone)]
pub(crate) struct Case {
    pub(crate) index: usize,
    pub(crate) label: String,
    pub(crate) schedule: String,
    pub(crate) mode: String,
    /// The suite's operation name and comparison view, as its records give them.
    pub(crate) operation_name: &'static str,
    pub(crate) view: &'static str,
    /// The input's directory, from the repository root, and its file name there.
    pub(crate) directory: &'static str,
    pub(crate) input: String,
    pub(crate) input_sha: String,
    pub(crate) input_bytes: usize,
    pub(crate) refusal: bool,
    pub(crate) arguments: Vec<String>,
    pub(crate) candidate_options: Vec<String>,
    pub(crate) alias: Option<usize>,
    pub(crate) time_outputs: Value,
    pub(crate) memory_outputs: Value,
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
            operation_name: "isobmff-open-index",
            view: "ordered-trace-and-packet-fold",
            directory: FIXTURES,
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
            reference_prefix: None,
            arguments: self.arguments.iter().map(OsString::from).collect(),
            candidate_options: self.candidate_options.iter().map(OsString::from).collect(),
            // A range plan's roles each fold their own digest (harness/features/isobmff-seek).
            work: timing::Work {
                fields: Vec::new(),
                own: comparison::own_fields(Some(&self.time_outputs), &[]),
            },
            timeout: worker::DEFAULT_TIMEOUT,
        }
    }

    pub(crate) fn id(&self) -> String {
        format!("{}-{}-{}", self.label, self.mode, self.schedule)
    }

    pub(crate) fn operation_record(&self) -> Value {
        Value::object([
            ("name", Value::str(self.operation_name)),
            ("view", Value::str(self.view)),
            ("prefix", Value::str(format!("{}-", self.mode))),
            ("arguments", Value::strings(self.arguments.clone())),
            (
                "candidate_options",
                Value::strings(self.candidate_options.clone()),
            ),
        ])
    }

    pub(crate) fn path(&self, root: &Path) -> PathBuf {
        root.join(self.directory).join(&self.input)
    }

    pub(crate) fn work(&self, candidate: bool, timed: bool) -> Value {
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

    /// Part `index` of `count` of `selection`: every `count`th physical operation from
    /// the `index`th, with the aliases that take its samples.
    fn part(&self, selection: &[usize], (index, count): (usize, usize)) -> Vec<usize> {
        let physical: Vec<usize> = selection
            .iter()
            .copied()
            .filter(|&i| self.cases[i].alias.is_none())
            .collect();
        let kept: Vec<usize> = physical.into_iter().skip(index).step_by(count).collect();
        selection
            .iter()
            .copied()
            .filter(|&i| kept.contains(&self.cases[i].alias.unwrap_or(i)))
            .collect()
    }
}

/// The registered contract and its output indices assigned to this gate part.
pub(crate) fn full_part_selection(root: &Path, part: (usize, usize)) -> R<(Value, Vec<usize>)> {
    let suite = Suite::load(root)?;
    let selection = suite.part(&suite.selection(false), part);
    if selection.is_empty() {
        return Err("ISO BMFF part has no physical operations".into());
    }
    Ok((suite.contract, selection))
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

/// Every suite input matches its pin; for `cargo xtask preflight`.
pub fn verify_inputs(root: &Path) -> R<String> {
    let suite = Suite::load(root)?;
    let mut inputs: Vec<&Case> = suite.cases.iter().collect();
    inputs.dedup_by(|a, b| a.input == b.input);
    for case in &inputs {
        verify_input(root, case)?;
    }
    Ok(format!("{} cases", suite.cases.len()))
}

/// The suite's inputs as `verify_inputs` checks them: each path with its pinned
/// SHA-256, once.
pub fn inputs(root: &Path) -> R<Vec<(PathBuf, String)>> {
    let suite = Suite::load(root)?;
    let mut inputs: Vec<(PathBuf, String)> = suite
        .cases
        .iter()
        .map(|case| (case.path(root), case.input_sha.clone()))
        .collect();
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

pub(crate) fn verify_input(root: &Path, case: &Case) -> R<()> {
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

pub(crate) fn write_json(path: &Path, value: &Value) -> R<String> {
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

pub(crate) fn output_matches(case: &Case, candidate: &Finished, reference: &Finished) -> bool {
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
    trace_with(root, candidate, reference, case, directory, output_matches)
}

/// Runs both roles' traces of `case` and compares a successful pair with
/// `matches`; a refusal pair must be the contract's matching refusal.
pub(crate) fn trace_with(
    root: &Path,
    candidate: &Path,
    reference: &Path,
    case: &Case,
    directory: &Path,
    matches: fn(&Case, &Finished, &Finished) -> bool,
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
        (Ok(Some(a)), Ok(Some(b))) => matches(case, a, b),
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

pub(crate) fn native() -> bool {
    cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    ))
}

fn sampling_matches(raw: &Value, pairs: usize, seed: u64) -> bool {
    timing::planned_pairs(raw) == Some(pairs as i128)
        && number(raw, "seed").ok() == Some(i128::from(seed))
}

pub(crate) struct Options {
    pub(crate) benchmark: bool,
    pub(crate) memory: bool,
    pub(crate) subset: bool,
    pub(crate) require_acceptance: bool,
    pub(crate) output: PathBuf,
    pub(crate) pairs: usize,
    pub(crate) memory_pairs: usize,
    pub(crate) seed: u64,
    pub(crate) full: bool,
    /// Which part of the measured set to measure, counted from 0, of how many.
    pub(crate) part: Option<(usize, usize)>,
    pub(crate) early_stop: bool,
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
            pairs: timing::GATE_PAIRS,
            memory_pairs: timing::GATE_PAIRS,
            seed: 37,
            full: false,
            part: None,
            early_stop: true,
        };
        let mut args = args.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--benchmark" => options.benchmark = true,
                "--memory" => options.memory = true,
                "--subset52" => options.subset = true,
                "--require-acceptance" => options.require_acceptance = true,
                "--full" => options.full = true,
                "--part" => {
                    let value = args.next().ok_or("--part needs I/N")?;
                    options.part = Some(crate::gate::parse_part(value)?);
                }
                "--no-early-stop" => options.early_stop = false,
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
        if options.pairs < timing::GATE_PAIRS
            || options.memory_pairs < timing::GATE_PAIRS
            || options.seed > i64::MAX as u64 - 2
        {
            return Err("need at least 30 pairs and a seed below the signed 64-bit limit".into());
        }
        if options.require_acceptance
            && (options.subset || options.part.is_some() || !options.benchmark || !options.memory)
        {
            return Err("acceptance requires the full runtime and memory sets".into());
        }
        Ok(options)
    }
}

pub(crate) struct Build {
    pub(crate) candidate: PathBuf,
    pub(crate) reference: PathBuf,
    pub(crate) workers: Value,
    pub(crate) identity: Value,
    pub(crate) graph: Value,
}

impl Build {
    pub(crate) fn load(root: &Path) -> R<Self> {
        let identity = json::read(&root.join("target/harness/identity.json"))?;
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

    pub(crate) fn unchanged(&self, root: &Path) -> R<bool> {
        Ok(Self::load(root).is_ok_and(|current| {
            current.workers == self.workers
                && current.identity == self.identity
                && equal(&current.graph, &self.graph)
        }))
    }
}

pub(crate) fn case_directory(output: &Path, index: usize) -> PathBuf {
    output.join(format!("case-{index:04}"))
}

fn measurement_error(error: String) -> Value {
    Value::object([
        ("status", Value::str("runner-error")),
        ("error", Value::str(error)),
    ])
}

pub(crate) fn measure_time(
    root: &Path,
    build: &Build,
    case: &Case,
    options: &Options,
    contract: &Value,
    row: &Value,
) -> R<Value> {
    let directory = case_directory(&options.output, case.index);
    let operation = case.operation();
    let input = case.path(root);
    let mut time = Value::Null;
    if options.benchmark {
        let raw = timing::benchmark(
            &build.candidate,
            &build.reference,
            &input,
            timing::Pairs::gate(options.pairs, Runtime::load(root)?.ceiling)
                .early_stop(options.early_stop),
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
            &report::gate_target()?,
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
    Ok(time)
}

pub(crate) fn measure_memory(
    slot: &memory::Slot,
    root: &Path,
    build: &Build,
    case: &Case,
    options: &Options,
    contract: &Value,
    row: &Value,
) -> R<Value> {
    let directory = case_directory(&options.output, case.index);
    let operation = case.operation();
    let input = case.path(root);
    let mut memory_result = Value::Null;
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
            slot,
            root,
            &build.candidate,
            &build.reference,
            &input,
            timing::Pairs::gate(options.memory_pairs, Memory::load(root)?.ceiling())
                .early_stop(options.early_stop),
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
            &report::gate_target()?,
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
    Ok(memory_result)
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

pub(crate) fn passed(value: &Value) -> bool {
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
            Value::int(
                selection
                    .iter()
                    .filter(|&&index| rows[index].get("alias_of") == Some(&Value::Null))
                    .count() as i128,
            ),
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
pub(crate) fn report_case(output: &Path, row: &Value) -> R<Value> {
    crate::harness::validation::expand_measurements(output, row)
}

/// Write canonical report JSON with at most one case's raw observations in memory.
/// Publish only a complete document and never replace an existing report.
pub(crate) fn final_report(
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
    let policy = json::read(&root.join("harness/policy.json"))?;
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
    if options.benchmark || options.memory {
        let key = gate_key(&root, &options, &build)?;
        reuse::step(&root, &options.output, &key, options.full, || {
            compare(&root, &options, &suite, &build, policy)
        })
    } else {
        compare(&root, &options, &suite, &build, policy)
    }
}

/// A part as `I/N`, counted from 1, or null for the whole set.
pub(crate) fn part_value(part: Option<(usize, usize)>) -> Value {
    part.map_or(Value::Null, |(i, n)| Value::str(format!("{}/{n}", i + 1)))
}

/// The key of a measuring run: its settings, the suite's inputs and the workers.
fn gate_key(root: &Path, options: &Options, build: &Build) -> R<reuse::Key> {
    let command = match (options.benchmark, options.memory) {
        (true, false) => "isobmff-bench --benchmark",
        (false, true) => "isobmff-bench --memory",
        _ => "isobmff-bench --benchmark --memory",
    };
    let settings = Value::object([
        ("subset52", Value::Bool(options.subset)),
        ("part", part_value(options.part)),
        (
            "require_acceptance",
            Value::Bool(options.require_acceptance),
        ),
        ("pairs", Value::int(options.pairs as i128)),
        ("memory_pairs", Value::int(options.memory_pairs as i128)),
        ("seed", Value::int(options.seed)),
        ("iterations", Value::int(ITERATIONS)),
        ("early_stop", Value::Bool(options.early_stop)),
        (
            "kernel_bound",
            std::env::var("TRIERGON_RSS_KERNEL_BOUND").map_or(Value::Null, Value::str),
        ),
    ]);
    let inputs: Vec<PathBuf> = source_inputs(root)?
        .iter()
        .map(|file| root.join(file))
        .collect();
    let files: Vec<&Path> = inputs.iter().map(PathBuf::as_path).collect();
    let workers = [
        ("candidate", build.candidate.as_path()),
        ("reference", build.reference.as_path()),
    ];
    reuse::Key::new(root, command, settings, &files, &workers)
}

/// Compare every case, then measure the selected set as requested.
fn compare(root: &Path, options: &Options, suite: &Suite, build: &Build, policy: Value) -> R<i32> {
    let selection = match options.part {
        Some(part) => suite.part(&suite.selection(options.subset), part),
        None => suite.selection(options.subset),
    };
    let base = Value::object([
        ("schema", Value::int(1)),
        ("contract", Value::str("isobmff-open-index")),
        ("contract_id", Value::str("isobmff-open-index")),
        ("target", Value::str(report::gate_target()?)),
        ("counts", counts()),
        ("contract_sha256", Value::str(CONTRACT_SHA)),
        ("corpus_sha256", Value::str(CORPUS_SHA)),
        ("corpus_manifest_sha256", Value::str(CORPUS_SHA)),
        ("work_contract_sha256", Value::str(WORK_CONTRACT)),
        ("subset52_sha256", Value::str(SUBSET_SHA)),
        ("part", part_value(options.part)),
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
    checkpoint(&options.output, &base, &rows, &selection, options, true)?;
    for case in &suite.cases {
        let directory = case_directory(&options.output, case.index);
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let mut row = trace(root, &build.candidate, &build.reference, case, &directory)?;
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
    let mut unchanged = build.unchanged(root)?;
    checkpoint(
        &options.output,
        &base,
        &rows,
        &selection,
        options,
        unchanged,
    )?;
    if all_parity && unchanged && (options.benchmark || options.memory) {
        // Every operation is timed before any memory runs; memory then fills the
        // CPUs, and aliases take the samples of the operation they repeat.
        let mut measured = Vec::new();
        for &index in &selection {
            let case = &suite.cases[index];
            if let Some(prior) = case.alias {
                if !selection.contains(&prior)
                    || !alias_traces_match(&options.output, index, prior)?
                {
                    return Err(format!(
                        "alias {index} has no identical selected operation {prior}"
                    ));
                }
                continue;
            }
            if let Err(error) = verify_input(root, case) {
                unchanged = false;
                field(&mut rows[index], "measurement_error", Value::str(error));
                checkpoint(&options.output, &base, &rows, &selection, options, false)?;
                break;
            }
            let time = measure_time(root, build, case, options, &suite.contract, &rows[index])?;
            if time != Value::Null {
                field(&mut rows[index], "timing", time);
                checkpoint(
                    &options.output,
                    &base,
                    &rows,
                    &selection,
                    options,
                    unchanged,
                )?;
            }
            measured.push(index);
        }
        if options.memory && unchanged {
            let workers = memory::WorkerPaths {
                candidate: &build.candidate,
                reference: &build.reference,
                registered: true,
            };
            let jobs = measured
                .iter()
                .map(|&index| {
                    let (case, row) = (&suite.cases[index], rows[index].clone());
                    let (root, build, options) = (&root, &build, &options);
                    let contract = &suite.contract;
                    memory::Job::new(&workers, [case.path(root)], move |slot| {
                        verify_input(root, case)?;
                        measure_memory(slot, root, build, case, options, contract, &row)
                    })
                })
                .collect();
            for (&index, memory) in measured.iter().zip(memory::collect_all(jobs)?) {
                let memory = memory?;
                if memory != Value::Null {
                    field(&mut rows[index], "memory", memory);
                }
            }
        }
        for &index in &selection {
            let case = &suite.cases[index];
            if let Some(prior) = case.alias {
                if !unchanged {
                    break;
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
                for key in ["timing", "memory"] {
                    if let Some(sample) = rows[prior].get(key).filter(|v| **v != Value::Null) {
                        let sample = sample.clone();
                        field(&mut rows[index], key, sample);
                    }
                }
            } else if !measured.contains(&index) {
                continue;
            }
            write_json(
                &case_directory(&options.output, index).join("result.json"),
                &rows[index],
            )?;
            println!("{}: {} {} {}", index, case.label, case.mode, case.schedule);
        }
        checkpoint(
            &options.output,
            &base,
            &rows,
            &selection,
            options,
            unchanged,
        )?;
    }
    unchanged &= build.unchanged(root)?;
    let mut summary = checkpoint(
        &options.output,
        &base,
        &rows,
        &selection,
        options,
        unchanged,
    )?;
    let observations = rows.iter().map(|row| report_case(&options.output, row));
    let errors = (!unchanged).then(|| Err("source or worker changed during collection".to_owned()));
    let verdict = acceptance::acceptance_iter(
        root,
        &suite.contract,
        observations.chain(errors),
        &build.identity,
        &report::gate_target()?,
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
mod tests;
