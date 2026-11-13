//! `cargo xtask ass equivalence`: the renderer of a base revision and of the
//! working tree on the same cases, frame by frame.
//!
//! Both sides build `crates/ass/examples/equivalence.rs` of the working
//! tree, so they render the same generated cases, with the `work-probe`
//! feature, and record every frame's change, images, error and work. The
//! base is the revision's sources from `git archive`, in a scratch
//! workspace of the crates the renderer needs. A revision from before the
//! feature gets an equivalent probe: its frame's work budget is wrapped in a
//! guard that keeps what it spent. The records must agree line for line;
//! the peak of each case's budget is reported for the `ass-render`
//! workloads, not compared. With `--work any` the work each frame spent is
//! not compared, for a change that does less work for the same frames,
//! such as a cache: totals are reported instead. A frame both sides refuse
//! for running out of work agrees, and so does one the base refuses for work
//! and the candidate makes, with the change value of the frame after it; a
//! frame only the candidate refuses differs, as does any other error, and
//! every later frame is compared all the same.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::prepare::run;

pub const USAGE: &str = "usage: cargo xtask ass equivalence --base REV [--seed N] [--random N]
       [--mutations N] [--jobs N] [--output DIR] [--cache OUTLINES,MEGABYTES]
       [--work equal|any] [--recorded]

  The renderer of REV and of the working tree render the ass-render fixtures
  (at their frames, every 40 ms, and --mutations N mutated with varied settings,
  default 4) and --random N generated scripts (default 300) from --seed N
  (default 1), on --jobs N threads (default 4), and must record the same
  change, images, error and work for every frame; a budget error is compared
  without what the budget had left, which depends on the storage each side
  holds. --cache gives every case those cache limits instead of its own.
  --work any leaves the work out of the comparison and reports both sides'
  totals: a frame both refuse for work (Policy(\"ass work\") or
  Policy(\"raster work\")) agrees, as does one only the base refuses that the
  candidate makes (with the change value after it); one only the candidate
  refuses differs, as does any other error. Builds and records go to DIR
  (default target/ass-equivalence); the peak budget of each workload is
  printed for both. --recorded compares the records already in DIR without
  building or rendering.";

/// The sources a scratch workspace needs to build the renderer and the
/// example, and the members it builds.
const SOURCES: [&str; 9] = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "crates/core",
    "crates/font",
    "crates/shape",
    "crates/raster",
    "crates/ass",
    "crates/ass-worker/src/fixture.rs",
];
const MEMBERS: &str =
    r#"members = ["crates/core", "crates/font", "crates/shape", "crates/raster", "crates/ass"]"#;
const EXAMPLE: &str = "crates/ass/examples/equivalence.rs";

struct Options {
    base: String,
    seed: u64,
    random: u64,
    mutations: u64,
    jobs: u64,
    output: PathBuf,
    cache: Option<String>,
    work: Work,
    recorded: bool,
}

/// Whether the work each frame spent must agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Equal,
    Any,
}

fn options(root: &Path, args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        base: String::new(),
        seed: 1,
        random: 300,
        mutations: 4,
        jobs: 4,
        output: root.join("target/ass-equivalence"),
        cache: None,
        work: Work::Equal,
        recorded: false,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--recorded" {
            options.recorded = true;
            continue;
        }
        let value = args.next().ok_or(USAGE)?;
        let number = || value.parse::<u64>().map_err(|_| USAGE.to_string());
        match flag.as_str() {
            "--base" => options.base.clone_from(value),
            "--seed" => options.seed = number()?,
            "--random" => options.random = number()?,
            "--mutations" => options.mutations = number()?,
            "--jobs" => options.jobs = number()?.clamp(1, 64),
            "--output" => options.output = root.join(value),
            "--cache" => options.cache = Some(value.clone()),
            "--work" => {
                options.work = match value.as_str() {
                    "equal" => Work::Equal,
                    "any" => Work::Any,
                    _ => return Err(USAGE.into()),
                }
            }
            _ => return Err(USAGE.into()),
        }
    }
    if options.base.is_empty() {
        return Err(USAGE.into());
    }
    Ok(options)
}

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let options = options(root, args)?;
    std::fs::create_dir_all(&options.output)
        .map_err(|e| format!("{}: {e}", options.output.display()))?;
    let records = [
        options.output.join("base.txt"),
        options.output.join("candidate.txt"),
    ];
    if !options.recorded {
        record(root, &options, &records)?;
    }
    let (base, candidate) = (read(&records[0])?, read(&records[1])?);
    let summary = compare(&parse(&base), &parse(&candidate), options.work);
    print!("{}", summary.text);
    if summary.mismatches == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} cases differ from {}",
            summary.mismatches, options.base
        ))
    }
}

/// Builds both sides and records their frames in `records`.
fn record(root: &Path, options: &Options, records: &[PathBuf; 2]) -> Result<(), String> {
    let base = checkout(root, options)?;
    let candidate = build(root, &root.join("target"))?;
    let base_example = build(&base, &options.output.join("target"))?;
    for (example, record) in [(&base_example, &records[0]), (&candidate, &records[1])] {
        let mut command = Command::new(example);
        command
            .arg(root.join(".research/fonts/files"))
            .arg(root.join("harness/fixtures/ass-render"))
            .args(
                [
                    options.seed,
                    options.random,
                    options.mutations,
                    options.jobs,
                ]
                .map(|n| n.to_string()),
            )
            .arg(record)
            .args(&options.cache);
        println!("$ {command:?}");
        run(&mut command)?;
    }
    Ok(())
}

/// The base revision's sources in a scratch workspace, with the working
/// tree's example and, if the revision has none, a work probe.
fn checkout(root: &Path, options: &Options) -> Result<PathBuf, String> {
    let dir = options.output.join("base");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tar = options.output.join("base.tar");
    run(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["archive", "--format=tar", "-o"])
        .arg(&tar)
        .arg(&options.base)
        .arg("--")
        .args(SOURCES))?;
    run(Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(&dir))?;
    std::fs::remove_file(&tar).map_err(|e| format!("{}: {e}", tar.display()))?;
    let manifest = dir.join("Cargo.toml");
    let text = read(&manifest)?;
    let members = text
        .lines()
        .find(|line| line.starts_with("members = ["))
        .ok_or("the base's Cargo.toml has no members line")?;
    write(&manifest, &text.replacen(members, MEMBERS, 1))?;
    let example = dir.join(EXAMPLE);
    if let Some(parent) = example.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::copy(root.join(EXAMPLE), &example)
        .map_err(|e| format!("{}: {e}", example.display()))?;
    let ass = dir.join("crates/ass");
    if !read(&ass.join("Cargo.toml"))?.contains("work-probe") {
        add_probe(&ass)?;
    }
    Ok(dir)
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The probe of a revision from before the `work-probe` feature: the
/// frame's work budget is wrapped in a guard that keeps what it spent when
/// the frame ends, however it ends, and `Renderer::work_spent` reads it,
/// 0 for a frame that made no budget, as the feature does.
fn add_probe(ass: &Path) -> Result<(), String> {
    let manifest = ass.join("Cargo.toml");
    let text = read(&manifest)?;
    write(
        &manifest,
        &text.replacen("[lints]", "[features]\nwork-probe = []\n\n[lints]", 1),
    )?;
    let render = ass.join("src/render/mod.rs");
    let text = read(&render)?;
    let budget = "let mut work = WorkBudget::new(";
    let entry = "pub fn render(&mut self, script: &mut Script, now: i64) -> Result<Frame<'_>> {";
    if text.matches(budget).count() != 1 || text.matches(entry).count() != 1 {
        return Err("the base's renderer has no single frame budget to probe".into());
    }
    let text = text
        .replacen(budget, "let mut work = crate::WorkProbe::new(", 1)
        .replacen(
            entry,
            &format!("{entry}\n        crate::WorkProbe::reset();"),
            1,
        );
    write(&render, &text)?;
    let lib = ass.join("src/lib.rs");
    let mut text = read(&lib)?;
    text.push_str(PROBE);
    write(&lib, &text)
}

const PROBE: &str = r"
/// The frame's work budget, which keeps what it spent when the frame ends.
#[derive(Debug)]
struct WorkProbe(WorkBudget);

thread_local! {
    static WORK_SPENT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl WorkProbe {
    fn new(units: u64) -> Self {
        Self(WorkBudget::new(units))
    }

    fn reset() {
        WORK_SPENT.with(|spent| spent.set(0));
    }
}

impl std::ops::Deref for WorkProbe {
    type Target = WorkBudget;
    fn deref(&self) -> &WorkBudget {
        &self.0
    }
}

impl std::ops::DerefMut for WorkProbe {
    fn deref_mut(&mut self) -> &mut WorkBudget {
        &mut self.0
    }
}

impl Drop for WorkProbe {
    fn drop(&mut self) {
        WORK_SPENT.with(|spent| spent.set(self.0.spent()));
    }
}

impl Renderer {
    /// The work units the last frame on this thread spent.
    pub fn work_spent(&self) -> u64 {
        WORK_SPENT.with(std::cell::Cell::get)
    }
}
";

/// Builds the example with the probe in `workspace`; the binary.
fn build(workspace: &Path, target: &Path) -> Result<PathBuf, String> {
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--release",
            "-p",
            "triergon-ass",
            "--example",
            "equivalence",
        ])
        .args(["--features", "work-probe"])
        .args(crate::harness::gate_build::target_args())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", target)
        .current_dir(workspace);
    println!("$ {command:?}");
    run(&mut command)?;
    Ok(target
        .join(crate::harness::gate_build::release_subdir())
        .join("examples/equivalence"))
}

/// `line` without what a budget error says the budget had left: the
/// storage each side holds differs, and the error, its request and the
/// work before it are what must agree. With [`Work::Any`], without the
/// work the frame spent either.
fn comparable(line: &str, work: Work) -> String {
    let line = match line.split_once(", available: ") {
        Some((head, tail)) => {
            let rest = tail
                .find(|c: char| !c.is_ascii_digit())
                .map_or("", |at| &tail[at..]);
            format!("{head}{rest}")
        }
        None => line.to_owned(),
    };
    match (work, line.rsplit_once(" work ")) {
        (Work::Any, Some((head, _))) => head.to_owned(),
        _ => line,
    }
}

/// The work a frame line says it spent.
fn spent(line: &str) -> u64 {
    line.rsplit_once(" work ")
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

/// What the frames compared with [`Work::Any`] add up to: the work both
/// sides spent on the frames both made, the frames where the candidate spent
/// more, and the frames refused for work by the base alone, by both and by
/// the candidate alone, each with the number of cases they are in.
#[derive(Default)]
struct Tally {
    base_work: u64,
    candidate_work: u64,
    more: usize,
    base_refused: (usize, usize),
    both_refused: (usize, usize),
    candidate_refused: (usize, usize),
}

/// Whether a frame line is a frame refused for running out of work: the
/// renderer's or the rasteriser's work bound, not another policy limit.
fn refused(line: &str) -> bool {
    line.starts_with("frame ")
        && [
            " error Policy(\"ass work\")",
            " error Policy(\"raster work\")",
        ]
        .iter()
        .any(|refusal| line.contains(refusal))
}

/// Whether a frame line is a frame made: its change and images.
fn made(line: &str) -> bool {
    line.starts_with("frame ") && line.contains(" change ")
}

/// `line` without its change value.
fn without_change(line: &str) -> String {
    match line.split_once(" change ") {
        Some((head, tail)) => format!("{head}{}", tail.find(' ').map_or("", |at| &tail[at..])),
        None => line.to_owned(),
    }
}

/// Compares a case's frames; the first line that differs. With
/// [`Work::Equal`] every line must agree. With [`Work::Any`] the work is left
/// out: a frame both sides refuse for work agrees, whichever stage ran out,
/// and a frame the base refuses for work and the candidate makes agrees, as
/// does the change value of the frame after it, which the two compare with
/// different frames; a frame only the candidate refuses differs, and so does
/// any other error. Only the work bounds count as work: another policy limit
/// must agree like any other error. Every later frame is compared all the
/// same.
fn compare_case(
    base: &Case<'_>,
    candidate: &Case<'_>,
    work: Work,
    tally: &mut Tally,
) -> Option<usize> {
    let mut first = None;
    let mut after_base_refusal = false;
    let mut seen = [false; 3];
    for (at, (b, c)) in base.lines.iter().zip(&candidate.lines).enumerate() {
        let agree = match work {
            Work::Equal => comparable(b, work) == comparable(c, work),
            Work::Any => match (refused(b), refused(c)) {
                (true, true) => {
                    tally.both_refused.0 += 1;
                    seen[1] = true;
                    true
                }
                (true, false) => {
                    tally.base_refused.0 += 1;
                    seen[0] = true;
                    made(c)
                }
                (false, true) => {
                    tally.candidate_refused.0 += 1;
                    seen[2] = true;
                    false
                }
                (false, false) => {
                    let (x, y) = (comparable(b, work), comparable(c, work));
                    let same =
                        x == y || (after_base_refusal && without_change(&x) == without_change(&y));
                    if same && b.starts_with("frame ") {
                        tally.base_work += spent(b);
                        tally.candidate_work += spent(c);
                        tally.more += usize::from(spent(c) > spent(b));
                    }
                    same
                }
            },
        };
        after_base_refusal = refused(b) && made(c);
        if !agree && first.is_none() {
            first = Some(at);
        }
    }
    for (seen, count) in seen.into_iter().zip([
        &mut tally.base_refused.1,
        &mut tally.both_refused.1,
        &mut tally.candidate_refused.1,
    ]) {
        *count += usize::from(seen);
    }
    first
}

/// A record's cases: each case's name, its frame lines and its peak.
struct Case<'a> {
    name: &'a str,
    lines: Vec<&'a str>,
    peak: u64,
}

fn parse(record: &str) -> Vec<Case<'_>> {
    let mut cases: Vec<Case<'_>> = Vec::new();
    for line in record.lines() {
        if let Some(name) = line.strip_prefix("case ") {
            cases.push(Case {
                name,
                lines: Vec::new(),
                peak: 0,
            });
        } else if let Some(case) = cases.last_mut() {
            match line.strip_prefix("peak ") {
                Some(peak) => case.peak = peak.parse().unwrap_or(0),
                None => case.lines.push(line),
            }
        }
    }
    cases
}

struct Summary {
    text: String,
    mismatches: usize,
}

/// The candidate's totals, every case whose lines differ from the base's,
/// and the peaks of the workloads.
fn compare(base: &[Case<'_>], candidate: &[Case<'_>], work: Work) -> Summary {
    let mut text = String::new();
    let mut mismatches = 0;
    let (mut frames, mut images, mut errors, mut policy, mut panics) = (0, 0, 0, 0, 0);
    for case in candidate {
        for line in &case.lines {
            frames += usize::from(line.starts_with("frame "));
            if let Some(count) = line
                .split(" images ")
                .nth(1)
                .and_then(|r| r.split(' ').next())
            {
                images += count.parse::<usize>().unwrap_or(0);
            }
            errors += usize::from(line.contains(" error "));
            policy += usize::from(line.contains(" error Policy"));
            panics += usize::from(*line == "panic");
        }
    }
    if base.len() != candidate.len() {
        mismatches += 1;
        let _ = writeln!(
            text,
            "case counts differ: {} and {}",
            base.len(),
            candidate.len()
        );
    }
    let mut tally = Tally::default();
    for (b, c) in base.iter().zip(candidate) {
        let first = if b.name == c.name {
            compare_case(b, c, work, &mut tally)
        } else {
            Some(0)
        };
        if first.is_some() || b.lines.len() != c.lines.len() {
            mismatches += 1;
            if mismatches <= 20 {
                let at = first.unwrap_or(b.lines.len().min(c.lines.len()));
                fn line<'a>(case: &Case<'a>, at: usize) -> &'a str {
                    case.lines.get(at).copied().unwrap_or("(none)")
                }
                let _ = writeln!(
                    text,
                    "{}: base   {}\n{}: change {}",
                    b.name,
                    line(b, at),
                    c.name,
                    line(c, at)
                );
            }
        }
    }
    let _ = writeln!(
        text,
        "{} cases, {frames} frames, {images} images, {errors} errors ({policy} Policy), {panics} panics; {mismatches} differ",
        candidate.len()
    );
    if work == Work::Any {
        let Tally {
            base_work,
            candidate_work,
            more,
            base_refused,
            both_refused,
            candidate_refused,
        } = tally;
        let _ = writeln!(
            text,
            "work not compared: frames refused for work by the base alone {}, by both {}, \
             by the candidate alone {} (each a difference); on the frames both made, base \
             {base_work}, candidate {candidate_work} units, the candidate spending more on \
             {more} frames",
            base_refused.0, both_refused.0, candidate_refused.0
        );
        let _ = writeln!(
            text,
            "cases with such frames: base alone {}, both {}, candidate alone {}",
            base_refused.1, both_refused.1, candidate_refused.1
        );
    }
    let _ = writeln!(text, "peak budget of each workload, base and candidate:");
    for (b, c) in base
        .iter()
        .zip(candidate)
        .filter(|(b, _)| !b.name.contains('~') && !b.name.starts_with("random-"))
    {
        let ratio = c.peak as f64 / b.peak.max(1) as f64;
        let _ = writeln!(
            text,
            "  {:<32} {:>12} {:>12} {ratio:.4}",
            b.name, b.peak, c.peak
        );
    }
    Summary { text, mismatches }
}

#[cfg(test)]
mod tests {
    use super::{Work, add_probe, compare, parse};
    use crate::process::TempDir;

    const RECORD: &str = "case a\nframe 0 change 2 images 1 digest 00 work 5\npeak 10\n\
                          case b\nframe 0 error Policy(\"ass work\") work 9\npeak 3\n";

    #[test]
    fn equal_records_agree_and_are_counted() {
        let summary = compare(&parse(RECORD), &parse(RECORD), Work::Equal);
        assert_eq!(summary.mismatches, 0);
        assert!(
            summary
                .text
                .contains("2 cases, 2 frames, 1 images, 1 errors (1 Policy), 0 panics; 0 differ"),
            "{}",
            summary.text
        );
    }

    #[test]
    fn any_frame_that_differs_fails_its_case_and_peaks_do_not() {
        let other = RECORD
            .replace("work 5", "work 6")
            .replace("peak 3", "peak 4");
        let summary = compare(&parse(RECORD), &parse(&other), Work::Equal);
        assert_eq!(summary.mismatches, 1);
        let missing = &RECORD[..RECORD.find("case b").unwrap()];
        assert_eq!(
            compare(&parse(RECORD), &parse(missing), Work::Equal).mismatches,
            1
        );
    }

    #[test]
    fn work_may_differ_when_asked_but_not_a_refusal_the_base_did_not_make() {
        let less = RECORD.replace("work 5", "work 2");
        let summary = compare(&parse(RECORD), &parse(&less), Work::Any);
        assert_eq!(summary.mismatches, 0);
        assert!(
            summary.text.contains("base 5, candidate 2 units"),
            "{}",
            summary.text
        );
        let refused = "frame 0 error Policy(\"ass work\") work 9";
        let made = "frame 0 change 2 images 0 digest 00 work 4";
        // Spending less, the candidate makes a frame the base refused.
        let summary = compare(
            &parse(RECORD),
            &parse(&RECORD.replace(refused, made)),
            Work::Any,
        );
        assert_eq!(summary.mismatches, 0);
        assert!(
            summary.text.contains("by the base alone 1,"),
            "{}",
            summary.text
        );
        // A frame only the candidate refuses differs.
        let base = RECORD.replace(refused, made);
        let summary = compare(&parse(&base), &parse(RECORD), Work::Any);
        assert_eq!(summary.mismatches, 1);
        assert!(
            summary.text.contains("by the candidate alone 1 "),
            "{}",
            summary.text
        );
        // Both refusing agrees, whichever stage ran out.
        let other = RECORD.replace("Policy(\"ass work\")", "Policy(\"raster work\")");
        assert_eq!(
            compare(&parse(RECORD), &parse(&other), Work::Any).mismatches,
            0
        );
        // Other images still differ.
        let other = less.replace("images 1 digest 00", "images 1 digest 01");
        assert_eq!(
            compare(&parse(RECORD), &parse(&other), Work::Any).mismatches,
            1
        );
    }

    #[test]
    fn a_base_refusal_agrees_only_with_a_frame_made() {
        let base = "case a\nframe 0 error Policy(\"ass work\") work 9\npeak 1\n";
        for other in [
            "frame 0 error Allocation work 3",
            "frame 0 error Unsupported(\"language\") work 3",
            "frame 0 error Policy(\"bitmap size\") work 3",
        ] {
            let candidate = format!("case a\n{other}\npeak 1\n");
            let summary = compare(&parse(base), &parse(&candidate), Work::Any);
            assert_eq!(summary.mismatches, 1, "{other}");
        }
    }

    #[test]
    fn a_limit_is_not_a_work_refusal() {
        let limit = "case a\nframe 0 error Policy(\"border outline points\") work 9\npeak 1\n";
        let made = "case a\nframe 0 change 2 images 0 digest 00 work 4\npeak 1\n";
        // A limit the base hits and the candidate does not, or the other way
        // round, differs; both hitting it agrees.
        assert_eq!(
            compare(&parse(limit), &parse(made), Work::Any).mismatches,
            1
        );
        assert_eq!(
            compare(&parse(made), &parse(limit), Work::Any).mismatches,
            1
        );
        assert_eq!(
            compare(&parse(limit), &parse(limit), Work::Any).mismatches,
            0
        );
    }

    #[test]
    fn frames_after_a_base_refusal_are_still_compared() {
        let base = "case a\nframe 0 error Policy(\"ass work\") work 9\n\
                    frame 1 change 2 images 1 digest 00 work 5\n\
                    frame 2 change 0 images 1 digest 00 work 5\npeak 1\n";
        let ok = "case a\nframe 0 change 2 images 1 digest 00 work 3\n\
                  frame 1 change 0 images 1 digest 00 work 1\n\
                  frame 2 change 0 images 1 digest 00 work 1\npeak 1\n";
        // The change after the refused frame compares with different frames.
        assert_eq!(compare(&parse(base), &parse(ok), Work::Any).mismatches, 0);
        let late = ok.replace(
            "frame 2 change 0 images 1 digest 00",
            "frame 2 change 2 images 1 digest 01",
        );
        assert_eq!(
            compare(&parse(base), &parse(&late), Work::Any).mismatches,
            1
        );
    }

    #[test]
    fn budget_errors_agree_whatever_the_budget_had_left() {
        let error = |available: usize| {
            format!(
                "case a\nframe 0 error Backpressure {{ requested: 9, available: {available} }} work 1\npeak 1\n"
            )
        };
        let (a, b) = (error(5), error(6));
        assert_eq!(compare(&parse(&a), &parse(&b), Work::Equal).mismatches, 0);
        let other = a.replace("requested: 9", "requested: 8");
        assert_eq!(
            compare(&parse(&a), &parse(&other), Work::Equal).mismatches,
            1
        );
    }

    #[test]
    fn the_probe_wraps_the_single_frame_budget() {
        let dir = TempDir::new(None).unwrap();
        let ass = dir.0.as_path();
        std::fs::create_dir_all(ass.join("src/render")).unwrap();
        std::fs::write(
            ass.join("Cargo.toml"),
            "[package]\n\n[lints]\nworkspace = true\n",
        )
        .unwrap();
        std::fs::write(ass.join("src/lib.rs"), "mod render;\n").unwrap();
        let render = "pub fn render(&mut self, script: &mut Script, now: i64) -> Result<Frame<'_>> {\n\
                      let mut work = WorkBudget::new(self.frame.work);\n}\n";
        std::fs::write(ass.join("src/render/mod.rs"), render).unwrap();
        add_probe(ass).unwrap();
        let manifest = std::fs::read_to_string(ass.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("[features]\nwork-probe = []\n\n[lints]"));
        let render = std::fs::read_to_string(ass.join("src/render/mod.rs")).unwrap();
        assert!(render.contains("crate::WorkProbe::reset();"));
        assert!(render.contains("let mut work = crate::WorkProbe::new(self.frame.work);"));
        let lib = std::fs::read_to_string(ass.join("src/lib.rs")).unwrap();
        assert!(lib.contains("pub fn work_spent(&self) -> u64"));
        // A renderer with two budgets, or none, is refused.
        std::fs::write(ass.join("src/render/mod.rs"), "fn render() {}\n").unwrap();
        assert!(add_probe(ass).is_err());
    }
}
