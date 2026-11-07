//! A quick check that a change did not slow the workers down, for changes that are
//! not meant to affect speed. Three independent readings:
//!
//! - [`count`] counts what two workers execute on one workload with `perf stat`. It
//!   takes seconds, and on equal input the instruction count repeats closely, so it
//!   finds a change in the code before any timing round is spent on it.
//! - [`same_code`] compares two worker binaries function by function, so a change
//!   that only moves or renames code shows as such. A worker whose functions moved
//!   can still time differently: where a function sits changes branch prediction and
//!   instruction fetch by about 1%, in either direction.
//! - [`screen`] grades one paired timing round against fixed tolerances. It is a
//!   check, not a measurement: a slowdown of 1% on one case passes it. Recorded
//!   results come from a full run and are never replaced by a screen.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use super::gate_build;
use crate::json::{self, Value};
use crate::stats;

/// Above this on one case the round is flagged, in percent. One round of 30 pairs
/// reads a case to about 0.22% (one standard deviation).
pub const CASE_LIMIT: f64 = 1.5;
/// Above this for the median of a group of cases the round is flagged, in percent.
pub const GROUP_LIMIT: f64 = 0.75;

/// Above this many percent more instructions on one workload the count is flagged.
pub const INSTRUCTION_LIMIT: f64 = 0.25;
/// Runs of each worker. The reading of a counter is its least value over the runs.
const COUNT_RUNS: usize = 5;
/// The counters of [`count`], user mode only.
const COUNTERS: [&str; 3] = ["instructions:u", "cycles:u", "branch-misses:u"];

/// One group's reading, and the cases it flags or could not read.
#[derive(Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub cases: usize,
    pub median_percent: f64,
    pub flagged: Vec<(String, f64)>,
    pub unmeasured: Vec<String>,
}

/// The candidate's cost over the baseline's for each case of one report, in percent.
fn case_changes(report: &Value) -> Vec<(String, Result<f64, String>)> {
    let single = [report.clone()];
    let cases = report
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or(&single);
    cases
        .iter()
        .map(|case| {
            let name = case
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("case")
                .to_owned();
            let timing = case
                .get("timing")
                .or_else(|| report.get("case")?.get("timing"));
            let status = timing
                .and_then(|t| t.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("missing");
            let ratio = timing
                .and_then(|t| t.get("ratio")?.get("median")?.as_f64())
                .filter(|_| status == "measured");
            (
                name,
                ratio
                    .map(|r| (r - 1.0) * 100.0)
                    .ok_or_else(|| status.to_owned()),
            )
        })
        .collect()
}

/// Grade one report's cases against the tolerances.
fn grade(name: &str, report: &Value) -> Group {
    let (mut changes, mut flagged, mut unmeasured) = (Vec::new(), Vec::new(), Vec::new());
    for (case, change) in case_changes(report) {
        match change {
            Ok(change) => {
                changes.push(change);
                if change > CASE_LIMIT {
                    flagged.push((case, change));
                }
            }
            Err(status) => unmeasured.push(format!("{case}: {status}")),
        }
    }
    Group {
        name: name.to_owned(),
        cases: changes.len() + unmeasured.len(),
        median_percent: if changes.is_empty() {
            0.0
        } else {
            stats::median(&changes)
        },
        flagged,
        unmeasured,
    }
}

/// Grade every `<group>/report.json` under `reports`. Passing means no flagged case,
/// no flagged group and no case left unmeasured.
pub fn screen(root: &Path, reports: &Path) -> Result<bool, String> {
    let pinned = gate_build::pinned_release(root)?;
    let mut folders: Vec<_> = fs::read_dir(reports)
        .map_err(|e| format!("{}: {e}", reports.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("report.json").is_file())
        .collect();
    folders.sort();
    if folders.is_empty() {
        return Err(format!("no reports under {}", reports.display()));
    }
    let mut passed = true;
    for folder in folders {
        let text = fs::read_to_string(folder.join("report.json")).map_err(|e| e.to_string())?;
        let report = json::parse(&text).map_err(|e| format!("{}: {e:?}", folder.display()))?;
        let name = folder.file_name().unwrap_or_default().to_string_lossy();
        let group = grade(&name, &report);
        println!(
            "{:32} {:4} cases  median {:+.2}%",
            group.name, group.cases, group.median_percent
        );
        let identity = report.get("identity").unwrap_or(&Value::Null);
        println!("  {}", gate_build::describe(identity, &pinned));
        for (case, change) in &group.flagged {
            println!("  above {CASE_LIMIT}%: {case} {change:+.2}%");
        }
        for case in &group.unmeasured {
            println!("  not measured: {case}");
        }
        if group.median_percent > GROUP_LIMIT {
            println!("  group median above {GROUP_LIMIT}%");
            passed = false;
        }
        passed &= group.flagged.is_empty() && group.unmeasured.is_empty();
    }
    println!("{}", if passed { "passed" } else { "flagged" });
    Ok(passed)
}

/// The instructions of one function, addresses removed.
#[derive(Debug, PartialEq)]
struct Function {
    name: String,
    body: Vec<String>,
}

/// Drop the crate disambiguators a demangler prints, which change with any source edit.
fn without_hashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let tail = &rest[start + 1..];
        match tail.find(']') {
            Some(end) if end > 0 && tail[..end].bytes().all(|b| b.is_ascii_hexdigit()) => {
                out.push_str(&rest[..start]);
                rest = &tail[end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Remove what depends on where things sit: a branch target's address before its
/// symbol, and the page and page offset of a data address.
fn position_free(instruction: &str, pages: &mut Vec<String>) -> String {
    let mut fields = instruction.split_whitespace();
    let operation = fields.next().unwrap_or_default();
    let first = fields.next().unwrap_or_default().trim_end_matches(',');
    if operation == "adrp" {
        pages.push(first.to_owned());
        return format!("adrp {first}");
    }
    if let Some(base) = instruction
        .split([',', '['])
        .skip(1)
        .map(str::trim)
        .find(|operand| pages.iter().any(|page| page == operand))
        && (operation == "add" || operation.starts_with("ldr") || operation.starts_with("str"))
    {
        return format!("{operation} {first} {base}");
    }
    match instruction.find('<') {
        Some(symbol) => {
            let head = instruction[..symbol].trim_end();
            let head = head.rsplit_once(char::is_whitespace).map_or(head, |h| h.0);
            format!("{} {}", head.trim_end(), &instruction[symbol..])
        }
        None => instruction.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

/// The functions of `objdump -d --no-show-raw-insn -C` output.
fn functions(listing: &str) -> Vec<Function> {
    let mut result: Vec<Function> = Vec::new();
    let mut pages = Vec::new();
    for line in listing.lines() {
        if let Some((_, name)) = line.split_once(" <").filter(|_| line.ends_with(">:")) {
            result.push(Function {
                name: without_hashes(name.trim_end_matches(">:")),
                body: Vec::new(),
            });
            pages.clear();
        } else if let (Some(function), Some((_, instruction))) =
            (result.last_mut(), line.split_once(":\t"))
        {
            let instruction = without_hashes(instruction.trim());
            function.body.push(position_free(&instruction, &mut pages));
        }
    }
    result
}

/// How two lists of functions differ.
#[derive(Debug, PartialEq, Default)]
pub struct CodeChange {
    pub identical: usize,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

/// Compare by name and length, so copies sharing a name pair up by size.
fn compare(candidate: Vec<Function>, reference: Vec<Function>) -> CodeChange {
    type Keyed = BTreeMap<(String, usize, usize), Vec<String>>;
    let keyed = |list: Vec<Function>| -> Keyed {
        let mut seen = BTreeMap::new();
        list.into_iter()
            .map(|f| {
                let copy = seen.entry((f.name.clone(), f.body.len())).or_insert(0usize);
                *copy += 1;
                ((f.name, f.body.len(), *copy), f.body)
            })
            .collect()
    };
    let (mut candidate, reference) = (keyed(candidate), keyed(reference));
    let mut change = CodeChange::default();
    for (key, body) in reference {
        match candidate.remove(&key) {
            Some(other) if other == body => change.identical += 1,
            Some(_) => change.changed.push(key.0),
            None => change.removed.push(key.0),
        }
    }
    change.added = candidate.into_keys().map(|key| key.0).collect();
    change
}

fn listing(worker: &Path) -> Result<String, String> {
    let output = Command::new("objdump")
        .args(["-d", "--no-show-raw-insn", "-C", "-j", ".text"])
        .arg(worker)
        .output()
        .map_err(|e| format!("objdump: {e}"))?;
    if !output.status.success() {
        return Err(format!("objdump: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Report how two worker binaries differ; passing means no function changed.
pub fn same_code(candidate: &Path, reference: &Path) -> Result<bool, String> {
    let change = compare(
        functions(&listing(candidate)?),
        functions(&listing(reference)?),
    );
    println!("{} functions identical", change.identical);
    for (label, names) in [
        ("changed", &change.changed),
        ("only in reference", &change.removed),
        ("only in candidate", &change.added),
    ] {
        for name in names {
            println!("  {label}: {name}");
        }
    }
    Ok(change.changed.is_empty() && change.removed.is_empty() && change.added.is_empty())
}

/// The values of [`COUNTERS`] in `perf stat -x,` output.
fn parse_counters(output: &str) -> Result<[u64; COUNTERS.len()], String> {
    let mut values = [0; COUNTERS.len()];
    for (value, name) in values.iter_mut().zip(COUNTERS) {
        let line = output
            .lines()
            .find(|line| line.split(',').nth(2) == Some(name))
            .ok_or_else(|| format!("perf reported no {name}"))?;
        *value = line
            .split(',')
            .next()
            .and_then(|field| field.parse().ok())
            .ok_or_else(|| format!("perf could not count {name}: {line}"))?;
    }
    Ok(values)
}

/// The least of each counter over the runs of `worker` on `arguments`.
fn counters(worker: &Path, arguments: &[String]) -> Result<[u64; COUNTERS.len()], String> {
    let mut least = [u64::MAX; COUNTERS.len()];
    for _ in 0..COUNT_RUNS {
        let output = Command::new("perf")
            .args(["stat", "-x,", "-e", &COUNTERS.join(","), "--"])
            .arg(worker)
            .args(arguments)
            .stdout(Stdio::null())
            .output()
            .map_err(|e| format!("perf: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "{} failed: {}",
                worker.display(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let values = parse_counters(&String::from_utf8_lossy(&output.stderr))?;
        for (low, value) in least.iter_mut().zip(values) {
            *low = (*low).min(value);
        }
    }
    Ok(least)
}

/// The candidate's count over the reference's, in percent.
fn change_percent(candidate: u64, reference: u64) -> f64 {
    (candidate as f64 / reference as f64 - 1.0) * 100.0
}

/// Count two workers on one workload and print each counter. True unless the candidate
/// runs more than [`INSTRUCTION_LIMIT`] percent more instructions. Cycles and branch
/// misses are printed for reading: they move with placement, so they do not grade.
pub fn count(candidate: &Path, reference: &Path, arguments: &[String]) -> Result<bool, String> {
    let candidate = counters(candidate, arguments)?;
    let reference = counters(reference, arguments)?;
    for (index, name) in COUNTERS.iter().enumerate() {
        println!(
            "{name:<18} {:>16} {:>16} {:+7.2}%",
            reference[index],
            candidate[index],
            change_percent(candidate[index], reference[index])
        );
    }
    Ok(change_percent(candidate[0], reference[0]) <= INSTRUCTION_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_output_reads_by_counter_name() {
        let output = "\n1200,,branch-misses:u,5,100.00,,\n\
                      900,,instructions:u,5,100.00,0.75,insn per cycle\n\
                      1000,,cycles:u,5,100.00,,\n";
        assert_eq!(parse_counters(output), Ok([900, 1000, 1200]));
    }

    #[test]
    fn a_counter_perf_could_not_read_is_an_error() {
        let output = "<not supported>,,instructions:u,0,100.00,,\n";
        assert!(parse_counters(output).is_err());
        assert!(parse_counters("").is_err());
    }

    #[test]
    fn the_limit_applies_to_more_instructions_only() {
        assert!(change_percent(1002, 1000) <= INSTRUCTION_LIMIT);
        assert!(change_percent(1003, 1000) > INSTRUCTION_LIMIT);
        assert!(change_percent(900, 1000) <= INSTRUCTION_LIMIT);
    }

    fn report(cases: &str) -> Value {
        json::parse(&format!(r#"{{"cases": [{cases}]}}"#)).unwrap()
    }

    fn case(id: &str, median: f64) -> String {
        format!(
            r#"{{"id": "{id}", "timing": {{"status": "measured", "ratio": {{"median": {median}}}}}}}"#
        )
    }

    #[test]
    fn a_case_above_the_limit_is_flagged_but_not_one_below_it() {
        let group = grade(
            "g",
            &report(&[case("a", 1.016), case("b", 1.014)].join(",")),
        );
        assert_eq!(group.flagged.len(), 1);
        assert_eq!(group.flagged[0].0, "a");
    }

    #[test]
    fn a_faster_case_is_not_flagged() {
        assert!(grade("g", &report(&case("a", 0.95))).flagged.is_empty());
    }

    #[test]
    fn a_case_without_a_measurement_is_never_dropped() {
        let group = grade(
            "g",
            &report(&format!(
                r#"{},{{"id": "b", "timing": {{"status": "contaminated"}}}}"#,
                case("a", 1.0)
            )),
        );
        assert_eq!(group.cases, 2);
        assert_eq!(group.unmeasured, ["b: contaminated"]);
    }

    #[test]
    fn the_group_reads_as_the_median_of_its_cases() {
        let cases = [case("a", 1.0), case("b", 1.01), case("c", 1.012)].join(",");
        assert!((grade("g", &report(&cases)).median_percent - 1.0).abs() < 1e-9);
    }

    const LISTING: &str = "\
0000000000001000 <worker[abc123]::run>:
    1000:\tadrp\tx8, 2000 <table+0x10>
    1004:\tadd\tx8, x8, #0x708
    1008:\tbl\t1100 <worker[abc123]::next>
    100c:\tcbz\tx8, 1000 <worker[abc123]::run>
    1010:\tret
";

    #[test]
    fn functions_that_only_moved_compare_identical() {
        let moved = LISTING
            .replace("1100 <", "2100 <")
            .replace("2000 <table+0x10>", "3000 <other+0x20>")
            .replace("#0x708", "#0x320")
            .replace("abc123", "def456")
            .replace("0000000000001000", "0000000000005000");
        let change = compare(functions(&moved), functions(LISTING));
        assert_eq!(change.identical, 1);
        assert!(change.changed.is_empty());
    }

    #[test]
    fn a_changed_instruction_or_call_target_is_reported() {
        for edited in [
            LISTING.replace("cbz\tx8", "cbnz\tx8"),
            LISTING.replace(
                "bl\t1100 <worker[abc123]::next>",
                "bl\t1100 <worker[abc123]::last>",
            ),
        ] {
            let change = compare(functions(&edited), functions(LISTING));
            assert_eq!(change.changed, ["worker::run"]);
        }
    }

    #[test]
    fn a_renamed_function_shows_on_both_sides() {
        let renamed = LISTING.replace("::run>:", "::walk>:");
        let change = compare(functions(&renamed), functions(LISTING));
        assert_eq!(change.removed, ["worker::run"]);
        assert_eq!(change.added, ["worker::walk"]);
    }
}
