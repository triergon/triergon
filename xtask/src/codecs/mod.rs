//! `cargo xtask codecs`: correctness grading, its summary, the reader packet
//! comparison, paired timing and memory, and output identities of the codec crates
//! against the pinned FFmpeg decoders.

mod aac_access_units;
mod aac_ps_tables;
mod aac_sources;
pub mod correctness;
pub mod equivalence;
pub mod fuzz;
pub mod memory;
pub mod readers;
pub mod summary;
pub mod timing;

use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: cargo xtask codecs <command>

  aac-access-units --out DIR [--record]
                 prepare the configured AAC-LC workloads and compare every native PCM plane
  aac-ps-tables [--check] [--ffmpeg FF] [--python PY] [--work DIR]
                 rerun the Parametric Stereo table recovery against the pinned FFmpeg 9.0.2
                 decoder and write, or with --check compare, the generated PS tables
  aac-sources --out DIR
                 author the fixed-layout and indexed-rate AAC-LC sources
  manifest --out M.json LANE=DIR[,DIR...]...
                 hash every vector of each lane (h264, hevc, ac3, eac3, aac, aac-mov, dts, flac,
                 mp3, mp2, mp1, vorbis, opus; reader-ogg, reader-flac, reader-mp3,
                 reader-aac, reader-ac3, reader-eac3)
  grade --manifest M.json --root R --bin DIR --ffmpeg FF --ffprobe FP --codec-worker W
        --reference R2 --out OUT.jsonl [--scratch DIR] [--only SUBSTR,...]
                 grade every vector by harness/codecs/numeric-rules.json; appends one row each
  summarize RUN.jsonl OUT.json
                 classify a grading run against the declared lane selection
  readers --manifest M.json --root R --ffprobe FP --codec-worker W --out OUT.jsonl
          [--only SUBSTR,...]
                 compare every packet the container readers emit, and Ogg's compat view,
                 with ffprobe by harness/codecs/reader-rules.json and the reviewed
                 difference lists of reader-differences.json; appends one row per input
                 and view, and writes the observed lists to OUT.differences.json
  time --candidate W --reference W --workloads F --root R --ledger F --out DIR --label L
       [--only ID,...]
                 paired timing of the decoder workloads (30 pairs, seed 280, retried on 281 and 282)
  memory --candidate W --reference W --workloads F --root R --out DIR
         [--pairs N] [--seed S] [--only ID,...]
                 paired peak heap and total resident peak RSS of the decoder workloads
                 under the shared observer (30 pairs, seed 283)
  equivalence --output F [--compare F] [--jobs N]
                 record every decoder output identity; --compare checks an earlier record
  fuzz --lane LANE --output DIR [--rounds N] [--seed N] [--timeout-seconds S] [--only SUBSTR,...]
                 cargo xtask fuzz campaign over the lane's vectors: release and checked
                 codec-worker builds decode each mutation whole (4 rounds, seed 280, 30 s)";

/// `--name value` and `--name=value` options and positional arguments, as
/// `argparse` accepts them; `switches` take no value.
pub struct Args {
    pub values: Vec<(String, String)>,
    pub positional: Vec<String>,
}

impl Args {
    pub fn parse(args: &[String], names: &[&str], switches: &[&str]) -> Result<Args, String> {
        let mut values = Vec::new();
        let mut positional = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let Some(flag) = arg.strip_prefix("--") else {
                positional.push(arg.clone());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (flag, None),
            };
            if switches.contains(&name) {
                values.push((name.to_owned(), String::new()));
                continue;
            }
            if !names.contains(&name) {
                return Err(format!("unrecognized argument: {arg}"));
            }
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("--{name} needs a value"))?,
            };
            values.push((name.to_owned(), value));
        }
        Ok(Args { values, positional })
    }

    /// The last value given for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn required(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("the following arguments are required: --{name}"))
    }

    pub fn path(&self, name: &str) -> Result<PathBuf, String> {
        self.required(name).map(PathBuf::from)
    }

    pub fn number(&self, name: &str) -> Result<Option<usize>, String> {
        self.get(name)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("--{name}: not a number: {v}"))
            })
            .transpose()
    }
}

/// Worker threads when `--jobs` is not given: one per CPU.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Run one `codecs` command; the result is the process exit code.
pub fn main(root: &Path, args: &[String]) -> Result<i32, String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    match command.as_str() {
        "aac-access-units" => {
            let a = Args::parse(rest, &["out"], &["record"])?;
            aac_access_units::run(root, &a.path("out")?, a.get("record").is_some())?;
        }
        "aac-ps-tables" => {
            let a = Args::parse(rest, &["ffmpeg", "python", "work"], &["check"])?;
            return aac_ps_tables::main(
                root,
                &aac_ps_tables::Options {
                    check: a.get("check").is_some(),
                    ffmpeg: a.get("ffmpeg").map(PathBuf::from),
                    python: a.get("python").unwrap_or("python3").to_owned(),
                    work: a.get("work").map(PathBuf::from),
                },
            );
        }
        "aac-sources" => {
            let a = Args::parse(rest, &["out"], &[])?;
            aac_sources::run(root, &a.path("out")?)?;
        }
        "manifest" => {
            let a = Args::parse(rest, &["out"], &[])?;
            if a.positional.is_empty() {
                return Err(USAGE.to_owned());
            }
            correctness::manifest(&a.path("out")?, &a.positional)?;
        }
        "grade" => {
            let a = Args::parse(
                rest,
                &[
                    "manifest",
                    "root",
                    "bin",
                    "ffmpeg",
                    "ffprobe",
                    "codec-worker",
                    "reference",
                    "out",
                    "scratch",
                    "only",
                ],
                &[],
            )?;
            let grade = correctness::Grade {
                manifest: a.path("manifest")?,
                root: a.path("root")?,
                bin: a.path("bin")?,
                ffmpeg: a.required("ffmpeg")?.to_owned(),
                ffprobe: a.required("ffprobe")?.to_owned(),
                codec_worker: a.path("codec-worker")?,
                reference: a.required("reference")?.to_owned(),
                out: a.path("out")?,
                scratch: a.get("scratch").map(PathBuf::from),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
                rules: correctness::Rules::load(root)?,
            };
            correctness::grade(&grade)?;
        }
        "readers" => {
            let a = Args::parse(
                rest,
                &["manifest", "root", "ffprobe", "codec-worker", "out", "only"],
                &[],
            )?;
            readers::run(&readers::Options {
                manifest: a.path("manifest")?,
                root: a.path("root")?,
                ffprobe: a.required("ffprobe")?.to_owned(),
                codec_worker: a.path("codec-worker")?,
                out: a.path("out")?,
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
                rules: readers::Rules::load(root)?,
            })?;
        }
        "memory" => {
            let a = Args::parse(
                rest,
                &[
                    "candidate",
                    "reference",
                    "workloads",
                    "root",
                    "out",
                    "pairs",
                    "seed",
                    "only",
                ],
                &[],
            )?;
            memory::run(&memory::Options {
                repository: root.to_path_buf(),
                candidate: a.path("candidate")?,
                reference: a.path("reference")?,
                workloads: a.path("workloads")?,
                root: a.path("root")?,
                out: a.path("out")?,
                pairs: a.number("pairs")?.unwrap_or(memory::PAIRS),
                seed: a.number("seed")?.map_or(memory::SEED, |s| s as u64),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
            })?;
        }
        "summarize" => match rest {
            [run, out] => summary::summarize(run, Path::new(out))?,
            _ => return Err(USAGE.to_owned()),
        },
        "time" => {
            let a = Args::parse(
                rest,
                &[
                    "candidate",
                    "reference",
                    "workloads",
                    "root",
                    "ledger",
                    "out",
                    "label",
                    "only",
                ],
                &[],
            )?;
            timing::run(&timing::Options {
                repository: root.to_path_buf(),
                candidate: a.path("candidate")?,
                reference: a.path("reference")?,
                workloads: a.path("workloads")?,
                root: a.path("root")?,
                ledger: a.path("ledger")?,
                out: a.path("out")?,
                label: a.required("label")?.to_owned(),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
            })?;
        }
        "equivalence" => {
            let a = Args::parse(rest, &["output", "compare", "jobs"], &[])?;
            return equivalence::run(
                root,
                &a.path("output")?,
                a.get("compare").map(Path::new),
                a.number("jobs")?.unwrap_or_else(default_jobs),
            );
        }
        "fuzz" => {
            let a = Args::parse(
                rest,
                &[
                    "lane",
                    "output",
                    "rounds",
                    "seed",
                    "timeout-seconds",
                    "only",
                ],
                &[],
            )?;
            let timeout_seconds = match a.get("timeout-seconds") {
                Some(v) => v
                    .parse()
                    .map_err(|_| format!("--timeout-seconds: not a number: {v}"))?,
                None => 30.0,
            };
            let campaign = crate::fuzz::Campaign {
                manifest: PathBuf::new(),
                output: a.path("output")?,
                rounds: a.number("rounds")?.unwrap_or(4),
                seed: a.number("seed")?.unwrap_or(280) as u64,
                maximum_input_bytes: 4 << 20,
                maximum_output_bytes: 8 << 20,
                timeout_seconds: Some(timeout_seconds),
            };
            let only: Vec<String> = a
                .get("only")
                .unwrap_or_default()
                .split(',')
                .filter(|o| !o.is_empty())
                .map(str::to_owned)
                .collect();
            return fuzz::run(root, a.required("lane")?, &only, &campaign);
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(0)
}
