//! `cargo xtask capture`: correctness, qualification and output identities of
//! the capture pipeline against the FFmpeg capture reference.

pub mod compare;
mod config;
pub mod equivalence;
pub mod qualify;
pub mod summary;
mod ts;

use std::path::{Path, PathBuf};

use crate::codecs::{Args, default_jobs};
use crate::harness::reuse;

pub const USAGE: &str = "usage: cargo xtask capture <command>

  compare --candidate W --reference W --ffmpeg-bin DIR --case JSON|FILE --input F --out DIR
                 conversion, mux, encoder and pipeline rules of one case; prints a summary line
  qualify --candidate W --reference W [--reference-small W] --oracle-bin DIR --observer SO
          --inputs DIR --out DIR [--cases F] [--only ID...] [--pairs N] [--iterations N]
          [--memory-pairs N] [--seed N] [--full] [--no-early-stop]
          [--stage parity | --stage timing | --stage memory --timing DIR] [--and RUN...]
                 --stage parity compares outputs without collecting measurements
                 --stage timing runs correctness and timing alone (on one CPU); --stage
                 memory collects every case's memory in parallel from a timing run's
                 records in DIR; without --stage one run does both; memory runs
                 joined by --and collect all their cases in one collection, each
                 run with its own --out, gate result and reuse
                 correctness, paired runtime, peak heap and total resident RSS per required case
                 (native Linux); inputs are generated with harness/capture/generate_inputs.py
                 using $TRIERGON_PYTHON (default python3), which needs numpy; a run a kept
                 evidence set holds is reused unless --full
  summarize RUN  a qualify run's per-case results as a Markdown table
  equivalence --output F [--compare F] [--jobs N]
                 record every capture-worker output identity; --compare checks an earlier record";

/// `--only ID...`: the values that follow until the next option.
fn split_only(args: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut rest, mut only) = (Vec::new(), Vec::new());
    let mut collecting = false;
    for arg in args {
        if arg == "--only" {
            collecting = true;
        } else if collecting && !arg.starts_with("--") {
            only.push(arg.clone());
        } else {
            collecting = false;
            rest.push(arg.clone());
        }
    }
    (rest, only)
}

/// Separates the runs of a `capture qualify` that measures several together.
pub const AND: &str = "--and";

/// The options of one `capture qualify` run, and whether it is `--full`.
fn qualify_options(root: &Path, args: &[String]) -> Result<(qualify::Options, bool), String> {
    let (rest, only) = split_only(args);
    let a = Args::parse(
        &rest,
        &[
            "candidate",
            "reference",
            "reference-small",
            "oracle-bin",
            "observer",
            "cases",
            "inputs",
            "out",
            "pairs",
            "iterations",
            "memory-pairs",
            "seed",
            "stage",
            "timing",
        ],
        &["full", "no-early-stop"],
    )?;
    let stage = match (a.get("stage"), a.get("timing")) {
        (None, None) => qualify::Stage::Both,
        (Some("parity"), None) => qualify::Stage::Parity,
        (Some("timing"), None) => qualify::Stage::Timing,
        (Some("memory"), Some(timing)) => qualify::Stage::Memory {
            timing: PathBuf::from(timing),
        },
        _ => return Err(USAGE.to_owned()),
    };
    let options = qualify::Options {
        candidate: a.required("candidate")?.to_owned(),
        reference: a.required("reference")?.to_owned(),
        reference_small: a
            .get("reference-small")
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        oracle_bin: a.required("oracle-bin")?.to_owned(),
        observer: a.required("observer")?.to_owned(),
        cases: a
            .get("cases")
            .map_or_else(|| root.join("harness/capture/cases.json"), PathBuf::from),
        only,
        inputs: a.path("inputs")?,
        out: a.path("out")?,
        pairs: a.number("pairs")?.unwrap_or(30),
        iterations: a.number("iterations")?.unwrap_or(3),
        memory_pairs: a.number("memory-pairs")?.unwrap_or(30),
        seed: a.number("seed")?.map_or(20_261_102, |s| s as u64),
        early_stop: a.get("no-early-stop").is_none(),
        stage,
    };
    Ok((options, a.get("full").is_some()))
}

/// Memory runs measured together: a run a kept set holds is reused unless it is
/// `--full`, the others are collected side by side in one collection, and each
/// records its own gate result in its `--out`. 1 if any run failed.
fn qualify_together(root: &Path, runs: &[(qualify::Options, bool)]) -> i32 {
    let mut failed = 0;
    let mut fail = |options: &qualify::Options, error: &str| {
        eprintln!("capture qualify --out {}: {error}", options.out.display());
        failed += 1;
    };
    let mut measured = Vec::new();
    for (options, full) in runs {
        let key = match qualify::gate_key(root, options) {
            Ok(key) => key,
            Err(error) => {
                fail(options, &error);
                continue;
            }
        };
        match reuse::reused(root, &options.out, &key, *full) {
            Ok(None) => measured.push((options, key)),
            Ok(Some(0)) => {}
            Ok(Some(status)) => fail(options, &format!("reused with status {status}")),
            Err(error) => fail(options, &error),
        }
    }
    if !measured.is_empty() {
        let options: Vec<&qualify::Options> =
            measured.iter().map(|(options, _)| *options).collect();
        for ((options, key), result) in measured.iter().zip(qualify::run_memory(root, &options)) {
            if let Err(error) = result.and_then(|()| reuse::record(&options.out, key, 0, None)) {
                fail(options, &error);
            }
        }
    }
    i32::from(failed > 0)
}

/// Run one `capture` command; the result is the process exit code.
pub fn main(root: &Path, args: &[String]) -> Result<i32, String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    match command.as_str() {
        "compare" => {
            let a = Args::parse(
                rest,
                &[
                    "candidate",
                    "reference",
                    "ffmpeg-bin",
                    "case",
                    "input",
                    "out",
                ],
                &[],
            )?;
            compare::run(
                root,
                &compare::Options {
                    candidate: a.required("candidate")?.to_owned(),
                    reference: a.required("reference")?.to_owned(),
                    ffmpeg_bin: a.required("ffmpeg-bin")?.to_owned(),
                    case: a.required("case")?.to_owned(),
                    input: a.required("input")?.to_owned(),
                    out: a.required("out")?.to_owned(),
                },
            )?;
        }
        "qualify" => {
            let runs = rest
                .split(|arg| arg == AND)
                .map(|run| qualify_options(root, run))
                .collect::<Result<Vec<_>, _>>()?;
            if let [(options, full)] = &runs[..] {
                let key = qualify::gate_key(root, options)?;
                let run = || qualify::run(root, options).map(|()| 0);
                // A memory stage reads the records of its timing stage, so a reused
                // timing stage restores them.
                return if matches!(options.stage, qualify::Stage::Timing) {
                    reuse::restoring_step(root, &options.out, &key, *full, run)
                } else {
                    reuse::step(root, &options.out, &key, *full, run)
                };
            }
            if runs
                .iter()
                .any(|(options, _)| !matches!(options.stage, qualify::Stage::Memory { .. }))
            {
                return Err(USAGE.to_owned());
            }
            return Ok(qualify_together(root, &runs));
        }
        "summarize" => match rest {
            [run] => summary::run(Path::new(run))?,
            _ => return Err(USAGE.to_owned()),
        },
        "equivalence" => {
            let a = Args::parse(rest, &["output", "compare", "jobs"], &[])?;
            return equivalence::run(
                root,
                &a.path("output")?,
                a.get("compare").map(Path::new),
                a.number("jobs")?.unwrap_or_else(default_jobs),
            );
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs join only at `--stage memory`, where they share one collection.
    #[test]
    fn only_memory_runs_join() {
        let run = |stage: &[&str], out: &str| {
            let mut args = vec![
                "--candidate",
                "c",
                "--reference",
                "r",
                "--oracle-bin",
                "bin",
                "--observer",
                "o.so",
                "--inputs",
                "inputs",
                "--out",
                out,
            ];
            args.extend(stage);
            args.extend(["--only", out]);
            args.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        let root = Path::new(".");
        let memory = run(&["--stage", "memory", "--timing", "t"], "c01");
        let (options, full) = qualify_options(root, &memory).unwrap();
        assert!(!full && options.only == ["c01"]);
        assert!(matches!(options.stage, qualify::Stage::Memory { .. }));
        let mut joined = vec!["qualify".to_owned()];
        joined.extend(run(&["--stage", "timing"], "c01"));
        joined.push(AND.into());
        joined.extend(memory);
        assert_eq!(main(root, &joined), Err(USAGE.to_owned()));
    }
}
