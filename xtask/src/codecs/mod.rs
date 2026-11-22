//! `cargo xtask codecs`: correctness grading, its summary, the reader packet
//! comparison, paired timing and memory, and output identities of the codec crates
//! against the pinned FFmpeg decoders.

mod aac_access_units;
mod aac_ps_tables;
mod aac_sources;
pub mod correctness;
pub mod derived;
pub mod equivalence;
pub mod fuzz;
pub mod isobmff_reader;
pub mod memory;
pub mod readers;
pub mod summary;
pub mod timing;

use std::path::{Path, PathBuf};

use crate::json::Value;

pub(crate) const INITIALIZED_AAC_REFERENCE: &str = "ffmpeg-aac-initialized-pcm";

pub const USAGE: &str = "usage: cargo xtask codecs <command>

  aac-access-units --out DIR [--record]
                 prepare the configured AAC-LC workloads and compare every native PCM plane
  aac-ps-tables [--check] [--ffmpeg FF] [--python PY] [--work DIR]
                 rerun the Parametric Stereo table recovery against the pinned FFmpeg 9.0.2
                 decoder and write, or with --check compare, the generated PS tables
  aac-sources --out DIR
                 author the fixed-layout and indexed-rate AAC-LC sources
  derive --ffmpeg FF
                 make the derived streams of harness/codecs/derived-streams.json from their
                 FATE sources, fetched by their pins, with the decode profile's ffmpeg, and
                 check each against its correctness manifest pin
  manifest --out M.json LANE=DIR[,DIR...]...
                 hash every vector of each lane (h264, hevc, ac3, eac3, aac, aac-mov, dts, flac,
                 mp3, mp2, mp1, vorbis, opus; reader-ogg, reader-flac, reader-mp3,
                 reader-aac, reader-ac3, reader-eac3)
  grade --manifest M.json --root R --bin DIR --ffmpeg FF --ffprobe FP --codec-worker W
        [--codec-worker-for OPERATION=W2 ...] --reference R2 --out OUT.jsonl [--scratch DIR]
        [--only SUBSTR,...] [--lanes LANE,...]
                 grade every vector, or those of the named lanes, by
                 harness/codecs/numeric-rules.json; appends one row each. A lane whose
                 operation has a --codec-worker-for worker decodes with that worker
  compare-hevc --decoder EXE --ffmpeg FF --ffprobe FP FILE...
                 stream individual files through the same planar decoder comparison as grade;
                 print one JSON row per file and return success only for exact matches
  summarize RUN.jsonl OUT.json [--require-pass]
                 classify a grading run against the declared lane selection; with
                 --require-pass fail on any vector that fails and is not a listed open case
  readers --manifest M.json --root R --ffprobe FP --codec-worker W --out OUT.jsonl
          [--seeks S.json] [--only SUBSTR,...]
                 compare every packet the container readers emit, and Ogg's compat view,
                 with ffprobe by harness/codecs/reader-rules.json and the reviewed
                 difference lists of reader-differences.json, and the packets after
                 each seek case of S.json (lane reader-ogg-seek); appends one row per
                 case, writes the observed lists to OUT.differences.json, and fails
                 when a case fails
  isobmff-reader --ffprobe FP --codec-worker W --out OUT.jsonl [--only SUBSTR,...]
                 compare every packet the neutral ISO BMFF reader emits, read through and
                 after a seek, with ffprobe by harness/features/isobmff-reader/rules.json and
                 the reviewed difference lists of its differences.json; appends one row per
                 case, writes the observed lists to OUT.differences.json and fails on a
                 failing case
  time --candidate W [--candidate-for OPERATION=W2 ...] --reference W --workloads F
       --root R --ledger F --out DIR --label L [--only ID,...] [--full] [--no-early-stop]
                 paired timing of the decoder workloads (30 pairs, seed 280, retried on 281 and 282,
                 ending early once settled below the ceiling)
  memory --candidate W [--candidate-for OPERATION=W2 ...] --reference W --workloads F
         --root R --out DIR [--pairs N] [--seed S] [--only ID,...] [--part I/N] [--full]
         [--no-early-stop]
                 paired peak heap and total resident peak RSS of the decoder workloads
                 under the shared observer (30 pairs, seed 283); --part I/N takes
                 every Nth workload from the Ith. A workload whose operation has a
                 --candidate-for worker, here and in time, is measured from that worker
                 a result a kept evidence set holds is reused unless --full
  equivalence --output F [--compare F] [--jobs N]
                 record every decoder output identity; --compare checks an earlier record
  fuzz --lane LANE --output DIR [--rounds N] [--seed N] [--timeout-seconds S] [--only SUBSTR,...]
                 cargo xtask fuzz campaign over the lane's vectors: release and checked
                 codec-worker builds decode each mutation whole (4 rounds, seed 280, 30 s)";

/// A measuring codec command as a gate step ([`crate::harness::reuse`]): its key
/// binds the workload list and every input it names, and every worker.
struct Gate<'a> {
    command: &'static str,
    settings: Value,
    workloads: &'a Path,
    root: &'a Path,
    candidates: &'a Candidates,
    reference: &'a Path,
    out: &'a Path,
}

impl Gate<'_> {
    /// Reuse the cited result unless `full`, else run and record the step.
    fn run(
        self,
        repository: &Path,
        full: bool,
        run: impl FnOnce() -> Result<(), String>,
    ) -> Result<i32, String> {
        let list = crate::checks::read_json(self.workloads)?;
        let mut inputs = vec![self.workloads.to_path_buf()];
        for workload in list
            .get("workloads")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            if let Some(path) = workload.get("path").and_then(Value::as_str) {
                inputs.push(self.root.join(path));
            }
        }
        let files: Vec<&Path> = inputs.iter().map(PathBuf::as_path).collect();
        let mut workers = self.candidates.roles();
        workers.push(("reference".to_owned(), self.reference));
        let workers: Vec<(&str, &Path)> = workers
            .iter()
            .map(|(role, worker)| (role.as_str(), *worker))
            .collect();
        let key = crate::harness::reuse::Key::new(
            repository,
            self.command,
            self.settings,
            &files,
            &workers,
        )?;
        crate::harness::reuse::step(repository, self.out, &key, full, || run().map(|()| 0))
    }
}

/// Keep the default key compatible with existing results. Full pair collections
/// have a distinct key because an early collection may have fewer observations.
fn collection_settings<'a>(
    members: impl IntoIterator<Item = (&'a str, Value)>,
    early_stop: bool,
) -> Value {
    Value::object(
        members
            .into_iter()
            .chain((!early_stop).then_some(("early_stop", Value::Bool(false)))),
    )
}

/// The candidate worker of each operation: `--candidate W` (`--codec-worker W` for a
/// grade) for every operation, and `--candidate-for OPERATION=W` for an operation
/// measured from a worker built apart, such as the AAC decoder's nightly build.
pub struct Candidates {
    default: PathBuf,
    by_operation: Vec<(String, PathBuf)>,
}

impl Candidates {
    /// The workers of `--NAME` and `--NAME-for`; an operation may be named once.
    fn parse(a: &Args, name: &str) -> Result<Self, String> {
        let flag = format!("{name}-for");
        let mut by_operation: Vec<(String, PathBuf)> = Vec::new();
        for (key, value) in &a.values {
            if *key != flag {
                continue;
            }
            let Some((operation, worker)) = value.split_once('=') else {
                return Err(format!("--{flag} needs OPERATION=WORKER: {value}"));
            };
            if operation.is_empty() || worker.is_empty() {
                return Err(format!("--{flag} needs OPERATION=WORKER: {value}"));
            }
            if by_operation.iter().any(|(named, _)| named == operation) {
                return Err(format!("--{flag}: {operation} named twice"));
            }
            by_operation.push((operation.to_owned(), PathBuf::from(worker)));
        }
        Ok(Self {
            default: a.path(name)?,
            by_operation,
        })
    }

    /// The worker that measures `operation`.
    pub fn of(&self, operation: &str) -> &Path {
        self.by_operation
            .iter()
            .find(|(named, _)| named == operation)
            .map_or(&self.default, |(_, worker)| worker)
    }

    /// The workers named for an operation, by operation.
    pub fn by_operation(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.by_operation
            .iter()
            .map(|(operation, worker)| (operation.as_str(), worker.as_path()))
    }

    /// Each worker with its role: `candidate` for the default, `candidate-for
    /// OPERATION` for the others, in the order given.
    pub fn roles(&self) -> Vec<(String, &Path)> {
        let mut roles = vec![("candidate".to_owned(), self.default.as_path())];
        roles.extend(
            self.by_operation.iter().map(|(operation, worker)| {
                (format!("candidate-for {operation}"), worker.as_path())
            }),
        );
        roles
    }
}

/// The list's `open_workloads` on `target`: workloads whose bounds fail there today,
/// each with the measured `cause`. An entry with `targets` is open on those of the
/// gate's targets only, and must hold elsewhere. Every id must be a workload of the
/// list, listed once.
pub fn open_workloads<'a>(list: &'a Value, target: &str) -> Result<Vec<&'a str>, String> {
    let array = |value: &'a Value, key: &str| value.get(key).and_then(Value::as_array);
    let ids: Vec<&str> = array(list, "workloads")
        .unwrap_or_default()
        .iter()
        .filter_map(|workload| workload.get("id").and_then(Value::as_str))
        .collect();
    let mut listed = Vec::new();
    let mut open = Vec::new();
    for entry in array(list, "open_workloads").unwrap_or_default() {
        let text = |key| entry.get(key).and_then(Value::as_str);
        let (Some(id), Some(cause)) = (text("id"), text("cause")) else {
            return Err("an open workload needs an id and a cause".into());
        };
        if cause.trim().is_empty() {
            return Err(format!("open workload {id}: its cause is empty"));
        }
        if !ids.contains(&id) {
            return Err(format!("open workload {id}: not a workload of the list"));
        }
        if listed.contains(&id) {
            return Err(format!("open workload {id}: listed twice"));
        }
        listed.push(id);
        if let Some(scope) = entry.get("targets") {
            let scope = scope.as_array().unwrap_or_default();
            let known = |t: &Value| {
                t.as_str()
                    .is_some_and(|t| crate::gate::TARGETS.contains(&t))
            };
            if scope.is_empty() || !scope.iter().all(known) {
                return Err(format!(
                    "open workload {id}: its targets must be some of the gate's targets"
                ));
            }
            if !scope.iter().any(|t| t.as_str() == Some(target)) {
                continue;
            }
        }
        open.push(id);
    }
    Ok(open)
}

/// The verdict of a codec collection's bounds, `results` each workload's id and whether
/// its bound held. Where the gate applies (`gate`: the target measured on native Linux
/// with glibc, AArch64 or 32-bit ARM, see [`memory::gate_platform`]) a failed bound
/// fails the collection, as a failed case fails a grade, and its lines tell failures
/// that are only the list's `open_workloads` on that target from others, as a grade's
/// do; elsewhere the bounds are diagnostics and fail nothing. Returns the lines to
/// print and whether the collection fails.
pub fn bounds(
    list: &Value,
    name: &str,
    what: &str,
    results: &[(String, bool)],
    gate: Option<&str>,
) -> Result<(Vec<String>, bool), String> {
    let passed = results.iter().filter(|(_, held)| *held).count();
    let total = results.len();
    let Some(target) = gate else {
        return Ok((
            vec![format!(
                "{name} {what}: diagnostic on this target, not gated: {passed} of {total} within the bound"
            )],
            false,
        ));
    };
    let open = open_workloads(list, target)?;
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, held)| !held)
        .map(|(id, _)| id.as_str())
        .collect();
    let (listed, other): (Vec<&str>, Vec<&str>) = failed.iter().partition(|id| open.contains(id));
    let mut lines = vec![format!("{name} {what}: {passed} of {total} cases pass")];
    if !listed.is_empty() {
        lines.push(format!(
            "{name} {what}: {} open failures, as listed: {}",
            listed.len(),
            listed.join(", ")
        ));
    }
    if !other.is_empty() {
        lines.push(format!("{name} {what}: failing: {}", other.join(", ")));
    }
    Ok((lines, !failed.is_empty()))
}

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
        "derive" => {
            let a = Args::parse(rest, &["ffmpeg"], &[])?;
            derived::run(root, &a.path("ffmpeg")?)?;
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
                    "codec-worker-for",
                    "reference",
                    "out",
                    "scratch",
                    "only",
                    "lanes",
                ],
                &[],
            )?;
            let grade = correctness::Grade {
                manifest: a.path("manifest")?,
                root: a.path("root")?,
                bin: a.path("bin")?,
                ffmpeg: a.required("ffmpeg")?.to_owned(),
                ffprobe: a.required("ffprobe")?.to_owned(),
                codec_workers: Candidates::parse(&a, "codec-worker")?,
                reference: a.required("reference")?.to_owned(),
                out: a.path("out")?,
                scratch: a.get("scratch").map(PathBuf::from),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
                lanes: a
                    .get("lanes")
                    .map(|l| l.split(',').map(str::to_owned).collect()),
                rules: correctness::Rules::load(root)?,
            };
            correctness::grade(&grade)?;
        }
        "compare-hevc" => {
            let a = Args::parse(rest, &["decoder", "ffmpeg", "ffprobe"], &[])?;
            if a.positional.is_empty() {
                return Err("compare-hevc needs at least one input file".into());
            }
            return correctness::compare_hevc(
                a.required("decoder")?,
                a.required("ffmpeg")?,
                a.required("ffprobe")?,
                &a.positional,
            );
        }
        "readers" => {
            let a = Args::parse(
                rest,
                &[
                    "manifest",
                    "root",
                    "ffprobe",
                    "codec-worker",
                    "out",
                    "only",
                    "seeks",
                ],
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
                seeks: match a.get("seeks") {
                    Some(seeks) => readers::SeekCase::load(Path::new(seeks))?,
                    None => Vec::new(),
                },
            })?;
        }
        "isobmff-reader" => {
            let a = Args::parse(rest, &["ffprobe", "codec-worker", "out", "only"], &[])?;
            return isobmff_reader::run(&isobmff_reader::Options {
                root: root.to_path_buf(),
                ffprobe: a.required("ffprobe")?.to_owned(),
                codec_worker: a.path("codec-worker")?,
                out: a.path("out")?,
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
            });
        }
        "memory" => {
            let a = Args::parse(
                rest,
                &[
                    "candidate",
                    "candidate-for",
                    "reference",
                    "workloads",
                    "root",
                    "out",
                    "pairs",
                    "seed",
                    "only",
                    "part",
                ],
                &["full", "no-early-stop"],
            )?;
            let options = memory::Options {
                repository: root.to_path_buf(),
                candidates: Candidates::parse(&a, "candidate")?,
                reference: a.path("reference")?,
                workloads: a.path("workloads")?,
                root: a.path("root")?,
                out: a.path("out")?,
                pairs: a.number("pairs")?.unwrap_or(memory::PAIRS),
                seed: a.number("seed")?.map_or(memory::SEED, |s| s as u64),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
                part: a.get("part").map(crate::gate::parse_part).transpose()?,
                early_stop: a.get("no-early-stop").is_none(),
            };
            let settings = collection_settings(
                [
                    ("pairs", Value::int(options.pairs as i64)),
                    ("part", a.get("part").map_or(Value::Null, Value::str)),
                    ("seed", Value::int(options.seed)),
                    (
                        "only",
                        options.only.as_deref().map_or(Value::Null, Value::str),
                    ),
                    (
                        "kernel_bound",
                        std::env::var("TRIERGON_RSS_KERNEL_BOUND").map_or(Value::Null, Value::str),
                    ),
                ],
                options.early_stop,
            );
            let gate = Gate {
                command: "codecs memory",
                settings,
                workloads: &options.workloads,
                root: &options.root,
                candidates: &options.candidates,
                reference: &options.reference,
                out: &options.out,
            };
            return gate.run(root, a.get("full").is_some(), || memory::run(&options));
        }
        "summarize" => {
            let a = Args::parse(rest, &[], &["require-pass"])?;
            match a.positional.as_slice() {
                [run, out] => {
                    summary::summarize(run, Path::new(out), a.get("require-pass").is_some())?;
                }
                _ => return Err(USAGE.to_owned()),
            }
        }
        "time" => {
            let a = Args::parse(
                rest,
                &[
                    "candidate",
                    "candidate-for",
                    "reference",
                    "workloads",
                    "root",
                    "ledger",
                    "out",
                    "label",
                    "only",
                ],
                &["full", "no-early-stop"],
            )?;
            let options = timing::Options {
                repository: root.to_path_buf(),
                candidates: Candidates::parse(&a, "candidate")?,
                reference: a.path("reference")?,
                workloads: a.path("workloads")?,
                root: a.path("root")?,
                ledger: a.path("ledger")?,
                out: a.path("out")?,
                label: a.required("label")?.to_owned(),
                only: a.get("only").filter(|o| !o.is_empty()).map(str::to_owned),
                early_stop: a.get("no-early-stop").is_none(),
            };
            let settings = collection_settings(
                [
                    ("label", Value::str(&options.label)),
                    (
                        "only",
                        options.only.as_deref().map_or(Value::Null, Value::str),
                    ),
                ],
                options.early_stop,
            );
            let gate = Gate {
                command: "codecs time",
                settings,
                workloads: &options.workloads,
                root: &options.root,
                candidates: &options.candidates,
                reference: &options.reference,
                out: &options.out,
            };
            return gate.run(root, a.get("full").is_some(), || timing::run(&options));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pair_settings_cannot_reuse_an_early_collection() {
        let members = || [("pairs", Value::int(30))];
        assert_eq!(
            collection_settings(members(), true),
            Value::object(members())
        );
        let full = collection_settings(members(), false);
        assert_eq!(full.get("early_stop"), Some(&Value::Bool(false)));
        assert_eq!(full.get("pairs"), Some(&Value::int(30)));
    }
}
