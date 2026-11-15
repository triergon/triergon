//! The training of the workers' link order: a corpus of its own, kept apart from
//! everything the gate reads, the operations the workers run on it, and the commands
//! that check the separation, record the trained orders and check the committed
//! ones.
//!
//! [`MANIFEST`] lists the training files by SHA-256 and the operations: a worker and
//! its command line, where `@NAME` stands for the training file `NAME`. The workers
//! with operations are the ordered ones; every other worker links as its toolchain
//! does. `train` runs each operation of each ordered worker under QEMU's user-mode
//! emulator, which logs every block of code when it first runs it, and writes the
//! worker's trained order ([`super::link_order::first_use`]) to
//! `harness/link-order/TARGET/WORKER.txt`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::build::{WorkerBuild, workers};
use super::gate_build;
use super::link_order;
use crate::json::Value;
use crate::sha256;

type R<T> = Result<T, String>;

/// The training manifest.
pub const MANIFEST: &str = "harness/link-order/training.json";

/// One training operation: a worker's command line on training files.
struct Operation {
    worker: String,
    arguments: Vec<String>,
    /// The targets it runs on; every target when empty.
    targets: Vec<String>,
}

impl Operation {
    /// The training files the command line names.
    fn files(&self) -> impl Iterator<Item = &str> {
        self.arguments
            .iter()
            .filter_map(|argument| argument.strip_prefix('@'))
    }

    /// The command line, run in the training directory.
    fn command_line(&self) -> Vec<&str> {
        self.arguments
            .iter()
            .map(|argument| argument.strip_prefix('@').unwrap_or(argument))
            .collect()
    }

    fn runs_on(&self, target: &str) -> bool {
        self.targets.is_empty() || self.targets.iter().any(|own| own == target)
    }

    /// The mode word: the first argument that is not an option.
    fn mode(&self) -> &str {
        self.arguments
            .iter()
            .find(|argument| !argument.starts_with("--"))
            .map_or("", String::as_str)
    }

    /// The operation family: the mode word without its mode, as a gate contract
    /// names it (`mp4-range-` for `mp4-range-memory`), or the mode word itself.
    fn family(&self) -> &str {
        let word = self.mode();
        ["fold", "bench", "memory", "trace"]
            .iter()
            .find_map(|mode| word.strip_suffix(mode))
            .unwrap_or(word)
    }

    /// Its arguments but the mode word and the training files, sorted.
    fn others(&self) -> Vec<&str> {
        let mode = self.mode();
        let mut others: Vec<&str> = self
            .arguments
            .iter()
            .map(String::as_str)
            .filter(|argument| *argument != mode && !argument.starts_with('@'))
            .collect();
        others.sort_unstable();
        others
    }
}

/// The training manifest: the directory the training files are in, relative to the
/// repository root, the files with their SHA-256 and the operations, in order.
struct Training {
    directory: PathBuf,
    files: BTreeMap<String, String>,
    operations: Vec<Operation>,
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

impl Training {
    /// Whether the manifest has operations for `worker`, which then links in a
    /// trained order.
    fn orders(&self, worker: &str) -> bool {
        self.operations
            .iter()
            .any(|operation| operation.worker == worker)
    }

    fn load(root: &Path) -> R<Self> {
        let path = root.join(MANIFEST);
        let document = crate::checks::read_json(&path)?;
        let fail = |problem: String| format!("{}: {problem}", path.display());
        let directory = document
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("no directory".into()))?;
        let mut files = BTreeMap::new();
        for file in document
            .get("files")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let (Some(name), Some(hash)) = (
                file.get("file").and_then(Value::as_str),
                file.get("sha256").and_then(Value::as_str),
            ) else {
                return Err(fail("a file without its name or SHA-256".into()));
            };
            if name.is_empty() || name.contains('/') || name.starts_with('.') {
                return Err(fail(format!("{name}: not a plain file name")));
            }
            if files.insert(name.to_owned(), hash.to_owned()).is_some() {
                return Err(fail(format!("{name}: listed twice")));
            }
        }
        let known: BTreeSet<&str> = workers().iter().map(|worker| worker.name).collect();
        let mut operations = Vec::new();
        for operation in document
            .get("operations")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let operation = Operation {
                worker: operation
                    .get("worker")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: strings(operation.get("arguments")),
                targets: strings(operation.get("targets")),
            };
            if !known.contains(operation.worker.as_str()) {
                return Err(fail(format!("{}: not a worker", operation.worker)));
            }
            if operation.arguments.is_empty() {
                return Err(fail(format!(
                    "{}: an operation without arguments",
                    operation.worker
                )));
            }
            if let Some(file) = operation.files().find(|file| !files.contains_key(*file)) {
                return Err(fail(format!("{file}: not a training file")));
            }
            if let Some(target) = operation
                .targets
                .iter()
                .find(|target| !crate::gate::TARGETS.contains(&target.as_str()))
            {
                return Err(fail(format!("{target}: not a gate target")));
            }
            operations.push(operation);
        }
        if let Some(unused) = files.keys().find(|file| {
            !operations
                .iter()
                .any(|operation| operation.files().any(|own| own == file.as_str()))
        }) {
            return Err(fail(format!("{unused}: no operation reads it")));
        }
        Ok(Self {
            directory: root.join(directory),
            files,
            operations,
        })
    }
}

/// Checks that the training corpus in `directory` (the manifest's when none) is
/// apart from the gate: each training file has its listed SHA-256, none has the
/// SHA-256 of a file a gate item reads or of a committed file
/// ([`crate::gate::input_hashes`]), and no operation copies a gate case: none runs a
/// case's operation family on inputs of the same content with the same other
/// arguments. Needs a prepared host, where every gate input is present.
pub fn separate(root: &Path, directory: Option<&Path>) -> R<()> {
    let training = Training::load(root)?;
    let directory = directory.unwrap_or(&training.directory);
    for (name, pin) in &training.files {
        let path = directory.join(name);
        let hash = sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if hash != *pin {
            return Err(format!(
                "{}: SHA-256 {hash}, listed as {pin}",
                path.display()
            ));
        }
    }
    let gate = crate::gate::input_hashes(root)?;
    let mut problems: Vec<String> = training
        .files
        .iter()
        .filter_map(|(name, hash)| {
            gate.get(hash)
                .map(|path| format!("{name}: the same file as the gate's {path}"))
        })
        .collect();
    let cases = gate_cases(root)?;
    for operation in &training.operations {
        let mut inputs: Vec<&str> = operation
            .files()
            .map(|file| training.files[file].as_str())
            .collect();
        inputs.sort_unstable();
        let others = operation.others();
        if let Some(case) = cases.iter().find(|case| {
            case.family == operation.family() && case.inputs == inputs && case.others == others
        }) {
            problems.push(format!(
                "{} {}: copies gate case {}",
                operation.worker,
                operation.arguments.join(" "),
                case.id
            ));
        }
    }
    if problems.is_empty() {
        println!(
            "{} training files and {} operations apart from {} gate inputs and {} gate cases",
            training.files.len(),
            training.operations.len(),
            gate.len(),
            cases.len()
        );
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// A gate case run through a worker operation prefix, as [`separate`] compares it.
struct GateCase {
    id: String,
    family: String,
    /// The SHA-256 of its input, entities and companions, sorted.
    inputs: Vec<String>,
    /// Its arguments that name none of those files, sorted.
    others: Vec<String>,
}

fn gate_cases(root: &Path) -> R<Vec<GateCase>> {
    let mut cases = Vec::new();
    for contract in super::features::discover(root)? {
        let Some(family) = contract
            .document
            .get("worker_operation")
            .and_then(Value::as_str)
        else {
            continue;
        };
        let (manifest, _) = super::manifest::read(root, &contract.manifest())?;
        for case in manifest
            .get("cases")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let mut inputs = Vec::new();
            let mut names = BTreeSet::new();
            if let Some(hash) = case.get("sha256").and_then(Value::as_str) {
                inputs.push(hash.to_owned());
            }
            for key in ["entities", "companions"] {
                for file in case.get(key).and_then(Value::as_array).unwrap_or_default() {
                    if let Some(hash) = file.get("sha256").and_then(Value::as_str) {
                        inputs.push(hash.to_owned());
                    }
                    if let Some(name) = file.get("file").and_then(Value::as_str) {
                        names.insert(name.to_owned());
                    }
                }
            }
            inputs.sort_unstable();
            let mut others: Vec<String> = strings(case.get("arguments"))
                .into_iter()
                .filter(|argument| !names.contains(argument))
                .collect();
            others.sort_unstable();
            cases.push(GateCase {
                id: format!(
                    "{}/{}",
                    contract.id,
                    case.get("id").and_then(Value::as_str).unwrap_or_default()
                ),
                family: family.to_owned(),
                inputs,
                others,
            });
        }
    }
    Ok(cases)
}

/// The user-mode emulator for a worker target and the CPU it emulates: the gate
/// host's, so code that depends on CPU features takes the same path.
fn emulator(target: &str) -> R<(&'static str, &'static str)> {
    if target == gate_build::ARM32_TARGET {
        Ok(("qemu-arm", "cortex-a15"))
    } else if target.starts_with("aarch64-") {
        Ok(("qemu-aarch64", "neoverse-n1"))
    } else {
        Err(format!("{target}: no trained link order"))
    }
}

/// The first lines of a trained order.
fn header(worker: &str, target: &str) -> String {
    format!(
        "# The functions the link-order training runs in {worker} on {target}, in first-use order.\n\
         # Written by `cargo xtask link-order train`; see docs/quality.md.\n"
    )
}

/// Runs the training operations of each built ordered worker (only `only` when
/// given) under the emulator and writes its trained order for the worker target.
/// The corpus is checked with [`separate`] first.
pub fn train(root: &Path, directory: Option<&Path>, only: Option<&str>) -> R<()> {
    separate(root, directory)?;
    let training = Training::load(root)?;
    let directory = directory.unwrap_or(&training.directory).to_path_buf();
    let target = gate_build::worker_target()?;
    let (emulator, cpu) = emulator(target)?;
    let release = root.join(gate_build::release_dir()?);
    let logs = root.join("target/harness/link-order/training");
    for worker in workers() {
        if only.is_some_and(|only| only != worker.name) || !training.orders(worker.name) {
            continue;
        }
        let binary = release.join(worker.name);
        let directory_logs = logs.join(worker.name);
        if directory_logs.exists() {
            fs::remove_dir_all(&directory_logs)
                .map_err(|e| format!("{}: {e}", directory_logs.display()))?;
        }
        fs::create_dir_all(&directory_logs)
            .map_err(|e| format!("{}: {e}", directory_logs.display()))?;
        let mut runs = Vec::new();
        for operation in training
            .operations
            .iter()
            .filter(|operation| operation.worker == worker.name && operation.runs_on(target))
        {
            let log = directory_logs.join(format!("{}.log", runs.len()));
            let status = Command::new(emulator)
                .args(["-cpu", cpu, "-d", "page,in_asm", "-D"])
                .arg(&log)
                .arg(&binary)
                .args(operation.command_line())
                .current_dir(&directory)
                .stdout(Stdio::null())
                .status()
                .map_err(|e| format!("{emulator}: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "{} {}: {status}",
                    worker.name,
                    operation.arguments.join(" ")
                ));
            }
            runs.push(log);
        }
        let names = link_order::first_use(&binary, &runs)?;
        let path = link_order::trained_path(root, target, worker.name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut text = header(worker.name, target);
        for name in &names {
            text.push_str(name);
            text.push('\n');
        }
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        println!(
            "{}: {} functions from {} operations",
            path.strip_prefix(root).unwrap_or(&path).display(),
            names.len(),
            runs.len()
        );
    }
    Ok(())
}

/// Checks the training manifest and the committed trained orders: every ordered
/// worker has one for each gate target and no other worker has one, each parses,
/// and, where the policy orders the link, each links: the worker is built for this
/// host, linked with the ordering file the trained order gives that build, and
/// linked again to the same ordering file.
pub fn check(root: &Path) -> R<()> {
    let training = Training::load(root)?;
    let mut trained = Vec::new();
    for worker in workers() {
        for target in crate::gate::TARGETS {
            let path = link_order::trained_path(root, target, worker.name);
            match (
                link_order::committed_trained(root, target, worker.name)?,
                training.orders(worker.name),
            ) {
                (Some(names), true) => trained.push((worker.name, target, names)),
                (None, false) => {}
                (Some(_), false) => {
                    return Err(format!(
                        "{}: {} has no training operations",
                        path.display(),
                        worker.name
                    ));
                }
                (None, true) => return Err(format!("{}: missing", path.display())),
            }
        }
    }
    if gate_build::ordered_link_arguments(root, None)?.is_empty() {
        println!(
            "link-order check: {} trained orders parse; this host links without them",
            trained.len()
        );
        return Ok(());
    }
    for worker in workers() {
        if !training.orders(worker.name) {
            continue;
        }
        let binary = link(root, &worker, None)?;
        for (_, target, names) in trained.iter().filter(|(name, ..)| *name == worker.name) {
            let order = link_order::symbol_order(&binary, names)?;
            let path = root
                .join("target/link-order-check")
                .join(format!("{}-{target}.txt", worker.name));
            fs::write(&path, &order).map_err(|e| format!("{}: {e}", path.display()))?;
            link(root, &worker, Some(&path))?;
            if link_order::symbol_order(&binary, names)? != order {
                return Err(format!(
                    "{} with the {target} order: the ordered link holds other functions",
                    worker.name
                ));
            }
        }
    }
    println!(
        "link-order check: {} trained orders parse and link",
        trained.len()
    );
    Ok(())
}

/// Builds `worker` for this host in the dev profile, linked by the toolchain's `lld`
/// in `order`, and returns the binary.
fn link(root: &Path, worker: &WorkerBuild, order: Option<&Path>) -> R<PathBuf> {
    let target_dir = root.join("target/link-order-check");
    let mut command = Command::new("cargo");
    command
        .args(["rustc", "-p", worker.package, "--locked", "--target-dir"])
        .arg(&target_dir);
    match worker.example {
        Some(example) => command.args(["--example", example]),
        None => command.args(["--bin", worker.name]),
    };
    if !worker.features.is_empty() {
        command.args(["--features", &worker.features.join(",")]);
    }
    command
        .arg("--")
        .args(gate_build::worker_link_arguments()?)
        .args(gate_build::ordered_link_arguments(root, order)?);
    let status = command
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if !status.success() {
        return Err(format!("{}: link failed: {status}", worker.name));
    }
    let debug = target_dir.join("debug");
    Ok(match worker.example {
        Some(example) => debug.join("examples").join(example),
        None => debug.join(worker.name),
    })
}

/// The `ffmpeg` options before the inputs: quiet, overwriting.
const FFMPEG: [&str; 5] = ["-hide_banner", "-nostdin", "-y", "-v", "error"];

/// The output options of every training file: bit-exact, without metadata.
const BITEXACT: [&str; 8] = [
    "-map_metadata",
    "-1",
    "-fflags",
    "+bitexact",
    "-flags:v",
    "+bitexact",
    "-flags:a",
    "+bitexact",
];

/// Video and audio sources, from FFmpeg's own generators.
const MANDELBROT: &str = "mandelbrot=size=320x180:rate=25:maxiter=256";
const LIFE: &str = "life=size=256x144:rate=30:mold=10:ratio=0.1:seed=7";
const CELLS: &str = "cellauto=size=320x176:rate=24:seed=11:rule=110";
const CHORD_48K: &str = "aevalsrc=0.3*sin(2*PI*523*t)|0.3*sin(2*PI*659*t):s=48000";
const CHORD_44K: &str = "aevalsrc=0.3*sin(2*PI*392*t)|0.2*sin(2*PI*587*t):s=44100";

/// The video encoders, on one thread so their output does not depend on the host,
/// with closed GOPs.
const AVC: [&str; 12] = [
    "-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-bf", "2", "-pix_fmt", "yuv420p",
    "-threads", "1",
];
const HEVC: [&str; 8] = [
    "-c:v",
    "libx265",
    "-preset",
    "ultrafast",
    "-x265-params",
    "pools=none:frame-threads=1:log-level=error:keyint=30:open-gop=0",
    "-pix_fmt",
    "yuv420p",
];

/// A subtitle script written for the training corpus.
const CUES: &str = "1\n00:00:00,400 --> 00:00:01,600\nA training cue\n\n\
                    2\n00:00:02,100 --> 00:00:03,900\nAnother line\nwith two rows\n\n\
                    3\n00:00:04,250 --> 00:00:05,500\nThe last cue\n";

fn with<'a>(parts: &[&[&'a str]]) -> Vec<&'a str> {
    parts.concat()
}

/// One `ffmpeg` run: `inputs` are lavfi graphs (or files in `out`), with their
/// durations, `options` the rest of the command line before the output file.
fn ffmpeg(out: &Path, inputs: &[(&str, &str)], options: &[&str], file: &str) -> R<()> {
    let mut command = Command::new("ffmpeg");
    command.args(FFMPEG);
    for (graph, seconds) in inputs {
        if graph.contains('=') {
            command.args(["-f", "lavfi", "-t", seconds, "-i", graph]);
        } else {
            command.arg("-i").arg(out.join(graph));
        }
    }
    let status = command
        .args(BITEXACT)
        .args(options)
        .arg(out.join(file))
        .status()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg {file}: {status}"))
    }
}

fn probe(path: &Path, arguments: &[&str]) -> R<Value> {
    let mut argv: Vec<String> = std::iter::once("ffprobe")
        .chain(arguments.iter().copied())
        .map(str::to_owned)
        .collect();
    argv.push(path.display().to_string());
    let output = crate::process::check_output(&argv)?;
    crate::json::parse(&output).map_err(|e| format!("ffprobe {}: {e}", path.display()))
}

fn integer(value: &Value, key: &str) -> R<i128> {
    value
        .get(key)
        .and_then(crate::python::int)
        .ok_or_else(|| format!("ffprobe: no {key}"))
}

/// A packet-framing script (`SMBSF001`) of the first `count` packets of the first
/// video stream of `source`, then a drain.
fn framing_script(source: &Path, count: usize) -> R<Vec<u8>> {
    let data = fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let probe = probe(
        source,
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,extradata:packet=pts,dts,duration,pos,size,flags",
            "-show_data",
            "-of",
            "json",
        ],
    )?;
    let Some([stream]) = probe.get("streams").and_then(Value::as_array) else {
        return Err(format!("{}: no video stream", source.display()));
    };
    let codec = match stream.get("codec_name").and_then(Value::as_str) {
        Some("h264") => 1u8,
        Some("hevc") => 2,
        other => return Err(format!("{}: video codec {other:?}", source.display())),
    };
    let configuration = crate::codecs::aac_access_units::dump_bytes(
        stream
            .get("extradata")
            .and_then(Value::as_str)
            .ok_or("ffprobe: no extradata")?,
    )?;
    let packets = probe
        .get("packets")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let packets = &packets[..packets.len().min(count)];
    let mut script = b"SMBSF001".to_vec();
    script.push(codec);
    script.extend_from_slice(
        &u32::try_from(configuration.len())
            .map_err(|e| e.to_string())?
            .to_be_bytes(),
    );
    script.extend_from_slice(&configuration);
    script.extend_from_slice(
        &u32::try_from(packets.len() + 1)
            .map_err(|e| e.to_string())?
            .to_be_bytes(),
    );
    for packet in packets {
        let position = integer(packet, "pos")?;
        let size = integer(packet, "size")?;
        let bytes = usize::try_from(position)
            .ok()
            .zip(usize::try_from(size).ok())
            .and_then(|(start, size)| data.get(start..start.checked_add(size)?))
            .ok_or_else(|| format!("{}: packet outside the file", source.display()))?;
        script.push(0);
        for key in ["pts", "dts", "duration"] {
            script.extend_from_slice(&(integer(packet, key)? as i64).to_be_bytes());
        }
        script.extend_from_slice(&(position as i64).to_be_bytes());
        let key = packet
            .get("flags")
            .and_then(Value::as_str)
            .is_some_and(|flags| flags.starts_with('K'));
        script.extend_from_slice(&u32::from(key).to_be_bytes());
        script.extend_from_slice(&0i32.to_be_bytes());
        script.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        script.extend_from_slice(&u32::MAX.to_be_bytes());
        script.extend_from_slice(bytes);
    }
    script.push(2);
    Ok(script)
}

/// Generates the training corpus into `out` with `ffmpeg` and `ffprobe` from `PATH`:
/// media from FFmpeg's own video and audio generators, encoded with libx264,
/// libx265 and FFmpeg's AAC, AC-3, E-AC-3 and Opus encoders; the playback capsules,
/// AAC access units and packet-framing scripts the workers read are made from those
/// files. Prints each file's SHA-256 as the manifest lists it.
pub fn corpus(out: &Path) -> R<()> {
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    fs::write(out.join("cues.srt"), CUES).map_err(|e| format!("cues.srt: {e}"))?;
    ffmpeg(
        out,
        &[(MANDELBROT, "6"), (CHORD_48K, "6"), ("cues.srt", "")],
        &with(&[&AVC, &["-c:a", "aac", "-b:a", "96k", "-c:s", "srt"]]),
        "avc-aac-text.mkv",
    )?;
    ffmpeg(
        out,
        &[(LIFE, "5"), (CHORD_48K, "5")],
        &with(&[&HEVC, &["-c:a", "libopus", "-b:a", "64k"]]),
        "hevc-opus.mkv",
    )?;
    ffmpeg(
        out,
        &[(CELLS, "4"), (CHORD_48K, "4")],
        &with(&[
            &AVC,
            &[
                "-map", "0:v", "-map", "1:a", "-map", "1:a", "-map", "1:a", "-c:a:0", "aac",
                "-c:a:1", "ac3", "-c:a:2", "eac3",
            ],
        ]),
        "avc-three-audio.mkv",
    )?;
    ffmpeg(
        out,
        &[(CELLS, "6"), (CHORD_48K, "6")],
        &with(&[&AVC, &["-c:a", "aac", "-b:a", "96k"]]),
        "avc-aac.mp4",
    )?;
    ffmpeg(
        out,
        &[(MANDELBROT, "5"), (CHORD_44K, "5")],
        &with(&[&AVC, &["-c:a", "aac", "-movflags", "+faststart"]]),
        "avc-aac-front.mp4",
    )?;
    ffmpeg(
        out,
        &[(LIFE, "6"), (CHORD_44K, "6")],
        &with(&[
            &HEVC,
            &[
                "-tag:v",
                "hvc1",
                "-c:a",
                "aac",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
            ],
        ]),
        "hevc-aac-fragmented.mp4",
    )?;
    ffmpeg(
        out,
        &[(CELLS, "5"), (CHORD_44K, "5")],
        &with(&[
            &AVC,
            &[
                "-c:a",
                "aac",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
            ],
        ]),
        "avc-aac-fragmented.mp4",
    )?;
    ffmpeg(
        out,
        &[(MANDELBROT, "4"), (CHORD_48K, "4")],
        &with(&[&AVC, &["-c:a", "libopus"]]),
        "avc-opus.mp4",
    )?;
    ffmpeg(
        out,
        &[(CELLS, "5"), (CHORD_48K, "5")],
        &with(&[
            &AVC,
            &["-c:a", "aac", "-f", "mpegts", "-mpegts_start_pid", "481"],
        ]),
        "program.ts",
    )?;
    ffmpeg(
        out,
        &[(LIFE, "4"), (CHORD_48K, "4")],
        &with(&[
            &HEVC,
            &["-c:a", "aac", "-f", "mpegts", "-mpegts_start_pid", "768"],
        ]),
        "program-hevc-aac.ts",
    )?;
    ffmpeg(
        out,
        &[(MANDELBROT, "4"), (CHORD_44K, "4")],
        &with(&[
            &AVC,
            &["-c:a", "aac", "-f", "mpegts", "-mpegts_service_id", "5"],
        ]),
        "service-5.ts",
    )?;
    ffmpeg(
        out,
        &[(LIFE, "4"), (CHORD_48K, "4")],
        &with(&[
            &HEVC,
            &[
                "-c:a",
                "eac3",
                "-f",
                "mpegts",
                "-mpegts_flags",
                "system_b",
                "-mpegts_service_id",
                "9",
            ],
        ]),
        "service-9-dvb.ts",
    )?;
    ffmpeg(
        out,
        &[(CHORD_44K, "5")],
        &["-c:a", "aac", "-f", "mpegts", "-mpegts_service_id", "6"],
        "audio-6.ts",
    )?;
    for (video, audio, codec, name) in [
        (&AVC[..], "aac", "avc", "hls-avc-aac"),
        (&HEVC[..], "ac3", "hevc", "hls-hevc-ac3"),
    ] {
        let segments = format!("{codec}-segment-%d.ts");
        ffmpeg(
            out,
            &[(CELLS, "6"), (CHORD_48K, "6")],
            &with(&[
                video,
                &[
                    "-c:a",
                    audio,
                    "-f",
                    "hls",
                    "-hls_time",
                    "2",
                    "-hls_segment_filename",
                    &out.join(&segments).display().to_string(),
                ],
            ]),
            &format!("{codec}.m3u8"),
        )?;
        fs::rename(
            out.join(format!("{codec}-segment-1.ts")),
            out.join(format!("{name}.ts")),
        )
        .map_err(|e| format!("{name}: {e}"))?;
    }
    ffmpeg(
        out,
        &[(CELLS, "3")],
        &with(&[&AVC, &["-f", "h264"]]),
        "video.h264",
    )?;
    ffmpeg(
        out,
        &[(LIFE, "3")],
        &with(&[&HEVC, &["-f", "hevc"]]),
        "video.hevc",
    )?;
    for (source, prefix, seeks) in [
        ("avc-aac-text.mkv", "avc", [1900, 3300]),
        ("hevc-opus.mkv", "hevc", [2300, 900]),
    ] {
        let data = fs::read(out.join(source)).map_err(|e| format!("{source}: {e}"))?;
        for (operation, target, name) in [
            ("startup", 0, "startup"),
            ("seek-after-discovery", seeks[0], "seek-discovery"),
            ("seek-after-end", seeks[1], "seek-end"),
        ] {
            let capsule = crate::corpus::playback_capsule(operation, target, &data)?;
            let path = out.join(format!("{prefix}-{name}.case"));
            fs::write(&path, capsule).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    let source = out.join("avc-aac.mp4");
    let data = fs::read(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    let (units, ..) = crate::codecs::aac_access_units::access_units(
        &data,
        &probe(&source, &crate::codecs::aac_access_units::PROBE)?,
    )?;
    fs::write(out.join("avc-aac.aac-access-units"), units)
        .map_err(|e| format!("access units: {e}"))?;
    for (source, script) in [
        ("avc-aac.mp4", "avc.bsf"),
        ("hevc-aac-fragmented.mp4", "hevc.bsf"),
    ] {
        let bytes = framing_script(&out.join(source), 48)?;
        fs::write(out.join(script), bytes).map_err(|e| format!("{script}: {e}"))?;
    }
    let mut names: Vec<String> = fs::read_dir(out)
        .map_err(|e| format!("{}: {e}", out.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    // The playlists, the other segments and the subtitle script are only the
    // means of making the training files.
    let (made, means): (Vec<String>, Vec<String>) = names.into_iter().partition(|name| {
        !Path::new(name)
            .extension()
            .is_some_and(|extension| extension == "m3u8" || extension == "srt")
            && !name.contains("-segment-")
    });
    for name in means {
        fs::remove_file(out.join(&name)).map_err(|e| format!("{name}: {e}"))?;
    }
    for name in made {
        let hash = sha256::file_hex(&out.join(&name)).map_err(|e| format!("{name}: {e}"))?;
        println!("{{\"file\": \"{name}\", \"sha256\": \"{hash}\"}}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Training;
    use std::path::Path;

    #[test]
    fn the_training_manifest_names_known_workers_and_uses_every_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let training = Training::load(root).unwrap();
        assert!(!training.operations.is_empty());
        let mp4_range = training
            .operations
            .iter()
            .find(|operation| operation.arguments.len() > 2 && operation.family() == "mp4-range-")
            .unwrap();
        assert_eq!(mp4_range.others(), ["24320"]);
        assert_eq!(mp4_range.command_line()[1], "avc-aac.mp4");
    }
}
