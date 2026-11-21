//! `gate run`: the steps of one shard in one stage, each with its log and record.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::json::Value;

use super::plan::CAPTURE_QUALIFY;
use super::{Item, Placement, R, SHARD, Stage, Step, git, owned, parse_part, shard_items, status};

/// The CPUs a memory collection uses: every online CPU but CPU 0.
fn memory_cpus() -> R<String> {
    let online = fs::read_to_string("/sys/devices/system/cpu/online")
        .map_err(|e| format!("/sys/devices/system/cpu/online: {e}"))?;
    let last = online
        .trim()
        .rsplit([',', '-'])
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|&n| n >= 1)
        .ok_or_else(|| format!("cannot read the online CPUs: {online}"))?;
    Ok(format!("1-{last}"))
}

fn append(path: &Path, line: &str) -> R<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(file, "{} {line}", crate::python::utc_now()).map_err(|e| e.to_string())
}

/// Runs the steps of one stage of a shard; returns 1 if any failed.
pub(super) fn run(
    root: &Path,
    output: &Path,
    stage: Stage,
    shard: (usize, usize),
    full: bool,
    no_early_stop: bool,
    selection: Option<&[String]>,
) -> R<i32> {
    fs::create_dir_all(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let output = output
        .canonicalize()
        .map_err(|e| format!("{}: {e}", output.display()))?;
    let out = output.display().to_string();
    let mut items = shard_items(root, &out, full, shard, selection)?;
    if no_early_stop {
        for step in items.iter_mut().flat_map(|item| &mut item.steps) {
            if step
                .command
                .get(1)
                .is_some_and(|command| command == "report")
            {
                step.command.push("--no-early-stop".into());
            }
        }
    }
    // A stage's records describe this run of it alone: the timing stage starts a
    // run afresh, and the memory stage replaces only its own steps' records.
    let records = output.join("steps");
    if stage == Stage::Timing {
        let _ = fs::remove_dir_all(&records);
    } else {
        for step in items.iter().flat_map(|item| &item.steps) {
            if step.stage == stage {
                let _ = fs::remove_file(records.join(format!("{}.json", step.name)));
            }
        }
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = output.join("run.log");
    let revision = git(root, &["rev-parse", "HEAD"])?;
    let changes = git(root, &["status", "--short", "--untracked-files=no"])?;
    crate::corpus::write_json(
        &output.join(SHARD),
        &Value::object([
            ("index", Value::int(shard.0 as i64 + 1)),
            ("count", Value::int(shard.1 as i64)),
            ("revision", Value::str(&revision)),
            ("changes", Value::str(&changes)),
            ("target", Value::str(crate::harness::report::gate_target()?)),
            ("output", Value::str(&out)),
            (
                "items",
                Value::strings(items.iter().map(|item| item.name.clone())),
            ),
            (
                "selection",
                selection.map_or(Value::Null, |names| Value::strings(names.iter().cloned())),
            ),
        ]),
    )?;
    let identity = root.join("target/harness/identity.json");
    fs::copy(
        &identity,
        output.join(format!("identity-{}.json", stage.name())),
    )
    .map_err(|e| format!("{}: {e}", identity.display()))?;
    append(&log, &format!("{} {revision}", stage.name()))?;
    let memory = memory_cpus()?;
    let steps: Vec<(&Item, &Step)> = items
        .iter()
        .flat_map(|item| item.steps.iter().map(move |step| (item, step)))
        .filter(|(_, step)| step.stage == stage)
        .collect();
    let mut failed = 0;
    // Parity before timing, memory before grades, item by item; a batch where its
    // first step stands.
    for placement in [Placement::Free, Placement::Timed, Placement::Memory] {
        let cpus = match placement {
            Placement::Timed => Some("1"),
            Placement::Memory => Some(memory.as_str()),
            Placement::Free | Placement::Grade => None,
        };
        let placed: Vec<(&Item, &Step)> = steps
            .iter()
            .filter(|(_, s)| s.placement == placement)
            .copied()
            .collect();
        let mut batches_run: Vec<&str> = Vec::new();
        for &(item, step) in &placed {
            let batch: Vec<(&Item, &Step)> = match step.batch {
                Some(name) if batches_run.contains(&name) => continue,
                Some(name) => {
                    batches_run.push(name);
                    placed
                        .iter()
                        .filter(|(_, s)| s.batch == Some(name))
                        .copied()
                        .collect()
                }
                None => Vec::new(),
            };
            if batch.len() > 1 {
                failed += run_batch(root, &exe, &output, &log, &batch, cpus)?;
                continue;
            }
            let mut command = command(root, &exe, &step.command, &step.environment, cpus);
            if run_step(&output, &log, item, step, cpus, &mut command)? != 0 {
                failed += 1;
            }
        }
    }
    // Grades read finished collections and measure nothing: several at once.
    let grades: Vec<_> = steps
        .iter()
        .filter(|(_, s)| s.placement == Placement::Grade)
        .collect();
    let grader = if grades.is_empty() {
        exe.clone()
    } else {
        release_xtask(root, &output)?
    };
    let width = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    for batch in grades.chunks(width.max(1)) {
        let results: Vec<R<i32>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|(item, step)| {
                    let (output, log, grader) = (&output, &log, &grader);
                    scope.spawn(move || {
                        let mut command =
                            command(root, grader, &step.command, &step.environment, None);
                        run_step(output, log, item, step, None, &mut command)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("grade panicked".into())))
                .collect()
        });
        for result in results {
            if result? != 0 {
                failed += 1;
            }
        }
    }
    append(&log, &format!("{} done, {failed} failed", stage.name()))?;
    Ok(i32::from(failed > 0))
}

/// This program built with the release profile, which grades faster than the
/// development build `cargo xtask` runs; the build's log goes to DIR/logs.
fn release_xtask(root: &Path, output: &Path) -> R<PathBuf> {
    let logs = output.join("logs");
    fs::create_dir_all(&logs).map_err(|e| format!("{}: {e}", logs.display()))?;
    let log = fs::File::create(logs.join("xtask-release.log")).map_err(|e| e.to_string())?;
    let errors = log.try_clone().map_err(|e| e.to_string())?;
    let code = status(
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["build", "--release", "--locked", "-p", "xtask"])
            .env("CARGO_INCREMENTAL", "0")
            .current_dir(root)
            .stdout(log)
            .stderr(errors),
    )?;
    if code != 0 {
        return Err(format!(
            "the release build of xtask failed: see {}",
            logs.display()
        ));
    }
    Ok(root.join("target/release/xtask"))
}

fn command(
    root: &Path,
    exe: &Path,
    program: &[String],
    environment: &[(&'static str, &'static str)],
    cpus: Option<&str>,
) -> Command {
    let program: Vec<String> = program
        .iter()
        .enumerate()
        .map(|(i, part)| {
            if i == 0 && part == "xtask" {
                exe.display().to_string()
            } else {
                part.clone()
            }
        })
        .collect();
    let mut command = if let Some(cpus) = cpus {
        let mut c = Command::new("taskset");
        c.args(["-c", cpus]).args(&program);
        c
    } else {
        let mut c = Command::new(&program[0]);
        c.args(&program[1..]);
        c
    };
    command
        .current_dir(root)
        .env("CARGO_INCREMENTAL", "0")
        .envs(environment.iter().copied())
        .stdin(Stdio::null());
    command
}

/// Runs one step with its output in `DIR/logs/STEP.log`, and records it in
/// `DIR/steps/STEP.json` and the run log.
fn run_step(
    output: &Path,
    log: &Path,
    item: &Item,
    step: &Step,
    cpus: Option<&str>,
    command: &mut Command,
) -> R<i32> {
    let ran = execute(output, log, &step.name, command)?;
    record_step(output, log, item, step, cpus, &ran, ran.code)
}

/// Runs the memory steps of one batch as one command, with its output in
/// `DIR/logs/BATCH.log`, and records each step with the status of the gate result
/// in its `--out`, or 1 if it wrote none; returns how many failed.
pub(super) fn run_batch(
    root: &Path,
    exe: &Path,
    output: &Path,
    log: &Path,
    steps: &[(&Item, &Step)],
    cpus: Option<&str>,
) -> R<usize> {
    let (_, first) = steps[0];
    let name = first.batch.expect("a batch");
    let mut program = owned(&CAPTURE_QUALIFY);
    for (i, (_, step)) in steps.iter().enumerate() {
        let arguments = step
            .command
            .strip_prefix(&program[..CAPTURE_QUALIFY.len()])
            .ok_or_else(|| format!("{}: a batch runs only capture qualify", step.name))?
            .to_vec();
        if step.environment != first.environment {
            return Err(format!(
                "{}: the batch {name} differs in environment",
                step.name
            ));
        }
        if i > 0 {
            program.push(crate::capture::AND.into());
        }
        program.extend(arguments);
    }
    // A step's verdict is the result it writes in this run, never an earlier one.
    let results: Vec<Option<PathBuf>> = steps
        .iter()
        .map(|(_, step)| {
            let result =
                flag_value(step, "--out").map(|out| out.join(crate::harness::reuse::RESULT));
            if let Some(result) = &result {
                let _ = fs::remove_file(result);
            }
            result
        })
        .collect();
    let mut command = command(root, exe, &program, &first.environment, cpus);
    let ran = execute(output, log, name, &mut command)?;
    let mut failed = 0;
    for ((item, step), result) in steps.iter().zip(results) {
        let status = result
            .and_then(|result| crate::checks::read_json(&result).ok())
            .and_then(|result| result.get("status").and_then(Value::as_f64))
            .map_or(1, |status| status as i32);
        if record_step(output, log, item, step, cpus, &ran, status)? != 0 {
            failed += 1;
        }
    }
    Ok(failed)
}

/// What [`execute`] ran: its exit code, when it started, how long it took, and
/// the log its output went to (`DIR/logs/NAME.log`).
pub(super) struct Ran {
    pub(super) code: i32,
    pub(super) started: String,
    pub(super) seconds: f64,
    pub(super) name: String,
}

/// Runs `command` with its output in `DIR/logs/NAME.log`, noted in the run log.
fn execute(output: &Path, log: &Path, name: &str, command: &mut Command) -> R<Ran> {
    let logs = output.join("logs");
    fs::create_dir_all(&logs).map_err(|e| format!("{}: {e}", logs.display()))?;
    let file = fs::File::create(logs.join(format!("{name}.log"))).map_err(|e| e.to_string())?;
    let errors = file.try_clone().map_err(|e| e.to_string())?;
    append(log, name)?;
    let started = crate::python::utc_now();
    let clock = Instant::now();
    let code = command
        .stdout(file)
        .stderr(errors)
        .status()
        .map_or(127, |s| s.code().unwrap_or(1));
    Ok(Ran {
        code,
        started,
        seconds: clock.elapsed().as_secs_f64(),
        name: name.to_owned(),
    })
}

/// Records `step`, which `ran` with exit `code` (alone, or in a batch whose
/// name the record keeps), in `DIR/steps/STEP.json` and the run log.
pub(super) fn record_step(
    output: &Path,
    log: &Path,
    item: &Item,
    step: &Step,
    cpus: Option<&str>,
    ran: &Ran,
    code: i32,
) -> R<i32> {
    // A measurement that exits 0 with cases it did not measure fails here, so no
    // feature measures nothing in silence.
    let unmeasured = if code == 0 {
        unmeasured_cases(step)
    } else {
        Vec::new()
    };
    let code = if unmeasured.is_empty() { code } else { 1 };
    if let Some(result) = checked_result(step) {
        crate::harness::reuse::checked(&result, code)?;
    }
    append(log, &format!("{} EXIT={code}", step.name))?;
    // The verdict lines of a grade or a codec collection, which tell listed open
    // cases from other failures; of a batch's shared log, only those naming one of
    // the step's own cases.
    let batched = ran.name != step.name;
    let cases = only_cases(step);
    let mut outcome: Vec<String> =
        fs::read_to_string(output.join("logs").join(format!("{}.log", ran.name)))
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                line.contains(" cases pass")
                    || line.contains(" open failures, as listed")
                    || line.contains(": failing: ")
                    || line.contains("diagnostic on this target")
            })
            .filter(|line| !batched || cases.iter().any(|case| line.contains(case.as_str())))
            .map(str::to_owned)
            .collect();
    if !unmeasured.is_empty() {
        outcome.push(format!(
            "{} of its cases not measured: {}",
            unmeasured.len(),
            unmeasured.join(", ")
        ));
    }
    let mut shown = step.command.clone();
    if let Some(cpus) = cpus {
        shown.splice(0..0, owned(&["taskset", "-c", cpus]));
    }
    let mut fields = vec![
        ("step", Value::str(&step.name)),
        ("item", Value::str(&item.name)),
        ("stage", Value::str(step.stage.name())),
        ("command", Value::strings(shown)),
        (
            "environment",
            Value::object(step.environment.iter().map(|(k, v)| (*k, Value::str(*v)))),
        ),
        ("started", Value::str(&ran.started)),
    ];
    // A batched step's time is its batch's, which it shares with the others.
    let seconds = Value::float((ran.seconds * 10.0).round() / 10.0);
    if batched {
        fields.extend([("batch", Value::str(&ran.name)), ("batch_seconds", seconds)]);
    } else {
        fields.push(("seconds", seconds));
    }
    fields.extend([
        ("status", Value::int(i64::from(code))),
        ("outcome", Value::strings(outcome)),
    ]);
    crate::corpus::write_json(
        &output.join("steps").join(format!("{}.json", step.name)),
        &Value::object(fields),
    )?;
    Ok(code)
}

/// The xtask commands whose timing or memory steps check their own collections and
/// fail on a workload or case they did not measure, writing no `report.json`.
const SELF_CHECKING: [&str; 2] = ["codecs", "capture"];

fn self_checking(step: &Step) -> bool {
    step.command
        .get(1)
        .is_some_and(|command| SELF_CHECKING.contains(&command.as_str()))
}

/// The directory a step writes its report and gate result to (`--output DIR`).
fn step_output(step: &Step) -> Option<PathBuf> {
    flag_value(step, "--output")
}

/// The case ids that follow `--only` in the step's command.
fn only_cases(step: &Step) -> Vec<String> {
    step.command
        .iter()
        .skip_while(|arg| *arg != "--only")
        .skip(1)
        .take_while(|arg| !arg.starts_with("--"))
        .cloned()
        .collect()
}

/// The path that follows `flag` in the step's command.
fn flag_value(step: &Step, flag: &str) -> Option<PathBuf> {
    step.command
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| PathBuf::from(&pair[1]))
}

/// The measured result of a timing or memory step that [`unmeasured_cases`]
/// checks, whose gate result then records the step's verdict.
pub(super) fn checked_result(step: &Step) -> Option<PathBuf> {
    if !matches!(step.placement, Placement::Timed | Placement::Memory) || self_checking(step) {
        return None;
    }
    let output = step_output(step)?;
    (output.join(crate::harness::reuse::RESULT).is_file()
        && !crate::harness::reuse::was_reused(&output))
    .then_some(output)
}

/// What a timing or memory step did not measure: the cases of its report
/// in `--output DIR` whose timing is not `measured` or whose memory
/// collection is not `qualified`, or the report itself when it is missing, does
/// not read or says its collection is incomplete. A command in [`SELF_CHECKING`]
/// checks itself. A step that reused a result has no report: it passes only when
/// a gate run checked the reused result's cases and recorded its verdict.
pub(super) fn unmeasured_cases(step: &Step) -> Vec<String> {
    let (key, measured) = match step.placement {
        Placement::Timed => ("timing", "measured"),
        Placement::Memory => ("memory", "qualified"),
        _ => return Vec::new(),
    };
    if self_checking(step) {
        return Vec::new();
    }
    let Some(output) = step_output(step) else {
        return vec!["the step names no --output".into()];
    };
    if crate::harness::reuse::was_reused(&output) {
        return if crate::harness::reuse::reused_checked(&output) {
            Vec::new()
        } else {
            vec!["a reused result whose cases no gate run checked; --full measures it".into()]
        };
    }
    let filename = if step
        .command
        .get(1)
        .is_some_and(|cmd| cmd == "isobmff-bench")
    {
        "collection.json"
    } else {
        "report.json"
    };
    let check = || {
        let contents =
            fs::read_to_string(output.join(filename)).map_err(|e| format!("no report: {e}"))?;
        let report =
            crate::json::parse_observations(&contents).map_err(|e| format!("{filename}: {e}"))?;
        unmeasured_in(step, &report, key, measured)
    };
    check().unwrap_or_else(|error| vec![error])
}

/// Only ISO BMFF assigns a subset of a corpus's measurements. Resolve it from
/// the scheduled command, including the aliases of each physical index operation.
fn collection_selection(
    step: &Step,
    command: &str,
    report: &Value,
    rows: &[Value],
) -> R<Option<Vec<usize>>> {
    if !matches!(command, "isobmff-bench" | "isobmff-seek") {
        return Ok(None);
    }
    let part = flag_value(step, "--part")
        .map(|p| parse_part(&p.to_string_lossy()))
        .transpose()?;
    let root = crate::root();
    let (contract, selected) = if command == "isobmff-seek" {
        crate::isobmff_seek::measurement_selection(&root, part)?
    } else {
        crate::isobmff_bench::full_part_selection(&root, part.unwrap_or((0, 1)))?
    };
    let indices = Value::Array(selected.iter().map(|&i| Value::int(i as i128)).collect());
    if report.get("contract_id") != contract.get("id")
        || report.get("part") != Some(&crate::isobmff_bench::part_value(part))
        || report.get("selection") != Some(&indices)
        || selected.is_empty()
    {
        return Err("ISO BMFF collection differs from the scheduled measurement selection".into());
    }
    let ids = contract
        .get("required_profiles")
        .and_then(Value::as_array)
        .ok_or("ISO BMFF contract lacks required profiles")?;
    if rows
        .iter()
        .map(|row| row.get("id"))
        .ne(ids.iter().map(Some))
    {
        return Err(
            "ISO BMFF collection differs from the registered case order or membership".into(),
        );
    }
    Ok(Some(selected))
}

/// Called after a successful runner exit. The corpus runner has already checked
/// target-specific open and unpinned parity against its contract. Those rows can
/// lack measurements. Any recorded measurement must have the requested status.
pub(super) fn unmeasured_in(
    step: &Step,
    report: &Value,
    key: &str,
    measured: &str,
) -> R<Vec<String>> {
    let rows = crate::harness::grade::report_rows(report)?;
    let command = step
        .command
        .get(1)
        .map(String::as_str)
        .ok_or("missing measured command")?;
    let external = command == "external-vector";
    if external != report.get("case").is_some() {
        return Err(format!("wrong report shape for {command}"));
    }
    let selection = collection_selection(step, command, report, rows)?;
    let mut missing = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("missing case id at index {index}"))?;
        let parity = row
            .get("parity")
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str);
        let parity_only = if let Some(selection) = &selection {
            selection.binary_search(&index).is_err() && parity == Some("pass")
        } else {
            !external && matches!(parity, Some("unpinned" | "mismatch"))
        };
        let collected = match row.get(key) {
            None | Some(Value::Null) => parity_only,
            Some(value) => {
                parity == Some("pass")
                    && value.get("status").and_then(Value::as_str) == Some(measured)
            }
        };
        if !collected {
            missing.push(id.to_owned());
        }
    }
    Ok(missing)
}
