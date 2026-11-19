//! ISO BMFF seek and range-plan comparisons.
//!
//! A seek case opens a preloaded input, optionally reads every packet to the end,
//! seeks backward on one track to a time in its media clock and reads a bounded
//! suffix; its ordered traces must be byte-identical. A range-plan case runs the
//! candidate's recovery-aware source range plan for the same request and reads the
//! plan's samples, against the reference's backward seek reading as many packets;
//! the delivered source positions and payloads must be identical, packet for
//! packet. Matching refusals are parity outcomes and are not timed. Every other
//! successful case is timed and measured with the ISO BMFF suite's harness.

mod fixtures;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::checks::read_json;
use crate::harness::worker::Finished;
use crate::harness::{
    acceptance, comparison, gate_build, memory, provenance, report, reuse, timing,
};
use crate::isobmff_bench::{
    self as bench, Build, Case, ITERATIONS, Options, REFERENCE, WORK_CONTRACT, field, integer,
    number, strings, text,
};
use crate::json::{self, Value};

type R<T> = Result<T, String>;

pub const USAGE: &str = "usage: cargo xtask isobmff-seek [--benchmark] [--memory] [--output DIR]
    [--pairs N] [--memory-pairs N] [--seed N] [--require-acceptance] [--full]
    [--part I/N] [--no-early-stop]
       cargo xtask isobmff-seek fixtures --out DIR

Compares every seek and range-plan case of harness/features/isobmff-seek; with
--benchmark and --memory, times and measures every case that is not a refusal;
--part I/N measures every Nth of them from the Ith, retaining every comparison.
`fixtures` writes the authored movies of harness/fixtures/isobmff-seek to DIR.
Workers must belong to the current recorded gate build. For timing, keep the RSS
kernel module unloaded and TRIERGON_RSS_KERNEL_BOUND unset or 0.";

pub const FEATURE: &str = "harness/features/isobmff-seek";
pub const FIXTURES: &str = "harness/fixtures/isobmff-seek";
const OPEN_INDEX_FIXTURES: &str = "harness/fixtures/isobmff-open-index";
const CONTRACT_SHA: &str = "15289cd313b5fa37b8534383dcb1239cbe56116cb4d71233c9edeb08a1f6119f";
const CORPUS_SHA: &str = "058257219f1ca3c27d0a09d8a0951776227e84ccfd704f7eca4c93dfa988d0d5";
/// Cases: seeks, range plans and refusals, and the distinct inputs they read.
const SEEKS: usize = 378;
const RANGE_PLANS: usize = 94;
const REFUSALS: usize = 4;
const INPUTS: usize = 44;
const CASES: usize = SEEKS + RANGE_PLANS + REFUSALS;
/// The candidate option that turns a seek mode into a range plan.
const RANGE_PLAN: &str = "--range-plan";
const COMPATIBILITY: [&str; 3] = [
    "--bounded-timing",
    "--bounded-sync",
    "--terminal-media-prefix",
];

fn counts() -> Value {
    Value::object([
        ("cases", Value::int(CASES as i128)),
        ("seeks", Value::int(SEEKS as i128)),
        ("range_plans", Value::int(RANGE_PLANS as i128)),
        ("explicit_refusals", Value::int(REFUSALS as i128)),
        ("timable_outputs", Value::int((SEEKS + RANGE_PLANS) as i128)),
        ("distinct_inputs", Value::int(INPUTS as i128)),
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

fn is_range_plan(case: &Case) -> bool {
    case.candidate_options.iter().any(|o| o == RANGE_PLAN)
}

/// One registered case, checked against the suite's rules.
fn parse(value: &Value, index: usize) -> R<Case> {
    let invalid = |what: &str| format!("case {index}: {what}");
    if number(value, "index")? != index as i128 {
        return Err("case indices must be complete and ordered".into());
    }
    let input = value.get("input").ok_or_else(|| invalid("missing input"))?;
    let refusal = match text(value, "outcome")? {
        "output" => false,
        "refusal" => true,
        _ => return Err(invalid("unknown outcome")),
    };
    if number(value, "reference_exit_code_in_trace")? != i128::from(refusal) {
        return Err(invalid("inconsistent refusal exit code"));
    }
    let directory = match text(input, "directory")? {
        FIXTURES => FIXTURES,
        OPEN_INDEX_FIXTURES => OPEN_INDEX_FIXTURES,
        _ => return Err(invalid("input outside the suite's fixture directories")),
    };
    let candidate_options = strings(value, "candidate_leading_arguments")?;
    let range_plan = match text(value, "request")? {
        "seek" => false,
        "range-plan" => true,
        _ => return Err(invalid("unknown request")),
    };
    let case = Case {
        index,
        label: text(value, "case")?.into(),
        schedule: text(value, "schedule")?.into(),
        mode: text(value, "mode")?.into(),
        operation_name: "isobmff-seek",
        view: if range_plan {
            "range-plan-source-packets"
        } else {
            "ordered-trace-and-packet-fold"
        },
        directory,
        input: text(input, "path")?.into(),
        input_sha: text(input, "sha256")?.into(),
        input_bytes: usize::try_from(number(input, "bytes")?).map_err(|e| e.to_string())?,
        refusal,
        arguments: strings(value, "arguments")?,
        candidate_options,
        alias: None,
        time_outputs: value
            .get("expected_time_outputs")
            .cloned()
            .unwrap_or(Value::Null),
        memory_outputs: value
            .get("expected_memory_outputs")
            .cloned()
            .unwrap_or(Value::Null),
    };
    if value.get("alias_of") != Some(&Value::Null) {
        return Err(invalid("seek cases have no aliases"));
    }
    let schedule = match case.schedule.as_str() {
        "ordinary" => ["65536", "0", "memory", "4096"],
        "tiny" => ["1", "0", "memory", "3"],
        _ => return Err(invalid("unknown schedule")),
    };
    let request = case.arguments.get(..4).unwrap_or_default();
    let integers = request
        .iter()
        .map(|a| a.parse::<i64>().ok())
        .collect::<Option<Vec<_>>>()
        .filter(|values| values.len() == 4);
    let options_valid = if range_plan {
        case.candidate_options == [RANGE_PLAN]
    } else {
        case.candidate_options.is_empty() || case.candidate_options == COMPATIBILITY
    };
    let input_valid = match directory {
        OPEN_INDEX_FIXTURES => case.input == format!("{}.mov", case.input_sha),
        _ => fixtures::all().iter().any(|(name, _)| *name == case.input),
    };
    if case.mode != "seek"
        || case.arguments.get(4..) != Some(schedule.map(str::to_owned).as_slice())
        || !integers.is_some_and(|v| {
            i32::try_from(v[0]).is_ok() && (0..=64).contains(&v[2]) && (0..=1).contains(&v[3])
        })
        || (range_plan && (refusal || case.schedule != "ordinary"))
        || !options_valid
        || !input_valid
        || case.input_sha.len() != 64
        || case.input_bytes == 0
    {
        return Err(invalid("invalid operation"));
    }
    if text(value, "id")? != case.id()
        || text(value, "profile")? != case.id()
        || text(value, "sha256")? != case.input_sha
        || text(value, "file")? != case.input
        || value.get("operation") != Some(&case.operation_record())
    {
        return Err(invalid("inconsistent registered case"));
    }
    if refusal {
        if case.time_outputs != Value::Null || case.memory_outputs != Value::Null {
            return Err(invalid("a refusal has no expected work"));
        }
        return Ok(case);
    }
    check_work(&case).map_err(|e| invalid(&e))?;
    Ok(case)
}

/// The recorded work of each role: the reference's proof fields, the candidate's
/// projection of them, fifty iterations composed from one, and, for a range plan,
/// the same packets and bytes on both sides.
fn check_work(case: &Case) -> R<()> {
    for (timed, iterations) in [(true, ITERATIONS), (false, 1)] {
        let reference = case.work(false, timed);
        let candidate = case.work(true, timed);
        if number(&reference, "schema")? != 1
            || number(&reference, "elapsed_ns")? != 0
            || number(&reference, "iterations")? != i128::from(iterations)
            || text(&reference, "input_sha256")? != case.input_sha
            || number(&reference, "input_bytes")? != case.input_bytes as i128
            || text(&reference, "contract_sha256")? != WORK_CONTRACT
        {
            return Err("invalid reference work metadata".into());
        }
        let Value::Object(fields) = &reference else {
            return Err("expected reference work object".into());
        };
        let projected: Vec<_> = fields
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
            .collect();
        let mut expected = Value::Object(projected);
        if is_range_plan(case) {
            field(
                &mut expected,
                "digest",
                candidate.get("digest").cloned().unwrap_or(Value::Null),
            );
        }
        if candidate != expected {
            return Err("invalid candidate work projection".into());
        }
        for key in ["packets", "bytes", "digest"] {
            u64::try_from(number(&candidate, key)?).map_err(|e| e.to_string())?;
        }
    }
    for role in [true, false] {
        let one =
            u64::try_from(number(&case.work(role, false), "digest")?).map_err(|e| e.to_string())?;
        let digest =
            (0..ITERATIONS).fold(0u64, |total, _| total.wrapping_mul(31).wrapping_add(one));
        let timed = case.work(role, true);
        if number(&timed, "digest")? != i128::from(digest)
            || ["packets", "bytes"].iter().any(|key| {
                number(&timed, key).ok()
                    != number(&case.work(role, false), key)
                        .ok()
                        .map(|v| v * i128::from(ITERATIONS))
            })
        {
            return Err("fifty iterations do not compose from one".into());
        }
    }
    Ok(())
}

struct Suite {
    contract: Value,
    cases: Vec<Case>,
}

impl Suite {
    fn load(root: &Path) -> R<Self> {
        let contract = document(root, "contract", CONTRACT_SHA)?;
        let corpus = document(root, "corpus", CORPUS_SHA)?;
        if contract.get("counts") != Some(&counts()) || corpus.get("counts") != Some(&counts()) {
            return Err("inconsistent seek corpus counts".into());
        }
        let cases = corpus
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("missing cases")?
            .iter()
            .enumerate()
            .map(|(i, v)| parse(v, i))
            .collect::<R<Vec<_>>>()?;
        let suite = Self { contract, cases };
        suite.validate()?;
        Ok(suite)
    }

    fn validate(&self) -> R<()> {
        let profiles: Vec<_> = self.cases.iter().map(|c| Value::str(c.id())).collect();
        let refusals: Vec<_> = self
            .cases
            .iter()
            .filter(|c| c.refusal)
            .map(|c| {
                Value::object([
                    ("id", Value::str(c.id())),
                    ("profile", Value::str(c.id())),
                    ("sha256", Value::str(&c.input_sha)),
                ])
            })
            .collect();
        if self.contract.get("required_profiles") != Some(&Value::Array(profiles))
            || self.contract.get("required_refusals") != Some(&Value::Array(refusals))
            || self.contract.get("case_operations") != Some(&Value::Bool(true))
        {
            return Err("contract outcomes differ from the seek corpus".into());
        }
        let mut operations = BTreeSet::new();
        let mut inputs = BTreeSet::new();
        for case in &self.cases {
            let key = json::canonical(&Value::object([
                ("input", Value::str(&case.input_sha)),
                ("arguments", Value::strings(case.arguments.clone())),
                ("options", Value::strings(case.candidate_options.clone())),
            ]));
            if !operations.insert(key) {
                return Err(format!("case {} repeats an operation", case.index));
            }
            inputs.insert(&case.input_sha);
        }
        let plans = self.cases.iter().filter(|c| is_range_plan(c)).count();
        let refusals = self.cases.iter().filter(|c| c.refusal).count();
        if self.cases.len() != CASES
            || plans != RANGE_PLANS
            || refusals != REFUSALS
            || inputs.len() != INPUTS
        {
            return Err("incomplete ISO BMFF seek membership".into());
        }
        Ok(())
    }

    /// The cases measured: every one that is not a refusal, or of `part` (counted
    /// from 0, of how many) every Nth of them from the Ith.
    fn timed(&self, part: Option<(usize, usize)>) -> Vec<usize> {
        let (first, step) = part.unwrap_or((0, 1));
        self.cases
            .iter()
            .filter(|c| !c.refusal)
            .map(|c| c.index)
            .skip(first)
            .step_by(step)
            .collect()
    }
}

/// The registered seek outputs assigned to a gate collection.
pub(crate) fn measurement_selection(root: &Path, part: Option<(usize, usize)>) -> R<Vec<usize>> {
    Ok(Suite::load(root)?.timed(part))
}

/// The suite's harness inputs, bound to the ISO BMFF worker's build.
pub fn source_inputs(root: &Path) -> R<Vec<String>> {
    let suite = Suite::load(root)?;
    let mut files: Vec<String> = [
        "xtask/src/isobmff_seek/mod.rs",
        "xtask/src/isobmff_seek/fixtures.rs",
        "xtask/src/isobmff_bench.rs",
        "harness/reference_isobmff.c",
    ]
    .map(str::to_owned)
    .to_vec();
    files.extend(["contract", "corpus"].map(|name| format!("{FEATURE}/{name}.json")));
    files.extend(
        suite
            .cases
            .iter()
            .map(|case| format!("{}/{}", case.directory, case.input)),
    );
    files.sort();
    files.dedup();
    Ok(files)
}

/// Every input matches its pin; for `cargo xtask preflight --isobmff-seek`.
pub fn verify_inputs(root: &Path) -> R<String> {
    let suite = Suite::load(root)?;
    for case in &suite.cases {
        bench::verify_input(root, case)?;
    }
    Ok(format!("{} cases", suite.cases.len()))
}

/// The inputs `verify_inputs` checks, each with its pinned SHA-256, once.
pub fn inputs(root: &Path) -> R<Vec<(PathBuf, String)>> {
    let suite = Suite::load(root)?;
    let mut inputs: Vec<_> = suite
        .cases
        .iter()
        .map(|case| (case.path(root), case.input_sha.clone()))
        .collect();
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

/// A range plan's samples against the reference's packets after its backward
/// seek: the same count as the case requests, all of the requested track, with
/// the same source position, size and payload in order, and each role's recorded
/// work.
fn range_plan_matches(case: &Case, candidate: &Finished, reference: &Finished) -> bool {
    let parse = |done: &Finished| {
        (done.code == 0)
            .then(|| std::str::from_utf8(&done.stdout).ok())
            .flatten()
            .and_then(|text| json::parse(text).ok())
    };
    let (Some(plan), Some(seek)) = (parse(candidate), parse(reference)) else {
        return false;
    };
    let work = |trace: &Value, role: bool| {
        trace.get("work").is_some_and(|work| {
            ["packets", "bytes", "digest"]
                .iter()
                .all(|k| work.get(k) == case.work(role, false).get(k))
        })
    };
    let track = case.arguments[0].parse::<i128>().ok();
    let count = case.arguments[2].parse::<usize>().ok();
    let packets = |trace: &Value| {
        trace
            .get("packets")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .to_vec()
    };
    let (planned, delivered) = (packets(&plan), packets(&seek));
    plan.get("operation").and_then(Value::as_str) == Some("source-range-plan-consumer-v33")
        && plan.get("status").and_then(Value::as_str) == Some("Planned")
        && plan.get("plan_complete") == Some(&Value::Bool(true))
        && seek.get("operation").and_then(Value::as_str) == Some("mov-native-seek-consumer-v33")
        && seek.get("seek_result").and_then(integer) == Some(0)
        && seek.get("delivery_limit_reached") == Some(&Value::Bool(true))
        && work(&plan, true)
        && work(&seek, false)
        && Some(planned.len()) == count
        && planned.len() == delivered.len()
        && planned.iter().zip(&delivered).all(|(p, d)| {
            let payload = p.get("payload").and_then(Value::as_str);
            d.get("pid").and_then(integer) == track
                && p.get("position").and_then(integer).is_some()
                && p.get("position").and_then(integer) == d.get("position").and_then(integer)
                && payload.is_some()
                && payload == d.get("payload").and_then(Value::as_str)
                && p.get("size").and_then(integer) == payload.map(|hex| hex.len() as i128 / 2)
        })
}

fn matches(case: &Case, candidate: &Finished, reference: &Finished) -> bool {
    if is_range_plan(case) {
        range_plan_matches(case, candidate, reference)
    } else {
        bench::output_matches(case, candidate, reference)
    }
}

fn options(root: &Path, args: &[String]) -> R<Options> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let mut options = Options {
        benchmark: false,
        memory: false,
        subset: false,
        require_acceptance: false,
        output: root.join(format!(
            "target/isobmff-seek/{stamp}-{}",
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
    let count = |value: Option<&String>, what: &str| -> R<usize> {
        value
            .ok_or_else(|| format!("{what} needs a count"))?
            .parse()
            .map_err(|e| format!("{what}: {e}"))
    };
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--benchmark" => options.benchmark = true,
            "--memory" => options.memory = true,
            "--require-acceptance" => options.require_acceptance = true,
            "--full" => options.full = true,
            "--no-early-stop" => options.early_stop = false,
            "--part" => {
                let value = args.next().ok_or("--part needs I/N")?;
                options.part = Some(crate::gate::parse_part(value)?);
            }
            "--output" => {
                options.output = root.join(args.next().ok_or("--output needs a directory")?);
            }
            "--pairs" => options.pairs = count(args.next(), "--pairs")?,
            "--memory-pairs" => options.memory_pairs = count(args.next(), "--memory-pairs")?,
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
        && (options.part.is_some() || !(options.benchmark && options.memory))
    {
        return Err("acceptance requires the complete runtime and memory sets".into());
    }
    Ok(options)
}

/// The ISO BMFF build, with this suite's inputs bound to it too.
fn build(root: &Path) -> R<Build> {
    let build = Build::load(root)?;
    let bindings = build
        .graph
        .get("shared_files")
        .ok_or("missing ISO BMFF harness bindings")?;
    for path in source_inputs(root)? {
        let (name, digest) = provenance::file_identity(root, Path::new(&path))?;
        if bindings.get(&name).and_then(Value::as_str) != Some(digest.as_str()) {
            return Err(format!(
                "ISO BMFF seek input is not bound to the build: {name}; rebuild workers"
            ));
        }
    }
    Ok(build)
}

pub fn main(root: &Path, args: &[String]) -> R<i32> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    if args.first().map(String::as_str) == Some("fixtures") {
        let [_, flag, out] = args else {
            return Err(USAGE.into());
        };
        if flag != "--out" {
            return Err(USAGE.into());
        }
        let out = root.join(out);
        fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        for (name, bytes) in fixtures::all() {
            let path = out.join(name);
            fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("{} {name}", crate::sha256::hex(&bytes));
        }
        return Ok(0);
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let options = options(&root, args)?;
    let suite = Suite::load(&root)?;
    let policy = read_json(&root.join("harness/policy.json"))?;
    let build = build(&root)?;
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

/// The key of a measuring run: its settings, the suite's inputs and the workers.
fn gate_key(root: &Path, options: &Options, build: &Build) -> R<reuse::Key> {
    let command = match (options.benchmark, options.memory) {
        (true, false) => "isobmff-seek --benchmark",
        (false, true) => "isobmff-seek --memory",
        _ => "isobmff-seek --benchmark --memory",
    };
    let settings = Value::object([
        ("part", bench::part_value(options.part)),
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

fn summary(rows: &[Value], timed: &[usize], options: &Options, unchanged: bool) -> Value {
    let matched = |r: &&Value| r.get("match") == Some(&Value::Bool(true));
    let completed = rows.iter().filter(|r| r.get("match").is_some()).count();
    let parity_matches = rows.iter().filter(matched).count();
    let refusal_matches = rows
        .iter()
        .filter(|r| matched(r) && r.get("outcome").and_then(Value::as_str) == Some("refusal"))
        .count();
    let passes = |kind: &str| {
        timed
            .iter()
            .filter(|&&i| rows[i].get(kind).is_some_and(bench::passed))
            .count()
    };
    let collected = |kind: &str| {
        timed
            .iter()
            .filter(|&&i| rows[i].get(kind).is_some_and(|v| *v != Value::Null))
            .count()
    };
    let collection_complete = completed == CASES
        && (!options.benchmark || collected("timing") == timed.len())
        && (!options.memory || collected("memory") == timed.len());
    let parity_complete = rows.len() == CASES && parity_matches == CASES;
    let all_requested = parity_complete
        && unchanged
        && (!options.benchmark || passes("timing") == timed.len())
        && (!options.memory || passes("memory") == timed.len());
    Value::object([
        ("parity_matches", Value::int(parity_matches as i128)),
        ("parity_checks_completed", Value::int(completed as i128)),
        ("collection_complete", Value::Bool(collection_complete)),
        ("parity_complete", Value::Bool(parity_complete)),
        (
            "output_matches",
            Value::int((parity_matches - refusal_matches) as i128),
        ),
        ("refusal_matches", Value::int(refusal_matches as i128)),
        ("timable_outputs", Value::int(timed.len() as i128)),
        ("explicit_refusals", Value::int(REFUSALS as i128)),
        ("timing_passes", Value::int(passes("timing") as i128)),
        ("memory_passes", Value::int(passes("memory") as i128)),
        ("source_and_workers_unchanged", Value::Bool(unchanged)),
        ("all_requested_checks_pass", Value::Bool(all_requested)),
    ])
}

fn checkpoint(
    output: &Path,
    base: &Value,
    rows: &[Value],
    timed: &[usize],
    options: &Options,
    unchanged: bool,
) -> R<Value> {
    let mut report = base.clone();
    let summary = summary(rows, timed, options, unchanged);
    let complete = summary.get("collection_complete") == Some(&Value::Bool(true));
    field(&mut report, "summary", summary.clone());
    field(&mut report, "cases", Value::Array(rows.to_vec()));
    field(&mut report, "case_storage", Value::str("sha256-files"));
    field(
        &mut report,
        "collection_status",
        Value::str(if complete { "complete" } else { "partial" }),
    );
    fs::write(
        output.join("partial.json"),
        format!("{}\n", json::canonical(&report)),
    )
    .map_err(|e| e.to_string())?;
    Ok(summary)
}

/// Compare every case, then time and measure every successful one as requested.
fn compare(root: &Path, options: &Options, suite: &Suite, build: &Build, policy: Value) -> R<i32> {
    let timed = suite.timed(options.part);
    let base = Value::object([
        ("schema", Value::int(1)),
        ("contract", Value::str("isobmff-seek")),
        ("contract_id", Value::str("isobmff-seek")),
        ("target", Value::str(report::gate_target()?)),
        ("counts", counts()),
        ("contract_sha256", Value::str(CONTRACT_SHA)),
        ("corpus_sha256", Value::str(CORPUS_SHA)),
        ("corpus_manifest_sha256", Value::str(CORPUS_SHA)),
        ("work_contract_sha256", Value::str(WORK_CONTRACT)),
        ("part", bench::part_value(options.part)),
        (
            "selection",
            Value::Array(timed.iter().map(|&i| Value::int(i as i128)).collect()),
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
        ("native_aarch64_linux", Value::Bool(bench::native())),
        ("benchmark_requested", Value::Bool(options.benchmark)),
        ("memory_requested", Value::Bool(options.memory)),
        ("pairs", Value::int(options.pairs as i128)),
        ("memory_pairs", Value::int(options.memory_pairs as i128)),
        ("iterations", Value::int(ITERATIONS)),
        ("seed", Value::int(options.seed)),
    ]);
    bench::write_json(&options.output.join("identity.json"), &base)?;
    let reference =
        comparison::reference_summary(&comparison::reference_named(&build.identity, REFERENCE)?);
    let mut rows = Vec::with_capacity(suite.cases.len());
    for case in &suite.cases {
        let directory = bench::case_directory(&options.output, case.index);
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let mut row = bench::trace_with(
            root,
            &build.candidate,
            &build.reference,
            case,
            &directory,
            matches,
        )?;
        field(&mut row, "reference", reference.clone());
        bench::write_json(&directory.join("parity.json"), &row)?;
        println!(
            "{}: {} {}",
            case.index,
            case.id(),
            row.get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
        );
        rows.push(row);
    }
    let all_parity = rows
        .iter()
        .all(|r| r.get("match") == Some(&Value::Bool(true)));
    let mut unchanged = build.unchanged(root)?;
    checkpoint(&options.output, &base, &rows, &timed, options, unchanged)?;
    if all_parity && unchanged && (options.benchmark || options.memory) {
        // Every case is timed before any memory runs; memory then fills the CPUs.
        for &index in &timed {
            let case = &suite.cases[index];
            if let Err(error) = bench::verify_input(root, case) {
                unchanged = false;
                field(&mut rows[index], "measurement_error", Value::str(error));
                break;
            }
            let time =
                bench::measure_time(root, build, case, options, &suite.contract, &rows[index])?;
            if time != Value::Null {
                field(&mut rows[index], "timing", time);
                checkpoint(&options.output, &base, &rows, &timed, options, unchanged)?;
            }
        }
        if options.memory && unchanged {
            let workers = memory::WorkerPaths {
                candidate: &build.candidate,
                reference: &build.reference,
                registered: true,
            };
            let jobs = timed
                .iter()
                .map(|&index| {
                    let (case, row) = (&suite.cases[index], rows[index].clone());
                    let contract = &suite.contract;
                    memory::Job::new(&workers, [case.path(root)], move |slot| {
                        bench::verify_input(root, case)?;
                        bench::measure_memory(slot, root, build, case, options, contract, &row)
                    })
                })
                .collect();
            for (&index, memory) in timed.iter().zip(memory::collect_all(jobs)?) {
                let memory = memory?;
                if memory != Value::Null {
                    field(&mut rows[index], "memory", memory);
                }
            }
        }
        for &index in &timed {
            bench::write_json(
                &bench::case_directory(&options.output, index).join("result.json"),
                &rows[index],
            )?;
        }
    }
    unchanged &= build.unchanged(root)?;
    let mut summary = checkpoint(&options.output, &base, &rows, &timed, options, unchanged)?;
    let observations = rows
        .iter()
        .map(|row| bench::report_case(&options.output, row));
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
    bench::final_report(&options.output, &base, &rows, &summary, &verdict)?;
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

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn committed_fixtures_are_the_authored_movies() {
        let root = root();
        let authored = fixtures::all();
        let mut committed: Vec<String> = fs::read_dir(root.join(FIXTURES))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        committed.sort();
        let mut names: Vec<String> = authored.iter().map(|(n, _)| (*n).to_owned()).collect();
        names.sort();
        assert_eq!(committed, names);
        for (name, bytes) in authored {
            assert_eq!(
                fs::read(root.join(FIXTURES).join(name)).unwrap(),
                bytes,
                "{name}"
            );
        }
    }

    #[test]
    fn the_corpus_keeps_every_seek_range_plan_and_refusal() {
        let suite = Suite::load(&root()).unwrap();
        assert_eq!(suite.cases.len(), CASES);
        assert_eq!(suite.timed(None).len(), SEEKS + RANGE_PLANS);
        assert_eq!(
            suite.cases.iter().filter(|c| is_range_plan(c)).count(),
            RANGE_PLANS
        );
        assert_eq!(suite.cases.iter().filter(|c| c.refusal).count(), REFUSALS);
        assert!(source_inputs(&root()).is_ok());
        assert_eq!(inputs(&root()).unwrap().len(), INPUTS);
    }

    #[test]
    fn a_range_plan_passes_only_with_identical_source_packets() {
        let suite = Suite::load(&root()).unwrap();
        let case = suite.cases.iter().find(|c| is_range_plan(c)).unwrap();
        let work = |role| {
            let w = case.work(role, false);
            format!(
                "\"work\":{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                integer(w.get("packets").unwrap()).unwrap(),
                integer(w.get("bytes").unwrap()).unwrap(),
                integer(w.get("digest").unwrap()).unwrap()
            )
        };
        let count: usize = case.arguments[2].parse().unwrap();
        let track = &case.arguments[0];
        let plan_rows: Vec<String> = (0..count)
            .map(|i| {
                format!(
                    "{{\"ordinal\":{i},\"position\":{},\"size\":1,\"payload\":\"0{i}\"}}",
                    100 + i
                )
            })
            .collect();
        let seek_rows: Vec<String> = (0..count)
            .map(|i| {
                format!(
                    "{{\"pid\":{track},\"position\":{},\"payload\":\"0{i}\"}}",
                    100 + i
                )
            })
            .collect();
        let finished = |text: String| Finished {
            code: 0,
            stdout: text.into_bytes(),
            stderr: Vec::new(),
        };
        let plan = |rows: &[String]| {
            finished(format!(
                "{{\"operation\":\"source-range-plan-consumer-v33\",\"status\":\"Planned\",\"packets\":[{}],\"plan_complete\":true,{}}}",
                rows.join(","),
                work(true)
            ))
        };
        let seek = |rows: &[String]| {
            finished(format!(
                "{{\"operation\":\"mov-native-seek-consumer-v33\",\"seek_result\":0,\"packets\":[{}],\"delivery_limit_reached\":true,{}}}",
                rows.join(","),
                work(false)
            ))
        };
        assert!(range_plan_matches(
            case,
            &plan(&plan_rows),
            &seek(&seek_rows)
        ));
        let mut moved = seek_rows.clone();
        moved[count - 1] = moved[count - 1].replace("\"position\":", "\"position\":1");
        assert!(!range_plan_matches(case, &plan(&plan_rows), &seek(&moved)));
        assert!(!range_plan_matches(
            case,
            &plan(&plan_rows[1..]),
            &seek(&seek_rows[1..])
        ));
    }
}
