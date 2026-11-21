//! The full gate as one plan: every step a gate run takes, grouped into items
//! that can run on different hosts, and split into shards of about equal work.
//!
//! An item is the work for one measured thing: a feature's timing, memory and
//! grade, a part of the ISO BMFF suite, the ISO BMFF seek, packet-framing or
//! progressive suite, the video codecs' parity, the audio codecs' correctness lanes,
//! the Ogg reader's packets and seeks, a codec workload list, a capture case, a test profile, the reference comparisons. Its steps
//! run on one host, in stages:
//! `timing` runs the parity and timing steps alone on CPU 1 with the RSS module
//! unloaded; `memory`, after the host has proved the kernel bound and rebuilt the
//! workers with the proof, runs the memory collections on every CPU but CPU 0 and
//! then grades, several grades at once. A shard is a set of items; `cargo xtask
//! gate summary` joins the shards' outputs into one result and checks that each
//! shard ran the items the plan gives it, every step of the plan once, on the same
//! source and workers.

use std::path::{Path, PathBuf};
use std::process::Command;

mod check;
mod plan;
mod run;
mod summary;
#[cfg(test)]
mod tests;
mod workspace_tests;

use check::check;
use plan::{COVERED, Coverage, capture_cases, items, report_features};
use run::run;
use summary::summary;
use workspace_tests::test;

pub use check::{input_hashes, input_pins};
pub use plan::dedicated_coverage;
pub(crate) use workspace_tests::INDEX_TESTS;

type R<T> = Result<T, String>;

pub const USAGE: &str = "usage: cargo xtask gate plan [--shards N] [--target TARGET]
       cargo xtask gate check [--shards N] [--target TARGET]
       cargo xtask gate preflight [--shard I/N] [--review FILE] [--kernel]
       cargo xtask gate run --stage timing|memory --output DIR [--shard I/N] [--full] [--no-early-stop]
       cargo xtask gate summary --output FILE DIR...
       cargo xtask gate test --profile checked|release --output DIR [--full]

  plan       print the gate's items and their planning weights (minutes on a
             32-bit ARMv7 c6g.4xlarge), split into N shards (default 1), for
             TARGET (default: every item)
  check      on any machine, before a host is launched: every file the preflight
             of each of N shards reads, resolved as the shard resolves it, is
             committed, generated from committed sources by a host's prepare,
             fetched by it with a pin, or listed in a kept evidence set unpacked
             at the repository root, with any hash preflight pins it to
  preflight  `cargo xtask preflight` over everything shard I of N measures, with
             the committed RSS module review or --review FILE; --kernel after the
             kernel proof
  run        the steps of shard I of N (default 1/1) in one stage: timing (parity
             steps, then timing on CPU 1) or memory (memory on CPUs 1 to the last,
             then the grades); each step's output goes under DIR, its log to
             DIR/logs and its record to DIR/steps; --full measures what a kept
             evidence set holds; --no-early-stop collects every planned report pair
  summary    join the outputs of every shard of a run into FILE: each step's
             status and time, with a failure if a planned step did not run, a
             step failed, or the shards measured different sources or workers
  test       the workspace tests for the gate target in one profile: checked
             (release optimisation with overflow checks and debug assertions) or
             release (the gate build's); a result a kept evidence set holds for the
             same profile, tracked
             sources, test inputs, toolchain, target and host class is reused
             unless --full";

/// The shard file each run writes in its output directory.
const SHARD: &str = "shard.json";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Timing,
    Memory,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Timing => "timing",
            Stage::Memory => "memory",
        }
    }
}

/// Where a step runs and with what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    /// Unpinned; nothing is measured.
    Free,
    /// Alone on CPU 1, the RSS module unloaded.
    Timed,
    /// On CPUs 1 to the last, under the kernel bound.
    Memory,
    /// A grade: unpinned, and several at once.
    Grade,
}

#[derive(Debug)]
struct Step {
    name: String,
    stage: Stage,
    placement: Placement,
    /// The program and its arguments; `xtask` means this program.
    command: Vec<String>,
    environment: Vec<(&'static str, &'static str)>,
    /// Memory steps of a shard in one batch run as one command, [`plan::CAPTURE_QUALIFY`]
    /// with each step's arguments joined by `--and`, so they measure side by side
    /// in one collection; each still writes its own result and step record.
    batch: Option<&'static str>,
}

#[derive(Debug)]
pub struct Item {
    pub name: String,
    /// Planning weight in minutes.
    pub weight: u32,
    steps: Vec<Step>,
    /// The `cargo xtask preflight` arguments for what the item reads.
    preflight: Vec<String>,
}

fn xtask(args: &[&str]) -> Vec<String> {
    std::iter::once("xtask")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Splits `items` into `count` shards: heaviest first, each to the lightest shard
/// so far (the first of equals).
pub fn shards(items: Vec<Item>, count: usize) -> Vec<Vec<Item>> {
    let mut items = items;
    items.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.name.cmp(&b.name)));
    let mut shards: Vec<Vec<Item>> = (0..count).map(|_| Vec::new()).collect();
    for item in items {
        let lightest = (0..count)
            .min_by_key(|&i| (shards[i].iter().map(|i| i.weight).sum::<u32>(), i))
            .expect("at least one shard");
        shards[lightest].push(item);
    }
    shards
}

/// `I/N` with 1 <= I <= N, as a shard or a part: (I - 1, N).
pub fn parse_part(value: &str) -> R<(usize, usize)> {
    let (index, count) = value
        .split_once('/')
        .and_then(|(i, n)| Some((i.parse::<usize>().ok()?, n.parse::<usize>().ok()?)))
        .filter(|&(i, n)| (1..=n).contains(&i))
        .ok_or_else(|| format!("{value}: expected I/N with 1 <= I <= N"))?;
    Ok((index - 1, count))
}

struct Options {
    shard: (usize, usize),
    shards: usize,
    stage: Option<Stage>,
    output: Option<PathBuf>,
    review: Option<String>,
    kernel: bool,
    full: bool,
    no_early_stop: bool,
    profile: Option<String>,
    target: Option<String>,
    selection: Option<Vec<String>>,
    directories: Vec<PathBuf>,
}

impl Options {
    fn parse(args: &[String]) -> R<Self> {
        let mut options = Self {
            shard: (0, 1),
            shards: 1,
            stage: None,
            output: None,
            review: None,
            kernel: false,
            full: false,
            no_early_stop: false,
            profile: None,
            target: None,
            selection: None,
            directories: Vec::new(),
        };
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let mut value = || {
                rest.next()
                    .cloned()
                    .ok_or_else(|| format!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--shard" => options.shard = parse_part(&value()?)?,
                "--shards" => {
                    options.shards = value()?
                        .parse()
                        .ok()
                        .filter(|&n| n > 0)
                        .ok_or("--shards needs a positive number")?;
                }
                "--stage" => {
                    options.stage = Some(match value()?.as_str() {
                        "timing" => Stage::Timing,
                        "memory" => Stage::Memory,
                        other => return Err(format!("--stage {other}: timing or memory")),
                    });
                }
                "--output" => options.output = Some(PathBuf::from(value()?)),
                "--review" => options.review = Some(value()?),
                "--kernel" => options.kernel = true,
                "--full" => options.full = true,
                "--no-early-stop" => options.no_early_stop = true,
                "--profile" => options.profile = Some(value()?),
                "--target" => options.target = Some(value()?),
                "--items" => {
                    options.selection = Some(value()?.split(',').map(str::to_owned).collect());
                }
                flag if flag.starts_with("--") => return Err(USAGE.into()),
                directory => options.directories.push(PathBuf::from(directory)),
            }
        }
        Ok(options)
    }
}

/// The items of shard `index` of `count`, writing under `out`.
fn shard_items(
    root: &Path,
    out: &str,
    full: bool,
    (index, count): (usize, usize),
    selection: Option<&[String]>,
) -> R<Vec<Item>> {
    let target = crate::harness::report::gate_target()?;
    let release = crate::harness::gate_build::release_dir()?;
    let all = select(
        items(
            &report_features(root)?,
            out,
            full,
            &target,
            &release,
            &capture_cases(root)?,
        ),
        selection,
    )?;
    Ok(shards(all, count).swap_remove(index))
}

/// The plan's items named in `selection` (all without one), in plan order: a run of
/// part of the gate. A name the plan does not hold is an error.
fn select(items: Vec<Item>, selection: Option<&[String]>) -> R<Vec<Item>> {
    let Some(names) = selection else {
        return Ok(items);
    };
    if let Some(unknown) = names
        .iter()
        .find(|name| !items.iter().any(|item| &item.name == *name))
    {
        return Err(format!(
            "--items {unknown}: not an item of this target's plan"
        ));
    }
    Ok(items
        .into_iter()
        .filter(|item| names.contains(&item.name))
        .collect())
}

/// The targets the gate runs on.
pub const TARGETS: [&str; 2] = [
    "thumbv7neon-unknown-linux-gnueabihf",
    "aarch64-unknown-linux-gnu",
];

pub fn main(root: &Path, args: &[String]) -> R<i32> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    let options = Options::parse(rest)?;
    match command.as_str() {
        "plan" => {
            let target = options.target.as_deref().unwrap_or("TARGET");
            let all = select(
                items(
                    &report_features(root)?,
                    "OUT",
                    false,
                    target,
                    "target/release",
                    &capture_cases(root)?,
                ),
                options.selection.as_deref(),
            )?;
            for (i, shard) in shards(all, options.shards).iter().enumerate() {
                let weight: u32 = shard.iter().map(|item| item.weight).sum();
                let names: Vec<&str> = shard.iter().map(|item| item.name.as_str()).collect();
                println!(
                    "{}/{} {weight} min: {}",
                    i + 1,
                    options.shards,
                    names.join(" ")
                );
            }
            for (id, coverage) in COVERED {
                match coverage {
                    Coverage::Item(command) => println!("{id}: in {command}"),
                    Coverage::Excluded(reason) => println!("{id}: not gated, {reason}"),
                }
            }
            Ok(0)
        }
        "check" => check(
            root,
            options.shards,
            options.target.as_deref().unwrap_or("TARGET"),
            options.selection.as_deref(),
        ),
        "preflight" => {
            let mut args = vec!["preflight".to_owned()];
            for item in shard_items(
                root,
                "OUT",
                false,
                options.shard,
                options.selection.as_deref(),
            )? {
                args.extend(item.preflight);
            }
            if let Some(review) = options.review {
                args.extend(["--review".to_owned(), review]);
            }
            if options.kernel {
                args.push("--kernel".into());
            }
            status(
                Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
                    .args(&args)
                    .current_dir(root),
            )
        }
        "run" => {
            let stage = options
                .stage
                .ok_or("gate run needs --stage timing|memory")?;
            let output = options.output.ok_or("gate run needs --output DIR")?;
            run(
                root,
                &output,
                stage,
                options.shard,
                options.full,
                options.no_early_stop,
                options.selection.as_deref(),
            )
        }
        "summary" => {
            let output = options.output.ok_or("gate summary needs --output FILE")?;
            summary(root, &output, &options.directories)
        }
        "test" => {
            let output = options.output.ok_or("gate test needs --output DIR")?;
            let Some(profile @ ("checked" | "release")) = options.profile.as_deref() else {
                return Err("gate test needs --profile checked|release".into());
            };
            test(root, &output, profile, options.full)
        }
        "--help" | "-h" => {
            println!("{USAGE}");
            Ok(0)
        }
        _ => Err(USAGE.into()),
    }
}

fn status(command: &mut Command) -> R<i32> {
    command
        .status()
        .map(|s| s.code().unwrap_or(1))
        .map_err(|e| format!("{command:?}: {e}"))
}

fn git(root: &Path, args: &[&str]) -> R<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
