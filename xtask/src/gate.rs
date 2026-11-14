//! The full gate as one plan: every step a gate run takes, grouped into items
//! that can run on different hosts, and split into shards of about equal work.
//!
//! An item is the work for one measured thing: a feature's timing, memory and
//! grade, the ISO BMFF or packet-framing suite, the codecs, half the capture cases,
//! the tests, the reference comparisons. Its steps run on one host, in stages:
//! `timing` runs the parity and timing steps alone on CPU 1 with the RSS module
//! unloaded; `memory`, after the host has proved the kernel bound and rebuilt the
//! workers with the proof, runs the memory collections on every CPU but CPU 0 and
//! then grades, several grades at once. A shard is a set of items; `cargo xtask
//! gate summary` joins the shards' outputs into one result and checks that every
//! step of the plan ran once, on the same source and workers.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::json::Value;
use crate::sha256;

type R<T> = Result<T, String>;

pub const USAGE: &str = "usage: cargo xtask gate plan [--shards N]
       cargo xtask gate preflight [--shard I/N] --review FILE [--kernel]
       cargo xtask gate run --stage timing|memory --output DIR [--shard I/N] [--full]
       cargo xtask gate summary --output FILE DIR...
       cargo xtask gate test --profile debug|release --output DIR [--full]

  plan       print the gate's items and their planning weights (minutes on a
             32-bit ARMv7 c6g.4xlarge), split into N shards (default 1)
  preflight  `cargo xtask preflight` over everything shard I of N measures, with
             the RSS module review; --kernel after the kernel proof
  run        the steps of shard I of N (default 1/1) in one stage: timing (parity
             steps, then timing on CPU 1) or memory (memory on CPUs 1 to the last,
             then the grades); each step's output goes under DIR, its log to
             DIR/logs and its record to DIR/steps; --full measures what a kept
             evidence set holds
  summary    join the outputs of every shard of a run into FILE: each step's
             status and time, with a failure if a planned step did not run, a
             step failed, or the shards measured different sources or workers
  test       the workspace tests for the gate target in one profile: debug (with
             overflow checks and debug assertions) or release (the gate build's
             optimisation); a result a kept evidence set holds for the same tracked
             sources, test inputs, toolchain, target and host class is reused
             unless --full";

/// The shard file each run writes in its output directory.
const SHARD: &str = "shard.json";
const PAIRS: &str = "30";

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

/// The features the gate measures through `report`, with their corpus
/// directory, manifest and planning weight.
const FEATURES: [(&str, Option<&str>, u32); 23] = [
    ("mp4-forward", None, 4),
    ("ts-program", None, 4),
    ("matroska-audio-clocks", None, 1),
    ("raw-annex-b", None, 1),
    ("mpegts-pes", None, 9),
    ("hls-segments", Some("harness/fixtures/hls"), 11),
    (
        "matroska-startup-seek",
        Some(".research/startup-seek-fixtures"),
        5,
    ),
    ("matroska-colour", Some(".research/colour-fixtures"), 4),
    ("matroska-cues-preroll", Some(".research/seek-fixtures"), 1),
    ("matroska-demux", Some(".research/fixtures"), 2),
    (
        "matroska-header-removal",
        Some(".research/header-fixtures"),
        1,
    ),
    (
        "matroska-segment-metadata",
        Some(".research/catalogue-fixtures"),
        7,
    ),
    ("matroska-zlib", Some(".research/zlib-fixtures"), 1),
    ("ass-render", Some(""), 36),
    ("ass-render-host", Some(""), 1),
    ("subtitle-text", Some(""), 42),
    ("subtitle-text-host", Some(""), 2),
    ("subtitle-pgs", Some(SUBTITLE_CORPUS), 46),
    ("subtitle-dvb", Some(SUBTITLE_CORPUS), 29),
    ("subtitle-dvd", Some(SUBTITLE_CORPUS), 25),
    ("subtitle-dvd-host", Some(SUBTITLE_CORPUS), 1),
    ("subtitle-cc", Some(SUBTITLE_CORPUS), 24),
    ("subtitle-teletext", Some(SUBTITLE_CORPUS), 10),
];
const SUBTITLE_CORPUS: &str = ".research/subtitle-corpus";
/// The capture cases, which the capture items split between them.
const CAPTURE_CASES: &str = "harness/capture/cases.json";
/// The decode reference's programs, the codecs' and capture's independent decoder.
const FFMPEG_BIN: &str = ".research/reference-decode/install/bin";

fn xtask(args: &[&str]) -> Vec<String> {
    std::iter::once("xtask")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

fn step(name: impl Into<String>, stage: Stage, placement: Placement, command: Vec<String>) -> Step {
    Step {
        name: name.into(),
        stage,
        placement,
        command,
        environment: Vec::new(),
    }
}

fn manifest(feature: &str) -> String {
    if feature.starts_with("ass-") {
        format!(".research/ass/manifests/{feature}/corpus.json")
    } else {
        format!("harness/features/{feature}/corpus.json")
    }
}

/// A gate collection: timing on CPU 1, then memory under the kernel bound.
fn collections(name: &str, timing: Vec<String>, memory: Vec<String>) -> [Step; 2] {
    let mut memory = step(
        format!("memory-{name}"),
        Stage::Memory,
        Placement::Memory,
        memory,
    );
    memory.environment.push(("TRIERGON_RSS_KERNEL_BOUND", "1"));
    [
        step(
            format!("benchmark-{name}"),
            Stage::Timing,
            Placement::Timed,
            timing,
        ),
        memory,
    ]
}

/// A grade of a collection pair; with `full` it grades even what a kept set holds.
fn grade(name: &str, contract: &str, manifest: &str, out: &str, full: bool) -> Step {
    let mut grade = step(
        format!("grade-{name}"),
        Stage::Memory,
        Placement::Grade,
        xtask(&[
            "grade",
            &format!("{out}/benchmark/{name}/report.json"),
            &format!("{out}/memory/{name}/report.json"),
            "--contract",
            &format!("harness/features/{contract}/contract.json"),
            "--manifest",
            manifest,
            "--output",
            &format!("{out}/graded/{name}"),
        ]),
    );
    if full {
        grade.command.push("--full".into());
    }
    grade
}

/// Every item of the gate, writing under `out`, with `full` passed to the
/// commands that reuse a kept result. `target` is the gate target, `release`
/// the workers' directory and `capture` the capture case ids.
fn items(out: &str, full: bool, target: &str, release: &str, capture: &[String]) -> Vec<Item> {
    let with_full = |mut args: Vec<String>| {
        if full {
            args.push("--full".into());
        }
        args
    };
    let mut items = Vec::new();
    for (feature, corpus, weight) in FEATURES {
        let manifest = manifest(feature);
        let report = |mode: &str| {
            let mut args = xtask(&[
                "report",
                "--contract",
                &format!("harness/features/{feature}/contract.json"),
                "--manifest",
                &manifest,
            ]);
            if let Some(corpus) = corpus.filter(|c| !c.is_empty()) {
                args.extend(owned(&["--corpus", corpus]));
            }
            args.extend(owned(&[
                &format!("--{mode}"),
                "--pairs",
                PAIRS,
                "--output",
                &format!("{out}/{mode}/{feature}"),
            ]));
            with_full(args)
        };
        let mut steps: Vec<Step> =
            collections(feature, report("benchmark"), report("memory")).into();
        steps.push(grade(feature, feature, &manifest, out, full));
        let mut preflight = owned(&["--feature", feature]);
        match corpus {
            None => {}
            Some("") => {}
            Some(corpus) => preflight.extend(owned(&["--corpus", &format!("{feature}={corpus}")])),
        }
        if feature.starts_with("ass-") {
            preflight.extend(owned(&["--manifest", &format!("{feature}={manifest}")]));
        }
        items.push(Item {
            name: feature.to_owned(),
            weight,
            steps,
            preflight,
        });
    }

    let vector = |mode: &str| {
        with_full(xtask(&[
            "external-vector",
            &format!("--{mode}"),
            "--pairs",
            PAIRS,
            "--output",
            &format!("{out}/{mode}/external-vector"),
        ]))
    };
    let mut steps: Vec<Step> =
        collections("external-vector", vector("benchmark"), vector("memory")).into();
    steps.push(grade(
        "external-vector",
        "matroska-segment-metadata",
        "harness/features/matroska-segment-metadata/corpus.json",
        out,
        full,
    ));
    items.push(Item {
        name: "external-vector".into(),
        weight: 1,
        steps,
        preflight: owned(&["--external-vector"]),
    });

    items.push(Item {
        name: "isobmff".into(),
        weight: 78,
        steps: collections(
            "isobmff",
            with_full(xtask(&[
                "isobmff-bench",
                "--benchmark",
                "--pairs",
                PAIRS,
                "--output",
                &format!("{out}/benchmark/isobmff"),
            ])),
            with_full(xtask(&[
                "isobmff-bench",
                "--memory",
                "--memory-pairs",
                PAIRS,
                "--output",
                &format!("{out}/memory/isobmff"),
            ])),
        )
        .into(),
        preflight: owned(&["--isobmff"]),
    });

    let framing = |mode: &str| {
        with_full(xtask(&[
            "framing",
            &format!("--{mode}"),
            "--pairs",
            PAIRS,
            "--output",
            &format!("{out}/{mode}/framing"),
        ]))
    };
    let mut steps: Vec<Step> =
        collections("framing", framing("benchmark"), framing("memory")).into();
    steps.push(grade(
        "framing",
        "packet-framing",
        "harness/features/packet-framing/corpus.json",
        out,
        full,
    ));
    items.push(Item {
        name: "framing".into(),
        weight: 9,
        steps,
        preflight: owned(&[
            "--feature",
            "packet-framing",
            "--corpus",
            "packet-framing=.research/fixtures-packet-framing",
        ]),
    });

    let target_args = |mut args: Vec<String>| {
        if !release.starts_with("target/release") {
            args.extend(owned(&["--target", target]));
        }
        args
    };
    let bin = format!("{out}/codec-bin");
    let mut build_codecs = owned(&["sh", "-c"]);
    let cargo_target = target_args(Vec::new()).join(" ");
    build_codecs.push(format!(
        "mkdir -p {bin} && \
         cargo build --release --locked {cargo_target} -p triergon-codec-h264 --example decode_h264 && \
         cargo build --release --locked {cargo_target} -p triergon-codec-hevc --example decode_hevc && \
         cp {release}/examples/decode_h264 {bin}/codec-h264-decode && \
         cp {release}/examples/decode_hevc {bin}/codec-hevc-decode"
    ));
    let codec_worker = format!("{release}/codec-worker");
    items.push(Item {
        name: "codecs".into(),
        weight: 46,
        steps: vec![
            step("codec-bin", Stage::Timing, Placement::Free, build_codecs),
            step(
                "codecs-manifest",
                Stage::Timing,
                Placement::Free,
                xtask(&[
                    "codecs",
                    "manifest",
                    "--out",
                    &format!("{out}/codecs-fate-manifest.json"),
                    "h264=.research/fate-samples/h264-conformance",
                    "hevc=.research/fate-samples/hevc-conformance",
                ]),
            ),
            step(
                "codecs-grade",
                Stage::Timing,
                Placement::Free,
                xtask(&[
                    "codecs",
                    "grade",
                    "--manifest",
                    &format!("{out}/codecs-fate-manifest.json"),
                    "--root",
                    ".",
                    "--bin",
                    &bin,
                    "--ffmpeg",
                    &format!("{FFMPEG_BIN}/ffmpeg"),
                    "--ffprobe",
                    &format!("{FFMPEG_BIN}/ffprobe"),
                    "--codec-worker",
                    &codec_worker,
                    "--reference",
                    "target/harness/ffmpeg-decode-worker",
                    "--out",
                    &format!("{out}/codecs-grade.jsonl"),
                ]),
            ),
            step(
                "benchmark-codecs",
                Stage::Timing,
                Placement::Timed,
                with_full(xtask(&[
                    "codecs",
                    "time",
                    "--candidate",
                    &codec_worker,
                    "--reference",
                    "target/harness/ffmpeg-decode-worker",
                    "--workloads",
                    "harness/codecs/workloads.json",
                    "--root",
                    ".",
                    "--ledger",
                    &format!("{out}/codecs-ledger.jsonl"),
                    "--out",
                    &format!("{out}/benchmark/codecs"),
                    "--label",
                    target,
                ])),
            ),
        ],
        preflight: owned(&["--codecs", "harness/codecs/workloads.json"]),
    });

    // The capture cases in two halves, alternating, so that each holds sizes
    // and rates alike.
    for half in 0..2 {
        let name = format!("capture-{}", half + 1);
        let ids: Vec<&str> = capture
            .iter()
            .skip(half)
            .step_by(2)
            .map(String::as_str)
            .collect();
        let mut command = xtask(&[
            "capture",
            "qualify",
            "--candidate",
            &format!("{release}/capture-worker"),
            "--reference",
            "target/harness/ffmpeg-capture-worker",
            "--reference-small",
            "target/harness/ffmpeg-capture-small-worker",
            "--oracle-bin",
            FFMPEG_BIN,
            "--observer",
            "target/harness/allocations.so",
            "--inputs",
            &format!("{out}/capture-inputs"),
            "--out",
            &format!("{out}/{name}"),
            "--only",
        ]);
        command.extend(ids.iter().map(|id| (*id).to_owned()));
        let mut qualify = step(
            name.clone(),
            Stage::Timing,
            Placement::Timed,
            with_full(command),
        );
        qualify.environment.push(("TRIERGON_PYTHON", "python3"));
        items.push(Item {
            name,
            weight: 100,
            steps: vec![qualify],
            preflight: owned(&["--capture", CAPTURE_CASES]),
        });
    }

    let mut core_time = owned(&[
        "python3",
        "tools/test-core-time.py",
        "--toolchain",
        crate::NIGHTLY,
        "--output",
        &format!("{out}/core-time"),
    ]);
    if !release.starts_with("target/release") {
        core_time.extend(owned(&["--target", target]));
    }
    items.push(Item {
        name: "tests".into(),
        weight: 45,
        steps: vec![
            step(
                "test-debug",
                Stage::Timing,
                Placement::Free,
                with_full(xtask(&[
                    "gate",
                    "test",
                    "--profile",
                    "debug",
                    "--output",
                    &format!("{out}/tests/debug"),
                ])),
            ),
            step(
                "test-release",
                Stage::Timing,
                Placement::Free,
                with_full(xtask(&[
                    "gate",
                    "test",
                    "--profile",
                    "release",
                    "--output",
                    &format!("{out}/tests/release"),
                ])),
            ),
            step(
                "worker-tests",
                Stage::Timing,
                Placement::Free,
                xtask(&["worker-tests"]),
            ),
            step("core-time", Stage::Timing, Placement::Free, core_time),
        ],
        preflight: Vec::new(),
    });

    let compare = |name: &str, args: &[&str]| {
        let mut command = xtask(args);
        command.extend(owned(&["--output", &format!("{out}/parity/{name}")]));
        step(
            format!("{name}-compare"),
            Stage::Timing,
            Placement::Free,
            command,
        )
    };
    items.push(Item {
        name: "font-shape-raster-ass".into(),
        weight: 11,
        steps: vec![
            compare("fonts", &["fonts", "compare"]),
            compare("shape", &["shape", "compare"]),
            compare("raster", &["raster", "compare", "--all"]),
            compare("ass", &["ass", "compare"]),
        ],
        preflight: Vec::new(),
    });
    items
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

/// The capture case ids, in the order of `harness/capture/cases.json`.
fn capture_cases(root: &Path) -> R<Vec<String>> {
    let cases = crate::checks::read_json(&root.join(CAPTURE_CASES))?;
    Ok(cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{CAPTURE_CASES} without cases"))?
        .iter()
        .filter_map(|case| case.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect())
}

/// `I/N`, counted from 1.
fn parse_shard(value: &str) -> R<(usize, usize)> {
    let (index, count) = value
        .split_once('/')
        .and_then(|(i, n)| Some((i.parse::<usize>().ok()?, n.parse::<usize>().ok()?)))
        .filter(|&(i, n)| (1..=n).contains(&i))
        .ok_or_else(|| format!("--shard {value}: expected I/N with 1 <= I <= N"))?;
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
    profile: Option<String>,
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
            profile: None,
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
                "--shard" => options.shard = parse_shard(&value()?)?,
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
                "--profile" => options.profile = Some(value()?),
                flag if flag.starts_with("--") => return Err(USAGE.into()),
                directory => options.directories.push(PathBuf::from(directory)),
            }
        }
        Ok(options)
    }
}

/// The items of shard `index` of `count`, writing under `out`.
fn shard_items(root: &Path, out: &str, full: bool, (index, count): (usize, usize)) -> R<Vec<Item>> {
    let target = crate::harness::report::gate_target()?;
    let release = crate::harness::gate_build::release_dir()?;
    let all = items(out, full, &target, &release, &capture_cases(root)?);
    Ok(shards(all, count).swap_remove(index))
}

pub fn main(root: &Path, args: &[String]) -> R<i32> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    let options = Options::parse(rest)?;
    match command.as_str() {
        "plan" => {
            let all = items(
                "OUT",
                false,
                "TARGET",
                "target/release",
                &capture_cases(root)?,
            );
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
            Ok(0)
        }
        "preflight" => {
            let review = options.review.ok_or("gate preflight needs --review FILE")?;
            let mut args = vec!["preflight".to_owned()];
            for item in shard_items(root, "OUT", false, options.shard)? {
                args.extend(item.preflight);
            }
            args.extend(["--review".to_owned(), review]);
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
            run(root, &output, stage, options.shard, options.full)
        }
        "summary" => {
            let output = options.output.ok_or("gate summary needs --output FILE")?;
            summary(&output, &options.directories)
        }
        "test" => {
            let output = options.output.ok_or("gate test needs --output DIR")?;
            let release = match options.profile.as_deref() {
                Some("debug") => false,
                Some("release") => true,
                _ => return Err("gate test needs --profile debug|release".into()),
            };
            test(root, &output, release, options.full)
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
fn run(root: &Path, output: &Path, stage: Stage, shard: (usize, usize), full: bool) -> R<i32> {
    fs::create_dir_all(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let output = output
        .canonicalize()
        .map_err(|e| format!("{}: {e}", output.display()))?;
    let out = output.display().to_string();
    let items = shard_items(root, &out, full, shard)?;
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
            (
                "items",
                Value::strings(items.iter().map(|item| item.name.clone())),
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
    // Parity before timing, memory before grades, item by item.
    for placement in [Placement::Free, Placement::Timed, Placement::Memory] {
        for (item, step) in steps.iter().filter(|(_, s)| s.placement == placement) {
            let cpus = match placement {
                Placement::Timed => Some("1"),
                Placement::Memory => Some(memory.as_str()),
                Placement::Free | Placement::Grade => None,
            };
            let mut command = command(root, &exe, step, cpus);
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
    let width = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    for batch in grades.chunks(width.max(1)) {
        let results: Vec<R<i32>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|(item, step)| {
                    let (output, log, exe) = (&output, &log, &exe);
                    scope.spawn(move || {
                        let mut command = command(root, exe, step, None);
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

fn command(root: &Path, exe: &Path, step: &Step, cpus: Option<&str>) -> Command {
    let program: Vec<String> = step
        .command
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
        .envs(step.environment.iter().copied())
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
    let logs = output.join("logs");
    fs::create_dir_all(&logs).map_err(|e| format!("{}: {e}", logs.display()))?;
    let file =
        fs::File::create(logs.join(format!("{}.log", step.name))).map_err(|e| e.to_string())?;
    let errors = file.try_clone().map_err(|e| e.to_string())?;
    append(log, &step.name)?;
    let started = crate::python::utc_now();
    let clock = Instant::now();
    let code = command
        .stdout(file)
        .stderr(errors)
        .status()
        .map_or(127, |s| s.code().unwrap_or(1));
    let seconds = clock.elapsed().as_secs_f64();
    append(log, &format!("{} EXIT={code}", step.name))?;
    let mut shown = step.command.clone();
    if let Some(cpus) = cpus {
        shown.splice(0..0, owned(&["taskset", "-c", cpus]));
    }
    crate::corpus::write_json(
        &output.join("steps").join(format!("{}.json", step.name)),
        &Value::object([
            ("step", Value::str(&step.name)),
            ("item", Value::str(&item.name)),
            ("stage", Value::str(step.stage.name())),
            ("command", Value::strings(shown)),
            (
                "environment",
                Value::object(step.environment.iter().map(|(k, v)| (*k, Value::str(*v)))),
            ),
            ("started", Value::str(started)),
            ("seconds", Value::float((seconds * 10.0).round() / 10.0)),
            ("status", Value::int(i64::from(code))),
        ]),
    )?;
    Ok(code)
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

/// The test inputs outside the repository that crate tests read when present.
const TEST_INPUTS: [&str; 3] = [
    ".research/fate-samples",
    ".research/flac-generated",
    ".research/flac-test-files",
];

/// Runs the workspace tests in one profile, or takes the result of a kept set
/// for the same key: every tracked file as the working tree holds it, untracked
/// files that are not ignored, the test inputs of [`TEST_INPUTS`], and what every
/// gate key holds (the measuring code, toolchains, target and host class).
fn test(root: &Path, output: &Path, release: bool, full: bool) -> R<i32> {
    let mut command = owned(&["test", "--workspace", "--locked"]);
    if release {
        command.push("--release".into());
    }
    command.extend(crate::harness::gate_build::target_args()?);
    let inputs = TEST_INPUTS
        .iter()
        .map(|dir| Ok(((*dir).to_owned(), Value::str(tree_digest(&root.join(dir))?))))
        .collect::<R<Vec<_>>>()?;
    let settings = Value::object([
        ("cargo", Value::strings(command.clone())),
        ("sources", Value::str(sources_digest(root)?)),
        ("inputs", Value::Object(inputs)),
    ]);
    let key = crate::harness::reuse::Key::new(root, "gate test", settings, &[], &[])?;
    crate::harness::reuse::step(root, output, &key, full, || {
        status(
            Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(&command)
                .env("CARGO_INCREMENTAL", "0")
                .current_dir(root),
        )
    })
}

/// The SHA-256 of every tracked file as the working tree holds it, and of the
/// untracked files git does not ignore.
fn sources_digest(root: &Path) -> R<String> {
    let mut digest = sha256::Sha256::default();
    digest.update(git(root, &["ls-files", "-s"])?.as_bytes());
    let changed = git(root, &["diff", "HEAD", "--name-only", "-z"])?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in changed
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|p| !p.is_empty())
    {
        let file = root.join(path);
        let sha = if file.is_file() {
            sha256::file_hex(&file).map_err(|e| format!("{}: {e}", file.display()))?
        } else {
            "absent".to_owned()
        };
        digest.update(format!("{sha}  {path}\n").as_bytes());
    }
    Ok(digest.finish_hex())
}

/// The SHA-256 of every file under `dir` by relative path, or `absent`.
fn tree_digest(dir: &Path) -> R<String> {
    if !dir.is_dir() {
        return Ok("absent".into());
    }
    let mut digest = sha256::Sha256::default();
    for (relative, path) in crate::evidence::files(dir)? {
        let sha = sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        digest.update(format!("{sha}  {relative}\n").as_bytes());
    }
    Ok(digest.finish_hex())
}

/// Joins the shards' outputs into one result at `file`; 0 when every planned
/// step ran once and passed on one source and one set of workers.
fn summary(file: &Path, directories: &[PathBuf]) -> R<i32> {
    let mut problems = Vec::new();
    let mut shards = Vec::new();
    let mut steps = Vec::new();
    let mut identities: Vec<(String, String)> = Vec::new();
    for directory in directories {
        let shard = crate::checks::read_json(&directory.join(SHARD))
            .map_err(|e| format!("{}: {e}", directory.join(SHARD).display()))?;
        let mut records = Vec::new();
        if let Ok(entries) = fs::read_dir(directory.join("steps")) {
            for entry in entries.flatten() {
                records.push(crate::checks::read_json(&entry.path())?);
            }
        }
        records.sort_by(|a, b| {
            let started = |v: &Value| {
                v.get("started")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            started(a).cmp(&started(b))
        });
        let seconds: f64 = records
            .iter()
            .filter_map(|r| r.get("seconds").and_then(Value::as_f64))
            .sum();
        for stage in ["timing", "memory"] {
            let path = directory.join(format!("identity-{stage}.json"));
            match sha256::file_hex(&path) {
                Ok(sha) => identities.push((stage.to_owned(), sha)),
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        }
        shards.push(Value::object([
            ("directory", Value::str(directory.display().to_string())),
            ("shard", shard.clone()),
            ("step_seconds", Value::float(seconds.round())),
        ]));
        steps.extend(records);
    }
    let field = |shard: &Value, key: &str| shard.get("shard").and_then(|s| s.get(key)).cloned();
    let count = shards
        .first()
        .and_then(|s| field(s, "count"))
        .and_then(|c| c.as_f64());
    let mut indices: Vec<i64> = shards
        .iter()
        .filter_map(|s| field(s, "index").and_then(|i| i.as_f64()).map(|i| i as i64))
        .collect();
    indices.sort_unstable();
    let expected: Vec<i64> = (1..=count.unwrap_or(0.0) as i64).collect();
    if indices != expected {
        problems.push(format!("shards {indices:?} of {count:?}: each shard once"));
    }
    for key in ["count", "revision", "changes", "target"] {
        let values: Vec<Option<Value>> = shards.iter().map(|s| field(s, key)).collect();
        if values.windows(2).any(|w| w[0] != w[1]) {
            problems.push(format!("the shards differ in {key}"));
        }
    }
    // The timing builds must be one build on every shard; the memory builds add
    // each host's proof pin.
    let timing: Vec<&String> = identities
        .iter()
        .filter(|(stage, _)| stage == "timing")
        .map(|(_, sha)| sha)
        .collect();
    if timing.windows(2).any(|w| w[0] != w[1]) {
        problems.push("the shards' timing builds differ".into());
    }
    // Every planned step, once.
    let ran_items: Vec<String> = shards
        .iter()
        .flat_map(|s| {
            field(s, "items")
                .and_then(|i| i.as_array().map(<[Value]>::to_vec))
                .unwrap_or_default()
        })
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let planned = items("OUT", false, "TARGET", "target/release", &[]);
    for item in &planned {
        let times = ran_items.iter().filter(|name| **name == item.name).count();
        if times != 1 {
            problems.push(format!("{} planned once, run {times} times", item.name));
        }
        for step in &item.steps {
            let ran = steps
                .iter()
                .filter(|r| r.get("step").and_then(Value::as_str) == Some(&step.name))
                .count();
            if ran != 1 {
                problems.push(format!("{}: {ran} records", step.name));
            }
        }
    }
    let failed: Vec<String> = steps
        .iter()
        .filter(|r| r.get("status").and_then(Value::as_f64) != Some(0.0))
        .filter_map(|r| r.get("step").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for record in &steps {
        println!(
            "{:<44} {:>8.0} s  exit {}",
            record
                .get("step")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            record
                .get("seconds")
                .and_then(Value::as_f64)
                .unwrap_or_default(),
            record.get("status").and_then(Value::as_f64).unwrap_or(-1.0)
        );
    }
    for problem in &problems {
        println!("FAIL {problem}");
    }
    println!(
        "{} steps on {} shards, {} failed",
        steps.len(),
        shards.len(),
        failed.len()
    );
    crate::corpus::write_json(
        file,
        &Value::object([
            ("schema", Value::str("gate-summary-v1")),
            ("shards", Value::Array(shards)),
            ("steps", Value::Array(steps)),
            ("failed", Value::strings(failed.clone())),
            ("problems", Value::strings(problems.clone())),
        ]),
    )?;
    Ok(i32::from(!failed.is_empty() || !problems.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Vec<Item> {
        let capture: Vec<String> = (1..=15).map(|i| format!("c{i:02}")).collect();
        items(
            "/o",
            false,
            "thumbv7neon-unknown-linux-gnueabihf",
            "target/thumbv7neon-unknown-linux-gnueabihf/release",
            &capture,
        )
    }

    #[test]
    fn every_step_is_named_once_and_every_collection_is_graded_or_suite_run() {
        let plan = plan();
        let names: Vec<&str> = plan
            .iter()
            .flat_map(|i| &i.steps)
            .map(|s| s.name.as_str())
            .collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        for item in &plan {
            let timed = item.steps.iter().any(|s| s.placement == Placement::Timed);
            let memory = item.steps.iter().any(|s| s.placement == Placement::Memory);
            assert!(!memory || timed, "{}", item.name);
            for step in &item.steps {
                assert_eq!(
                    step.placement == Placement::Memory,
                    step.stage == Stage::Memory && step.placement != Placement::Grade
                );
                if step.placement == Placement::Memory {
                    assert_eq!(step.environment, [("TRIERGON_RSS_KERNEL_BOUND", "1")]);
                }
            }
        }
        assert!(names.contains(&"grade-subtitle-cc"));
        assert!(names.contains(&"benchmark-isobmff"));
    }

    #[test]
    fn capture_cases_split_into_two_halves_that_cover_them_once() {
        let plan = plan();
        let ids: Vec<String> = plan
            .iter()
            .filter(|i| i.name.starts_with("capture-"))
            .flat_map(|i| {
                let command = &i.steps[0].command;
                let at = command.iter().position(|a| a == "--only").unwrap();
                command[at + 1..].to_vec()
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            (1..=15).map(|i| format!("c{i:02}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn shards_are_balanced_deterministically_and_hold_every_item() {
        let total: u32 = plan().iter().map(|i| i.weight).sum();
        for count in 1..=6 {
            let split = shards(plan(), count);
            assert_eq!(split.len(), count);
            let weights: Vec<u32> = split
                .iter()
                .map(|s| s.iter().map(|i| i.weight).sum())
                .collect();
            assert_eq!(weights.iter().sum::<u32>(), total);
            let heaviest = plan().iter().map(|i| i.weight).max().unwrap();
            assert!(weights.iter().max().unwrap() - weights.iter().min().unwrap() <= heaviest);
            let again: Vec<Vec<String>> = shards(plan(), count)
                .iter()
                .map(|s| s.iter().map(|i| i.name.clone()).collect())
                .collect();
            let names: Vec<Vec<String>> = split
                .iter()
                .map(|s| s.iter().map(|i| i.name.clone()).collect())
                .collect();
            assert_eq!(again, names);
        }
    }

    #[test]
    fn a_test_result_is_keyed_by_every_source_the_working_tree_holds() {
        let root = crate::walk::scratch("gate-test-sources");
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(args)
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        fs::write(root.join("lib.rs"), b"a").unwrap();
        git(&["add", "lib.rs"]);
        git(&["commit", "-qm", "a"]);
        let committed = sources_digest(&root).unwrap();
        fs::write(root.join("lib.rs"), b"b").unwrap();
        let changed = sources_digest(&root).unwrap();
        assert_ne!(committed, changed);
        fs::write(root.join("new.rs"), b"c").unwrap();
        assert_ne!(sources_digest(&root).unwrap(), changed);
        fs::remove_file(root.join("new.rs")).unwrap();
        fs::write(root.join("lib.rs"), b"a").unwrap();
        assert_eq!(sources_digest(&root).unwrap(), committed);
        assert_eq!(tree_digest(&root.join("absent")).unwrap(), "absent");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shard_arguments_count_from_one() {
        assert_eq!(parse_shard("1/4").unwrap(), (0, 4));
        assert_eq!(parse_shard("4/4").unwrap(), (3, 4));
        assert!(parse_shard("0/4").is_err());
        assert!(parse_shard("5/4").is_err());
    }
}
