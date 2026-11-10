//! Paired runtime measurement: warm both workers, then time them in randomly
//! ordered pairs under a host-wide lock, rejecting any run during which a
//! compiler, linker or FFmpeg process used CPU (or, on Linux, steal time grew).
//! The whole run is observed: each pair starts from the snapshot that closed the
//! one before. Every raw sample is kept; equal work is proven by the work digest.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::acceptance::{LINUX_CPU_KEY, active_cpu};
use super::validation::{array, field, text};
use super::worker::{self, arg, data, ok};
use crate::json::Value;
use crate::pyrandom::PyRandom;
use crate::stats;

/// Shared by every checkout and tool on the host, so only one timing or memory
/// session runs at a time.
const LOCK_NAME: &str = "triergon-bench.lock";
pub const MODULE_POLICY: &str = "rss-kernel-module-must-be-absent-v1";
const LINUX_MODULE: &str = "/sys/module/triergon_rss_bound";

/// Validate every recorded module-absence stage, independently of the current boot.
pub fn validate_module_absence(
    clock: &Value,
    raw: &[Value],
    require_linux: bool,
) -> Result<(), String> {
    if text(clock, "timing_module_policy")? != MODULE_POLICY {
        return Err("timing module policy mismatch".into());
    }
    let mut stages = vec!["before-timing-session".to_owned()];
    for role in ["candidate", "reference"] {
        stages.extend([
            format!("before-warmup:{role}"),
            format!("after-warmup:{role}"),
        ]);
    }
    for (index, sample) in raw.iter().enumerate() {
        let order = array(sample, "order")?;
        if order.len() != 2
            || !order.contains(&Value::str("candidate"))
            || !order.contains(&Value::str("reference"))
        {
            return Err("timing pair worker order is incomplete".into());
        }
        for role in order {
            let role = role.as_str().ok_or("timing worker order is not a string")?;
            stages.extend([
                format!("before-pair:{index}:{role}"),
                format!("after-pair:{index}:{role}"),
            ]);
        }
    }
    let observations = array(clock, "timing_module_observations")?;
    if stages.len() != observations.len() {
        return Err("timing module observations are incomplete".into());
    }
    for (stage, observation) in stages.iter().zip(observations) {
        if text(observation, "stage")? != stage {
            return Err("timing module observation stage mismatch".into());
        }
        let state = field(observation, "state")?;
        if text(state, "status")?
            != if require_linux {
                "module-absent"
            } else {
                "not-applicable"
            }
        {
            return Err("RSS kernel module absence during timing is unproven".into());
        }
    }
    Ok(())
}

/// The shortest span over which tracked processes are compared. CPU clocks tick
/// every 10 ms or so, and a pair can be shorter than that.
const CONTAMINATION_WINDOW: Duration = Duration::from_millis(125);

/// The exclusive host-wide measurement lock; fails at once if it is held.
pub struct Lock(File);

pub fn lock() -> Result<Lock, String> {
    let path = std::env::temp_dir().join(LOCK_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.try_lock()
        .map_err(|_| "another measurement holds the benchmark lock".to_owned())?;
    file.set_len(0).map_err(|e| e.to_string())?;
    write!(file, "{}", std::process::id()).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    Ok(Lock(file))
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// The process listing; a `ps` that cannot start or exits non-zero is an error.
fn ps() -> Result<String, String> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,time=,state=,comm="])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("ps: {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Darwin: tracked build processes and their CPU time from `ps`.
fn darwin_snapshot() -> Result<Value, String> {
    darwin_processes(&ps()?)
}

/// The tracked processes of a Darwin `ps` listing.
fn darwin_processes(listing: &str) -> Result<Value, String> {
    let mut result = Vec::new();
    for line in listing.lines() {
        let Some([pid, clock, state, command]) = split4(line) else {
            continue;
        };
        let name = Path::new(command)
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
        if ![
            "rustc", "cargo", "clang", "cc", "gcc", "ld", "lto", "ffmpeg",
        ]
        .contains(&name.as_str())
        {
            continue;
        }
        let Ok(pid) = pid.parse::<u64>() else {
            continue;
        };
        let mut seconds = 0.0;
        for component in clock.split(':') {
            seconds = seconds * 60.0
                + component
                    .parse::<f64>()
                    .map_err(|_| format!("unparsed ps clock {clock}"))?;
        }
        let ticks = (seconds * 100.0).round_ties_even() as i64;
        result.retain(|(k, _): &(String, Value)| *k != pid.to_string());
        result.push((
            pid.to_string(),
            Value::object([
                ("name", Value::str(name)),
                ("cpu_ticks", Value::int(ticks)),
                ("state", Value::str(state)),
            ]),
        ));
    }
    Ok(Value::Object(result))
}

/// Python's `line.split(maxsplit=3)` when it yields exactly four fields.
fn split4(line: &str) -> Option<[&str; 4]> {
    let mut rest = line.trim_start();
    let mut fields = [""; 4];
    for field in fields.iter_mut().take(3) {
        let end = rest.find(char::is_whitespace)?;
        *field = &rest[..end];
        rest = rest[end..].trim_start();
    }
    if rest.is_empty() {
        return None;
    }
    fields[3] = rest;
    Some(fields)
}

fn tracked_on_linux(name: &str) -> bool {
    let digits = |s: &str| {
        s.is_empty()
            || (s.starts_with('-') && s.len() > 1 && s[1..].bytes().all(|b| b.is_ascii_digit()))
    };
    let versioned = |base: &str| name.strip_prefix(base).is_some_and(digits);
    matches!(
        name,
        "rustc"
            | "cargo"
            | "cc"
            | "cc1"
            | "cc1plus"
            | "ld"
            | "ld.lld"
            | "ld.bfd"
            | "ld.gold"
            | "rust-lld"
            | "lto"
            | "lto1"
            | "ffmpeg"
            | "as"
            | "make"
            | "gmake"
            | "ninja"
            | "cmake"
    ) || versioned("clang")
        || versioned("clang++")
        || versioned("gcc")
        || versioned("g++")
}

/// Linux: tracked processes plus aggregate CPU accounting (steal time), boot
/// identity and clock rate, all with their raw sources.
fn linux_snapshot() -> Value {
    linux_snapshot_in(Path::new("/proc"), ps, clock_ticks)
}

/// The system clock rate (`getconf CLK_TCK`), which is fixed while the host runs.
fn clock_ticks() -> Result<i64, String> {
    static TICKS: std::sync::OnceLock<Result<i64, String>> = std::sync::OnceLock::new();
    TICKS
        .get_or_init(|| {
            let output = Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .map_err(|e| e.to_string())?;
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .map_err(|_| "missing boot/tick identity".to_owned())
        })
        .clone()
}

/// [`linux_snapshot`] with `proc` mounted at the given root and the process
/// listing and clock rate from the given sources.
fn linux_snapshot_in(
    proc: &Path,
    ps: impl FnOnce() -> Result<String, String>,
    clock_ticks: impl FnOnce() -> Result<i64, String>,
) -> Value {
    let mut evidence = vec![
        ("status".to_owned(), Value::str("unavailable")),
        ("platform".to_owned(), Value::str("Linux")),
    ];
    let mut processes = Vec::new();
    let collect = || -> Result<(), String> {
        let commands = ps()?;
        evidence.push(("ps_raw".into(), Value::str(&commands)));
        let raw = fs::read_to_string(proc.join("stat")).map_err(|e| e.to_string())?;
        evidence.push(("proc_stat_raw".into(), Value::str(&raw)));
        let aggregate: Vec<Vec<&str>> = raw
            .lines()
            .filter(|l| l.starts_with("cpu "))
            .map(|l| l.split_whitespace().collect())
            .collect();
        if aggregate.len() != 1 || aggregate[0].len() < 9 {
            return Err("missing aggregate /proc/stat steal counter".into());
        }
        let counters: Vec<i64> = aggregate[0][1..]
            .iter()
            .map(|v| v.parse::<i64>().map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        if counters.iter().any(|v| *v < 0) {
            return Err("negative /proc/stat counter".into());
        }
        let cpus: Vec<&str> = raw
            .lines()
            .filter_map(|l| {
                let name = l.split_whitespace().next()?;
                let rest = name.strip_prefix("cpu")?;
                (!rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_digit())
                    && l.len() > name.len())
                .then_some(name)
            })
            .collect();
        let mut unique = cpus.clone();
        unique.sort_unstable();
        unique.dedup();
        if cpus.is_empty() || unique.len() != cpus.len() {
            return Err("missing or duplicate per-CPU accounting".into());
        }
        let boot = fs::read_to_string(proc.join("sys/kernel/random/boot_id"))
            .map_err(|e| e.to_string())?
            .trim()
            .to_owned();
        let ticks = clock_ticks()?;
        if boot.is_empty() || ticks <= 0 {
            return Err("missing boot/tick identity".into());
        }
        evidence.push((
            "counters".into(),
            Value::Array(counters.into_iter().map(Value::int).collect()),
        ));
        evidence.push(("cpus".into(), Value::strings(cpus)));
        evidence.push(("boot_id".into(), Value::str(boot)));
        evidence.push(("ticks_per_second".into(), Value::int(ticks)));
        for line in commands.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Some([pid, clock, state, command]) = split4(line) else {
                return Err(format!("unparsed Linux ps record: {line}"));
            };
            let name = Path::new(command)
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned());
            if !tracked_on_linux(&name) {
                continue;
            }
            let process_raw =
                fs::read_to_string(proc.join(pid).join("stat")).map_err(|e| e.to_string())?;
            let (head, tail) = process_raw
                .rsplit_once(')')
                .ok_or_else(|| format!("unparsed Linux process stat: {pid}"))?;
            let values: Vec<&str> = tail.split_whitespace().collect();
            if !head.starts_with(&format!("{pid} (")) || values.len() < 20 {
                return Err(format!("unparsed Linux process stat: {pid}"));
            }
            let field = |i: usize| values[i].parse::<i64>().map_err(|e| e.to_string());
            let (utime, stime, start) = (field(11)?, field(12)?, field(19)?);
            if utime.min(stime).min(start) < 0 {
                return Err("negative Linux process clock".into());
            }
            processes.push((
                pid.to_owned(),
                Value::object([
                    ("name", Value::str(&name)),
                    ("cpu_ticks", Value::int(utime + stime)),
                    ("state", Value::str(values[0])),
                    ("start_ticks", Value::int(start)),
                    ("ps_clock_raw", Value::str(clock)),
                    ("ps_state_raw", Value::str(state)),
                    ("proc_stat_raw", Value::str(&process_raw)),
                ]),
            ));
        }
        Ok(())
    };
    match collect() {
        Ok(()) => evidence[0].1 = Value::str("recorded"),
        Err(error) => evidence.push(("error".into(), Value::str(error))),
    }
    let mut result = vec![(LINUX_CPU_KEY.to_owned(), Value::Object(evidence))];
    result.extend(processes);
    Value::Object(result)
}

pub fn cpu_snapshot() -> Result<Value, String> {
    if is_linux() {
        Ok(linux_snapshot())
    } else {
        darwin_snapshot()
    }
}

/// Tracked processes that used CPU over a short window.
pub fn competing_builds() -> Result<Vec<String>, String> {
    let before = cpu_snapshot()?;
    std::thread::sleep(CONTAMINATION_WINDOW);
    Ok(active_cpu(&before, &cpu_snapshot()?, false))
}

/// Compares each snapshot of a run with the one that opened the current window, and
/// opens the next window once this one is long enough to show a process's CPU clock.
struct ContaminationWatch {
    opened: Instant,
    snapshot: Value,
}

impl ContaminationWatch {
    fn new(snapshot: Value) -> Self {
        Self {
            opened: Instant::now(),
            snapshot,
        }
    }

    fn observe(&mut self, snapshot: &Value, now: Instant) -> Vec<String> {
        if now.saturating_duration_since(self.opened) < CONTAMINATION_WINDOW {
            return Vec::new();
        }
        let busy = active_cpu(&self.snapshot, snapshot, false);
        self.opened = now;
        self.snapshot = snapshot.clone();
        busy
    }

    /// Wait out the open window and check it, so a run shorter than one window is
    /// still observed over a full window.
    fn close(mut self) -> Result<Vec<String>, String> {
        std::thread::sleep(CONTAMINATION_WINDOW.saturating_sub(self.opened.elapsed()));
        let last = cpu_snapshot()?;
        Ok(self.observe(&last, Instant::now()))
    }
}

/// The external RSS kernel module must not even be loaded while timing.
pub fn timing_module_state() -> Value {
    if !is_linux() {
        let system = String::from_utf8_lossy(
            &Command::new("uname")
                .arg("-s")
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default(),
        )
        .trim()
        .to_owned();
        return Value::object([
            ("status", Value::str("not-applicable")),
            ("platform", Value::str(system)),
        ]);
    }
    module_state(Path::new(LINUX_MODULE))
}

#[cfg(unix)]
fn module_state(module: &Path) -> Value {
    use std::os::unix::fs::MetadataExt;
    let root = module.parent().expect("module under sysfs");
    let observe = || -> Result<Value, String> {
        let before = fs::metadata(root).map_err(|e| e.to_string())?;
        if !before.is_dir() {
            return Err("Linux module sysfs root is not a directory".into());
        }
        match fs::metadata(module) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let after = fs::metadata(root).map_err(|e| e.to_string())?;
                if !after.is_dir() || (before.dev(), before.ino()) != (after.dev(), after.ino()) {
                    return Err("Linux module sysfs root changed during observation".into());
                }
                Ok(Value::object([
                    ("status", Value::str("module-absent")),
                    ("path", Value::str(module.display().to_string())),
                    ("sysfs_root_device", Value::int(before.dev())),
                    ("sysfs_root_inode", Value::int(before.ino())),
                ]))
            }
            Err(e) => Err(e.to_string()),
            Ok(loaded) => Ok(Value::object([
                ("status", Value::str("module-loaded")),
                ("path", Value::str(module.display().to_string())),
                ("mode", Value::int(loaded.mode())),
                ("device", Value::int(loaded.dev())),
                ("inode", Value::int(loaded.ino())),
            ])),
        }
    };
    observe().unwrap_or_else(|error| {
        Value::object([
            ("status", Value::str("unavailable")),
            ("path", Value::str(module.display().to_string())),
            ("error", Value::str(error)),
        ])
    })
}

#[cfg(not(unix))]
fn module_state(module: &Path) -> Value {
    Value::object([
        ("status", Value::str("unavailable")),
        ("path", Value::str(module.display().to_string())),
    ])
}

/// The worker operation timed: `{prefix}bench FILE ITERATIONS ARGS...`.
#[derive(Clone)]
pub struct Operation {
    pub prefix: String,
    pub arguments: Vec<OsString>,
    /// Candidate compatibility options before the operation name.
    pub candidate_options: Vec<OsString>,
    /// Contract work fields in addition to the mandatory packet/byte/digest work.
    pub work_fields: Vec<String>,
    /// How long one worker run may take.
    pub timeout: Duration,
}

impl Operation {
    /// Arguments for one role, excluding the executable path.
    pub(crate) fn worker_arguments(
        &self,
        candidate: bool,
        mode: &str,
        input: &Path,
        iterations: Option<u32>,
    ) -> Vec<OsString> {
        let mut arguments = Vec::new();
        if candidate {
            arguments.extend(self.candidate_options.iter().cloned());
        }
        arguments.push(format!("{}{mode}", self.prefix).into());
        arguments.push(input.as_os_str().to_owned());
        if let Some(iterations) = iterations {
            arguments.push(arg(iterations));
        }
        arguments.extend(self.arguments.iter().cloned());
        arguments
    }
}

fn digest(path: &Path) -> Result<String, String> {
    Ok(crate::sha256::hex(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

fn status_only(
    status: &str,
    samples: &[Value],
    warmups: &[Value],
    observations: &[Value],
    extra: Vec<(&str, Value)>,
) -> Value {
    let mut fields = vec![
        ("status", Value::str(status)),
        ("samples", Value::Array(samples.to_vec())),
        ("warmups", Value::Array(warmups.to_vec())),
        (
            "timing_module_observations",
            Value::Array(observations.to_vec()),
        ),
        ("timing_module_policy", Value::str(MODULE_POLICY)),
    ];
    fields.extend(extra);
    Value::object(fields)
}

/// Up to three attempts; a contaminated attempt is kept and retried.
pub fn benchmark(
    candidate: &Path,
    reference: &Path,
    input: &Path,
    pairs: usize,
    iterations: u32,
    seed: u64,
    operation: &Operation,
) -> Result<Value, String> {
    retry_contaminated(|attempt| {
        benchmark_once(
            candidate,
            reference,
            input,
            pairs,
            iterations,
            seed + attempt,
            operation,
        )
    })
}

fn retry_contaminated(
    mut measure: impl FnMut(u64) -> Result<Value, String>,
) -> Result<Value, String> {
    let mut rejected = Vec::new();
    for attempt in 0..3 {
        let result = match measure(attempt) {
            Ok(result) => result,
            Err(error) if rejected.is_empty() => return Err(error),
            Err(error) => Value::object([
                ("status", Value::str("timing-attempt-failed")),
                ("error", Value::str(error)),
            ]),
        };
        if result.get("status").and_then(Value::as_str) != Some("contaminated") {
            let mut result = result;
            if let Value::Object(fields) = &mut result {
                fields.push((
                    "rejected_contaminated_attempts".into(),
                    Value::Array(rejected),
                ));
            }
            return Ok(result);
        }
        rejected.push(result);
    }
    let contaminants = rejected
        .last()
        .and_then(|result| result.get("contaminants"))
        .cloned()
        .unwrap_or(Value::Array(vec![]));
    Ok(Value::object([
        ("status", Value::str("contaminated")),
        ("rejected_contaminated_attempts", Value::Array(rejected)),
        ("contaminants", contaminants),
    ]))
}

/// Private copies of the two workers, each written in one sequential pass and at
/// paths of equal length.
///
/// A freshly linked binary and a sequentially written copy of it sit differently in
/// the page cache, and identical bytes then time about 0.7% apart, so neither worker
/// runs from the file its build left behind. Every timing runner (`report`, `codecs
/// time`, `capture qualify`) times these copies.
pub struct TimedWorkers {
    directory: PathBuf,
    pub candidate: PathBuf,
    pub reference: PathBuf,
}

impl TimedWorkers {
    pub fn new(candidate: &Path, reference: &Path) -> Result<Self, String> {
        let directory = candidate.with_file_name(format!(".timed-{}", std::process::id()));
        let copy = |slot: &str, worker: &Path| -> Result<PathBuf, String> {
            let target = directory.join(slot);
            fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            let target = target.join("worker");
            let bytes = fs::read(worker).map_err(|e| format!("{}: {e}", worker.display()))?;
            fs::write(&target, bytes).map_err(|e| format!("{}: {e}", target.display()))?;
            let permissions = fs::metadata(worker)
                .map_err(|e| format!("{}: {e}", worker.display()))?
                .permissions();
            fs::set_permissions(&target, permissions)
                .map_err(|e| format!("{}: {e}", target.display()))?;
            Ok(target)
        };
        let (candidate, reference) = (copy("a", candidate)?, copy("b", reference)?);
        Ok(TimedWorkers {
            directory,
            candidate,
            reference,
        })
    }
}

impl Drop for TimedWorkers {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn benchmark_once(
    candidate: &Path,
    reference: &Path,
    input: &Path,
    pairs: usize,
    iterations: u32,
    seed: u64,
    operation: &Operation,
) -> Result<Value, String> {
    if pairs < 2 || iterations < 1 {
        return Err("need at least two pairs and one iteration".into());
    }
    let timed = TimedWorkers::new(candidate, reference)?;
    let workers: [(&str, PathBuf); 2] = [
        ("candidate", timed.candidate.clone()),
        ("reference", timed.reference.clone()),
    ];
    let mut rng = PyRandom::new(seed);
    let mut samples: Vec<Value> = Vec::new();
    let (mut observations, mut warmups) = (Vec::new(), Vec::new());
    let argv = |name: &str, worker: &Path| {
        let mut argv = vec![worker.as_os_str().to_owned()];
        argv.extend(operation.worker_arguments(
            name == "candidate",
            "bench",
            input,
            Some(iterations),
        ));
        argv
    };
    macro_rules! module_check {
        ($stage:expr) => {{
            let state = timing_module_state();
            observations.push(Value::object([
                ("stage", Value::str($stage)),
                ("state", state.clone()),
            ]));
            let status = state
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if !matches!(status.as_str(), "module-absent" | "not-applicable") {
                return Ok(Value::object([
                    (
                        "status",
                        Value::str(if status == "module-loaded" {
                            "timing-module-loaded"
                        } else {
                            "timing-environment-invalid"
                        }),
                    ),
                    ("samples", Value::Array(samples)),
                    ("warmups", Value::Array(warmups)),
                    ("timing_module_observations", Value::Array(observations)),
                    ("timing_module_policy", Value::str(MODULE_POLICY)),
                ]));
            }
        }};
    }
    let _lock = lock()?;
    module_check!("before-timing-session");
    let busy = competing_builds()?;
    if !busy.is_empty() {
        return Ok(status_only(
            "contaminated",
            &samples,
            &warmups,
            &observations,
            vec![("contaminants", Value::strings(busy))],
        ));
    }
    let hashes: Vec<(&str, String)> = workers
        .iter()
        .map(|(n, w)| Ok((*n, digest(w)?)))
        .collect::<Result<_, String>>()?;
    let input_sha = match digest(input) {
        Ok(hash) => hash,
        Err(error) => {
            return Ok(status_only(
                "input-check-failed",
                &samples,
                &warmups,
                &observations,
                vec![("input_error", Value::str(error))],
            ));
        }
    };
    for (name, worker) in &workers {
        module_check!(format!("before-warmup:{name}"));
        let warm = worker::execute(&argv(name, worker), operation.timeout);
        warmups.push(Value::object([
            ("worker", Value::str(*name)),
            ("result", warm.clone()),
        ]));
        module_check!(format!("after-warmup:{name}"));
        if !ok(&warm) {
            return Ok(status_only(
                "warmup-failed",
                &samples,
                &warmups,
                &observations,
                vec![("result", warm)],
            ));
        }
    }
    let Ok(warm_work) =
        super::comparison::warmup_work(&warmups, i128::from(iterations), &operation.work_fields)
    else {
        return Ok(status_only(
            "unequal-work",
            &samples,
            &warmups,
            &observations,
            vec![],
        ));
    };
    let mut before = cpu_snapshot()?;
    let mut watch = ContaminationWatch::new(before.clone());
    for index in 0..pairs {
        let mut order = [0usize, 1];
        rng.shuffle(&mut order);
        let mut row = vec![
            ("pair".to_owned(), Value::int(index as i64)),
            (
                "order".to_owned(),
                Value::strings(order.map(|i| workers[i].0)),
            ),
            ("load".to_owned(), super::report::load_average()),
            ("cpu_before".to_owned(), before.clone()),
        ];
        samples.push(Value::Object(row.clone()));
        for i in order {
            let (name, worker) = &workers[i];
            module_check!(format!("before-pair:{index}:{name}"));
            row.push((
                name.to_string(),
                worker::execute(&argv(name, worker), operation.timeout),
            ));
            *samples.last_mut().expect("row") = Value::Object(row.clone());
            module_check!(format!("after-pair:{index}:{name}"));
        }
        let after = cpu_snapshot()?;
        row.push(("cpu_after".into(), after.clone()));
        *samples.last_mut().expect("row") = Value::Object(row);
        let mut busy = active_cpu(&before, &after, false);
        busy.extend(watch.observe(&after, Instant::now()));
        if !busy.is_empty() {
            return Ok(status_only(
                "contaminated",
                &samples,
                &warmups,
                &observations,
                vec![("contaminants", Value::strings(busy))],
            ));
        }
        before = after;
    }
    let busy = watch.close()?;
    if !busy.is_empty() {
        return Ok(status_only(
            "contaminated",
            &samples,
            &warmups,
            &observations,
            vec![("contaminants", Value::strings(busy))],
        ));
    }
    let mut values = Vec::new();
    for row in &samples {
        if workers.iter().any(|(n, _)| !row.get(n).is_some_and(ok)) {
            return Ok(status_only(
                "worker-failure",
                &samples,
                &warmups,
                &observations,
                vec![],
            ));
        }
        let (a, b) = (
            data(row.get("candidate").unwrap()),
            data(row.get("reference").unwrap()),
        );
        let work = super::comparison::paired_timing_work(
            row.get("candidate").expect("checked candidate"),
            row.get("reference").expect("checked reference"),
            i128::from(iterations),
            &operation.work_fields,
        );
        if !work
            .as_ref()
            .is_ok_and(|w| super::diff::equal(w, &warm_work))
        {
            return Ok(status_only(
                "unequal-work",
                &samples,
                &warmups,
                &observations,
                vec![],
            ));
        }
        let elapsed = |d: &Value| {
            d.get("elapsed_ns")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN)
        };
        values.push((elapsed(a), elapsed(b)));
    }
    let input_after = match digest(input) {
        Ok(hash) => hash,
        Err(error) => {
            return Ok(status_only(
                "input-check-failed",
                &samples,
                &warmups,
                &observations,
                vec![("input_error", Value::str(error))],
            ));
        }
    };
    let unchanged = [candidate, reference]
        .into_iter()
        .chain(workers.iter().map(|(_, w)| w.as_path()))
        .zip(hashes.iter().chain(&hashes))
        .all(|(w, (_, h))| digest(w).is_ok_and(|d| &d == h))
        && input_after == input_sha;
    if !unchanged {
        return Ok(status_only(
            "worker-or-input-changed",
            &samples,
            &warmups,
            &observations,
            vec![],
        ));
    }
    let summary = stats::ratio_interval(&values, seed, 4000)?;
    let per_iteration = |name: &str| {
        let times: Vec<f64> = samples
            .iter()
            .map(|r| {
                data(r.get(name).unwrap())
                    .get("elapsed_ns")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN)
                    / f64::from(iterations)
            })
            .collect();
        Value::object([
            ("p50", Value::float(stats::median(&times))),
            ("p95", Value::float(stats::percentile(&times, 0.95))),
        ])
    };
    Ok(Value::object([
        ("status", Value::str("measured")),
        (
            "worker_hashes",
            Value::object(hashes.iter().map(|(n, h)| (*n, Value::str(h)))),
        ),
        ("input_sha256", Value::str(input_sha)),
        ("seed", Value::int(seed as i64)),
        ("iterations", Value::int(iterations)),
        ("pairs", Value::int(pairs as i64)),
        (
            "ratio",
            Value::object([
                ("median", Value::float(summary.median)),
                ("lower_95", Value::float(summary.lower_95)),
                ("upper_95", Value::float(summary.upper_95)),
            ]),
        ),
        ("samples", Value::Array(samples.clone())),
        ("warmups", Value::Array(warmups)),
        ("contaminants", Value::Array(vec![])),
        ("timing_module_observations", Value::Array(observations)),
        ("timing_module_policy", Value::str(MODULE_POLICY)),
        (
            "per_iteration_ns",
            Value::object(workers.iter().map(|(n, _)| (*n, per_iteration(n)))),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_retry_preserves_rejected_samples() {
        let first = Value::object([
            ("status", Value::str("contaminated")),
            ("samples", Value::Array(vec![Value::int(17)])),
        ]);
        let result = retry_contaminated(|attempt| match attempt {
            0 => Ok(first.clone()),
            1 => Err("input unreadable".into()),
            _ => panic!("timing must stop on a failed retry"),
        })
        .unwrap();
        assert_eq!(
            result.get("status").and_then(Value::as_str),
            Some("timing-attempt-failed")
        );
        assert_eq!(
            result.get("error").and_then(Value::as_str),
            Some("input unreadable")
        );
        assert_eq!(
            result.get("rejected_contaminated_attempts"),
            Some(&Value::Array(vec![first]))
        );
    }

    #[test]
    fn failed_timing_preserves_complete_warmups_and_module_observations() {
        let warmups = vec![Value::object([
            ("worker", Value::str("candidate")),
            ("result", Value::object([("digest_b", Value::int(17))])),
        ])];
        let observations = vec![Value::object([(
            "stage",
            Value::str("after-warmup:candidate"),
        )])];
        let result = status_only("unequal-work", &[], &warmups, &observations, vec![]);
        assert_eq!(result.get("warmups"), Some(&Value::Array(warmups)));
        assert_eq!(
            result.get("timing_module_observations"),
            Some(&Value::Array(observations))
        );
        assert_eq!(result.get("samples"), Some(&Value::Array(vec![])));
    }

    #[test]
    fn compatibility_options_apply_only_to_the_candidate() {
        let operation = Operation {
            prefix: "open-".into(),
            arguments: ["1", "0", "memory", "4096"].map(OsString::from).to_vec(),
            candidate_options: vec!["--bounded-timing".into(), "--bounded-sync".into()],
            work_fields: vec![],
            timeout: worker::DEFAULT_TIMEOUT,
        };
        let expected = ["open-bench", "input.mov", "50", "1", "0", "memory", "4096"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(
            operation.worker_arguments(false, "bench", Path::new("input.mov"), Some(50)),
            expected
        );
        let mut candidate = operation.candidate_options.clone();
        candidate.extend(expected);
        assert_eq!(
            operation.worker_arguments(true, "bench", Path::new("input.mov"), Some(50)),
            candidate
        );
    }

    #[test]
    fn ordinary_memory_arguments_keep_the_existing_order() {
        let operation = Operation {
            prefix: "hls-".into(),
            arguments: vec!["avc".into(), "aac".into()],
            candidate_options: vec![],
            work_fields: vec![],
            timeout: worker::DEFAULT_TIMEOUT,
        };
        for candidate in [false, true] {
            assert_eq!(
                operation.worker_arguments(candidate, "memory", Path::new("input.ts"), None),
                ["hls-memory", "input.ts", "avc", "aac"].map(OsString::from)
            );
        }
    }

    #[test]
    fn timed_workers_are_equal_length_copies_removed_afterwards() {
        let dir = crate::walk::scratch("timed-workers");
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::write(dir.join("media-worker"), b"candidate").unwrap();
        fs::write(dir.join("other/ffmpeg-worker"), b"reference").unwrap();
        let timed =
            TimedWorkers::new(&dir.join("media-worker"), &dir.join("other/ffmpeg-worker")).unwrap();
        assert_eq!(fs::read(&timed.candidate).unwrap(), b"candidate");
        assert_eq!(fs::read(&timed.reference).unwrap(), b"reference");
        assert_eq!(
            timed.candidate.as_os_str().len(),
            timed.reference.as_os_str().len()
        );
        let directory = timed.directory.clone();
        drop(timed);
        assert!(!directory.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn linux_process_names_follow_the_tracked_set() {
        for name in [
            "rustc",
            "clang",
            "clang-18",
            "clang++-17",
            "gcc-13",
            "g++",
            "ld.lld",
            "cc1plus",
            "ffmpeg",
            "ninja",
        ] {
            assert!(tracked_on_linux(name), "{name}");
        }
        for name in ["clang-", "gcc-x", "python3", "ld.mold", "bash"] {
            assert!(!tracked_on_linux(name), "{name}");
        }
    }

    #[test]
    fn splits_ps_records_like_python() {
        assert_eq!(
            split4("  12 0:01.50 R  /usr/bin/cc -c x.c"),
            Some(["12", "0:01.50", "R", "/usr/bin/cc -c x.c"])
        );
        assert_eq!(split4("12 0:01 R"), None);
    }

    #[test]
    fn darwin_snapshots_parse_ps() {
        if is_linux() {
            return;
        }
        let snapshot = darwin_snapshot().unwrap();
        assert!(matches!(snapshot, Value::Object(_)));
        assert_eq!(
            timing_module_state().get("status").and_then(Value::as_str),
            Some("not-applicable")
        );
    }

    /// An authored `/proc` with one tracked process (pid 123) and its own
    /// aggregate and per-CPU counters; nothing native is read.
    struct Proc(PathBuf);

    /// A Linux `ps` record whose clock has a days field.
    const CLANG: &str = "123 1-02:03:04 S clang\n";
    const STAT: &str = "cpu 10 0 20 100 0 0 0 5 2 1\ncpu0 10 0 20 100 0 0 0 5 2 1\n";

    /// `/proc/123/stat` with `utime` `cpu`, `stime` 2 and start time `start`.
    fn proc_stat(cpu: i64, start: i64) -> String {
        let mut fields = vec!["0".to_owned(); 21];
        fields[0] = "S".into();
        fields[11] = cpu.to_string();
        fields[12] = "2".into();
        fields[19] = start.to_string();
        format!("123 (clang test) {}", fields.join(" "))
    }

    impl Proc {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("triergon-proc-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("123")).unwrap();
            fs::create_dir_all(root.join("sys/kernel/random")).unwrap();
            let proc = Proc(root);
            proc.write("123/stat", &proc_stat(10, 100));
            proc.write("sys/kernel/random/boot_id", "authored-boot-id\n");
            proc.write("stat", STAT);
            proc
        }

        fn write(&self, path: &str, text: &str) {
            fs::write(self.0.join(path), text).unwrap();
        }

        fn remove(&self, path: &str) {
            fs::remove_file(self.0.join(path)).unwrap();
        }

        fn snapshot(&self, listing: &str) -> Value {
            linux_snapshot_in(&self.0, || Ok(listing.to_owned()), || Ok(100))
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_watch_compares_processes_only_across_a_full_window() {
        let proc = Proc::new();
        let quiet = proc.snapshot(CLANG);
        proc.write("123/stat", &proc_stat(11, 100));
        let busy = proc.snapshot(CLANG);
        let mut watch = ContaminationWatch::new(quiet.clone());
        let start = watch.opened;
        assert!(watch.observe(&busy, start).is_empty());
        assert_eq!(
            watch.observe(&busy, start + CONTAMINATION_WINDOW),
            ["clang".to_owned()]
        );
        // The next window opens at the snapshot that closed this one.
        assert!(
            watch
                .observe(&busy, start + CONTAMINATION_WINDOW * 2)
                .is_empty()
        );
    }

    fn linux(snapshot: &Value) -> &Value {
        snapshot.get(LINUX_CPU_KEY).unwrap()
    }

    fn steal(snapshot: &Value) -> &Value {
        &linux(snapshot).get("counters").unwrap().as_array().unwrap()[7]
    }

    fn findings(before: &Value, after: &Value) -> Vec<String> {
        active_cpu(before, after, false)
    }

    /// Replace `key` of the Linux accounting record.
    fn with_linux(snapshot: &Value, key: &str, value: Value) -> Value {
        let mut changed = snapshot.clone();
        let Value::Object(members) = &mut changed else {
            unreachable!()
        };
        let (_, record) = members
            .iter_mut()
            .find(|(k, _)| k == LINUX_CPU_KEY)
            .unwrap();
        let Value::Object(fields) = record else {
            unreachable!()
        };
        fields.iter_mut().find(|(k, _)| k == key).unwrap().1 = value;
        changed
    }

    #[test]
    fn linux_ps_days_clock_does_not_replace_native_process_ticks() {
        let proc = Proc::new();
        let snapshot = proc.snapshot(CLANG);
        let process = snapshot.get("123").unwrap();
        assert_eq!(process.get("cpu_ticks"), Some(&Value::int(12)));
        assert_eq!(
            process.get("ps_clock_raw").and_then(Value::as_str),
            Some("1-02:03:04")
        );
        assert_eq!(
            linux(&snapshot).get("status").and_then(Value::as_str),
            Some("recorded")
        );
        assert_eq!(steal(&snapshot), &Value::int(5));
    }

    #[test]
    fn process_tick_advance_and_pid_reuse_are_contaminants() {
        let proc = Proc::new();
        let before = proc.snapshot(CLANG);
        for text in [proc_stat(11, 100), proc_stat(10, 101)] {
            proc.write("123/stat", &text);
            assert!(findings(&before, &proc.snapshot(CLANG)).contains(&"clang".to_owned()));
        }
    }

    #[test]
    fn cpu_steal_is_rejected_without_deleting_raw_counters() {
        let proc = Proc::new();
        let before = proc.snapshot("");
        proc.write("stat", &STAT.replace(" 5 2 1", " 6 2 1"));
        let after = proc.snapshot("");
        assert!(findings(&before, &after).contains(&"linux-cpu-steal".to_owned()));
        assert_eq!(steal(&after), &Value::int(6));
    }

    #[test]
    fn missing_proc_or_failed_ps_is_unavailable_never_idle() {
        let proc = Proc::new();
        let before = proc.snapshot("");
        proc.remove("stat");
        let unavailable = "linux-cpu-evidence-unavailable".to_owned();
        assert!(findings(&before, &proc.snapshot("")).contains(&unavailable));
        let failed =
            linux_snapshot_in(&proc.0, || Err("ps: exit status: 1".to_owned()), || Ok(100));
        assert!(findings(&before, &failed).contains(&unavailable));
    }

    #[test]
    fn missing_process_stat_and_missing_steal_counter_are_not_idle() {
        let proc = Proc::new();
        let before = proc.snapshot(CLANG);
        proc.remove("123/stat");
        let unavailable = "linux-cpu-evidence-unavailable".to_owned();
        assert!(findings(&before, &proc.snapshot(CLANG)).contains(&unavailable));
        proc.write("stat", "cpu 10 0 20 100\n");
        assert!(findings(&before, &proc.snapshot("")).contains(&unavailable));
    }

    #[test]
    fn counter_regression_boot_change_and_missing_linux_marker_fail_closed() {
        let proc = Proc::new();
        let before = proc.snapshot("");
        let regressed = {
            let mut counters = linux(&before).get("counters").unwrap().clone();
            let Value::Array(items) = &mut counters else {
                unreachable!()
            };
            items[7] = Value::int(4);
            with_linux(&before, "counters", counters)
        };
        let rebooted = with_linux(&before, "boot_id", Value::str("another-boot"));
        for after in [regressed, rebooted] {
            assert!(findings(&before, &after).contains(&"linux-cpu-evidence-invalid".to_owned()));
        }
        let empty = Value::Object(Vec::new());
        assert!(
            active_cpu(&empty, &empty, true).contains(&"linux-cpu-evidence-missing".to_owned())
        );
    }

    #[test]
    fn darwin_process_snapshot_shape_and_fractional_clock_are_unchanged() {
        let snapshot = darwin_processes("123 1:02.34 S /usr/bin/clang\n").unwrap();
        let expected =
            crate::json::parse(r#"{"123": {"name": "clang", "cpu_ticks": 6234, "state": "S"}}"#)
                .unwrap();
        assert!(
            super::super::diff::equal(&snapshot, &expected),
            "{snapshot:?}"
        );
    }
}
