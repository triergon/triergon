//! `cargo xtask capture`: correctness, qualification and output identities of
//! the capture pipeline against the FFmpeg capture reference.

pub mod compare;
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
          [--stage timing | --stage memory --timing DIR]
                 --stage timing runs correctness and timing alone (on one CPU); --stage
                 memory collects every case's memory in parallel from a timing run's
                 records in DIR; without --stage one run does both
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
            compare::run(&compare::Options {
                candidate: a.required("candidate")?.to_owned(),
                reference: a.required("reference")?.to_owned(),
                ffmpeg_bin: a.required("ffmpeg-bin")?.to_owned(),
                case: a.required("case")?.to_owned(),
                input: a.required("input")?.to_owned(),
                out: a.required("out")?.to_owned(),
            })?;
        }
        "qualify" => {
            let (rest, only) = split_only(rest);
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
            let key = qualify::gate_key(root, &options)?;
            return reuse::step(root, &options.out, &key, a.get("full").is_some(), || {
                qualify::run(root, &options).map(|()| 0)
            });
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
