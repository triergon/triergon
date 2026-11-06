//! Paired runtime measurement: warm both workers, then time them in randomly
//! ordered pairs under a host-wide lock, rejecting any pair during which a
//! compiler, linker or FFmpeg process used CPU (or, on Linux, steal time grew).
//! Every raw sample is kept; equal work is proven by the work digest.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::acceptance::{LINUX_CPU_KEY, active_cpu};
use super::worker::{self, DEFAULT_TIMEOUT, arg, data, ok};
use crate::json::Value;
use crate::pyrandom::PyRandom;
use crate::stats;

/// Shared by every checkout and tool on the host, so only one timing or memory
/// session runs at a time.
const LOCK_NAME: &str = "safe-media-bench.lock";
pub const MODULE_POLICY: &str = "rss-kernel-module-must-be-absent-v1";
const LINUX_MODULE: &str = "/sys/module/safe_media_rss_bound";

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

fn ps() -> Result<String, String> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,time=,state=,comm="])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Darwin: tracked build processes and their CPU time from `ps`.
fn darwin_snapshot() -> Result<Value, String> {
    let listing = ps()?;
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
    let mut evidence = vec![
        ("status".to_owned(), Value::str("unavailable")),
        ("platform".to_owned(), Value::str("Linux")),
    ];
    let mut processes = Vec::new();
    let mut collect = || -> Result<(), String> {
        let commands = ps()?;
        evidence.push(("ps_raw".into(), Value::str(&commands)));
        let raw = fs::read_to_string("/proc/stat").map_err(|e| e.to_string())?;
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
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|e| e.to_string())?
            .trim()
            .to_owned();
        let ticks: i64 = String::from_utf8_lossy(
            &Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .map_err(|e| e.to_string())?
                .stdout,
        )
        .trim()
        .parse()
        .map_err(|_| "missing boot/tick identity")?;
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
                fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
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
    std::thread::sleep(Duration::from_millis(125));
    Ok(active_cpu(&before, &cpu_snapshot()?, false))
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
#[derive(Clone, Default)]
pub struct Operation {
    pub prefix: String,
    pub arguments: Vec<OsString>,
}

fn digest(path: &Path) -> Result<String, String> {
    Ok(crate::sha256::hex(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    ))
}

fn status_only(status: &str, extra: Vec<(&str, Value)>) -> Value {
    let mut fields = vec![("status", Value::str(status))];
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
    let mut rejected = Vec::new();
    let mut last = Value::Null;
    for attempt in 0..3 {
        let result = benchmark_once(
            candidate,
            reference,
            input,
            pairs,
            iterations,
            seed + attempt,
            operation,
        )?;
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
        rejected.push(result.clone());
        last = result;
    }
    Ok(Value::object([
        ("status", Value::str("contaminated")),
        ("rejected_contaminated_attempts", Value::Array(rejected)),
        (
            "contaminants",
            last.get("contaminants")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        ),
    ]))
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
    let workers: [(&str, PathBuf); 2] = [
        ("candidate", candidate.to_path_buf()),
        ("reference", reference.to_path_buf()),
    ];
    let mut rng = PyRandom::new(seed);
    let mut samples: Vec<Value> = Vec::new();
    let (mut observations, mut warmups) = (Vec::new(), Vec::new());
    let argv = |worker: &Path| {
        let mut argv = vec![
            worker.as_os_str().to_owned(),
            format!("{}bench", operation.prefix).into(),
            input.as_os_str().to_owned(),
            arg(iterations),
        ];
        argv.extend(operation.arguments.iter().cloned());
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
            vec![
                ("contaminants", Value::strings(busy)),
                ("samples", Value::Array(vec![])),
            ],
        ));
    }
    let hashes: Vec<(&str, String)> = workers
        .iter()
        .map(|(n, w)| Ok((*n, digest(w)?)))
        .collect::<Result<_, String>>()?;
    let input_sha = digest(input)?;
    for (name, worker) in &workers {
        module_check!(format!("before-warmup:{name}"));
        let warm = worker::execute(&argv(worker), DEFAULT_TIMEOUT);
        warmups.push(Value::object([
            ("worker", Value::str(*name)),
            ("result", warm.clone()),
        ]));
        module_check!(format!("after-warmup:{name}"));
        if !ok(&warm) {
            return Ok(status_only("warmup-failed", vec![("result", warm)]));
        }
    }
    for index in 0..pairs {
        let busy = competing_builds()?;
        if !busy.is_empty() {
            return Ok(status_only(
                "contaminated",
                vec![
                    ("contaminants", Value::strings(busy)),
                    ("samples", Value::Array(samples)),
                ],
            ));
        }
        let mut order = [0usize, 1];
        rng.shuffle(&mut order);
        let before = cpu_snapshot()?;
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
                worker::execute(&argv(worker), DEFAULT_TIMEOUT),
            ));
            *samples.last_mut().expect("row") = Value::Object(row.clone());
            module_check!(format!("after-pair:{index}:{name}"));
        }
        let after = cpu_snapshot()?;
        row.push(("cpu_after".into(), after.clone()));
        *samples.last_mut().expect("row") = Value::Object(row);
        let busy = active_cpu(&before, &after, false);
        if !busy.is_empty() {
            return Ok(status_only(
                "contaminated",
                vec![
                    ("contaminants", Value::strings(busy)),
                    ("samples", Value::Array(samples)),
                ],
            ));
        }
    }
    let mut values = Vec::new();
    for row in &samples {
        if workers.iter().any(|(n, _)| !row.get(n).is_some_and(ok)) {
            return Ok(status_only(
                "worker-failure",
                vec![("samples", Value::Array(samples))],
            ));
        }
        let (a, b) = (
            data(row.get("candidate").unwrap()),
            data(row.get("reference").unwrap()),
        );
        if ["iterations", "packets", "bytes", "digest"]
            .iter()
            .any(|k| {
                !a.get(k)
                    .zip(b.get(k))
                    .is_some_and(|(x, y)| super::diff::equal(x, y))
            })
        {
            return Ok(status_only(
                "unequal-work",
                vec![("samples", Value::Array(samples))],
            ));
        }
        let elapsed = |d: &Value| {
            d.get("elapsed_ns")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN)
        };
        values.push((elapsed(a), elapsed(b)));
    }
    let unchanged = workers
        .iter()
        .zip(&hashes)
        .all(|((_, w), (_, h))| digest(w).is_ok_and(|d| &d == h))
        && digest(input)? == input_sha;
    if !unchanged {
        return Ok(status_only(
            "worker-or-input-changed",
            vec![("samples", Value::Array(samples))],
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
}
