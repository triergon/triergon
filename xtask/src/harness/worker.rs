//! Running one worker operation. A failed, timed-out or malformed run is a
//! recorded outcome; a successful result is never synthesised.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::{self, Value};

/// How long one worker run may take unless its contract says otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest `timeout_seconds` a contract may declare.
pub const MAXIMUM_TIMEOUT_SECONDS: u64 = 3600;

/// How long one worker run of `contract`'s cases may take: its `timeout_seconds`,
/// a whole number of seconds from 1 to [`MAXIMUM_TIMEOUT_SECONDS`], or
/// [`DEFAULT_TIMEOUT`] when it declares none.
pub fn contract_timeout(contract: &Value) -> Result<Duration, String> {
    let Some(declared) = contract.get("timeout_seconds") else {
        return Ok(DEFAULT_TIMEOUT);
    };
    match declared {
        Value::Number(number) => number
            .as_i128()
            .and_then(|seconds| u64::try_from(seconds).ok())
            .filter(|seconds| (1..=MAXIMUM_TIMEOUT_SECONDS).contains(seconds))
            .map(Duration::from_secs),
        _ => None,
    }
    .ok_or_else(|| {
        format!(
            "timeout_seconds must be a whole number of seconds from 1 to {MAXIMUM_TIMEOUT_SECONDS}"
        )
    })
}

/// One argument of a worker command line.
pub fn arg(value: impl ToString) -> OsString {
    OsString::from(value.to_string())
}

struct WorkerImage {
    bytes: Vec<u8>,
    permissions: fs::Permissions,
}

impl WorkerImage {
    fn read(path: &Path) -> std::io::Result<Self> {
        let mut file = fs::File::open(path)?;
        let permissions = file.metadata()?.permissions();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Self { bytes, permissions })
    }

    fn write(&self, slot: &Path) -> Result<PathBuf, String> {
        fs::create_dir_all(slot).map_err(|e| format!("{}: {e}", slot.display()))?;
        let target = slot.join("worker");
        let pending = slot.join(format!(".worker-{}", std::process::id()));
        let mut file =
            fs::File::create_new(&pending).map_err(|e| format!("{}: {e}", pending.display()))?;
        let written = file
            .write_all(&self.bytes)
            .and_then(|()| file.set_permissions(self.permissions.clone()));
        drop(file);
        if let Err(error) = written.and_then(|()| fs::rename(&pending, &target)) {
            let _ = fs::remove_file(&pending);
            return Err(format!("{}: {error}", target.display()));
        }
        Ok(target)
    }
}

pub(super) struct WorkerPair(WorkerImage, WorkerImage);

impl WorkerPair {
    pub fn read(candidate: &Path, reference: &Path) -> Result<Self, String> {
        let read =
            |path: &Path| WorkerImage::read(path).map_err(|e| format!("{}: {e}", path.display()));
        Ok(Self(read(candidate)?, read(reference)?))
    }

    /// Name persistent copies by the exact bytes about to be written.
    pub fn content_key(&self) -> String {
        format!(
            "{}-{}",
            crate::sha256::hex(&self.0.bytes),
            crate::sha256::hex(&self.1.bytes)
        )
    }

    /// Give both workers fresh files with the same write pattern and path length.
    pub fn write(&self, directory: &Path) -> Result<[PathBuf; 2], String> {
        Ok([
            self.0.write(&directory.join("a"))?,
            self.1.write(&directory.join("b"))?,
        ])
    }
}

/// The exit code (negative signal number when killed), stdout and stderr of a
/// finished process; `None` when it outlived `timeout` and was killed.
pub struct Finished {
    pub code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `argv` with extra environment changes; a missing executable is an error.
pub fn run(
    argv: &[OsString],
    env: &[(String, Option<String>)],
    timeout: Duration,
) -> Result<Option<Finished>, String> {
    run_in(argv, env, None, timeout)
}

/// [`run`] in the working directory `cwd`.
pub fn run_in(
    argv: &[OsString],
    env: &[(String, Option<String>)],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Option<Finished>, String> {
    let started = Instant::now();
    let mut command = Command::new(&argv[0]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("{}: {e}", argv[0].to_string_lossy()))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => break None,
        }
    };
    let (stdout, stderr) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    Ok(status.map(|status| Finished {
        code: status.code().map_or_else(|| -signal(&status), i64::from),
        stdout,
        stderr,
    }))
}

/// Candidate invocations recorded for trace identities: the worker whose calls
/// are recorded, and each call's arguments, exit code and stdout digest.
struct Recorder {
    worker: PathBuf,
    calls: Vec<Value>,
    argument_aliases: Vec<(OsString, OsString)>,
}

static RECORDER: std::sync::Mutex<Option<Recorder>> = std::sync::Mutex::new(None);

/// Record every later invocation of `worker` until [`stop_recording`].
pub fn record_calls(worker: &Path) {
    if let Ok(mut slot) = RECORDER.lock() {
        *slot = Some(Recorder {
            worker: worker.to_path_buf(),
            calls: Vec::new(),
            argument_aliases: Vec::new(),
        });
    }
}

/// Record the declared spelling of a resolved corpus argument for this case.
pub(super) fn record_argument_aliases(actual: &[OsString], declared: &[OsString]) {
    if let Ok(mut slot) = RECORDER.lock()
        && let Some(recorder) = slot.as_mut()
    {
        recorder.argument_aliases.clear();
        recorder.argument_aliases.extend(
            actual
                .iter()
                .zip(declared)
                .filter(|(actual, declared)| actual != declared)
                .map(|(actual, declared)| (actual.clone(), declared.clone())),
        );
    }
}

pub fn stop_recording() -> Vec<Value> {
    RECORDER
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .map(|recorder| recorder.calls)
        .unwrap_or_default()
}

/// A recorded candidate run: no timeout, and only status, exit code and output.
fn recorded(argv: &[OsString]) -> Option<Value> {
    let mut slot = RECORDER.lock().ok()?;
    let recorder = slot.as_mut()?;
    if fs::canonicalize(&argv[0]).ok().as_deref() != Some(recorder.worker.as_path()) {
        return None;
    }
    let finished = match run(argv, &[], Duration::from_secs(u64::MAX / 4)) {
        Ok(Some(finished)) => finished,
        Ok(None) => return None,
        Err(error) => panic!("cannot start worker: {error}"),
    };
    let shown = argv[1..].iter().enumerate().map(|(i, a)| {
        if i == 1 {
            Path::new(a).file_name().map_or_else(
                || a.to_string_lossy().into_owned(),
                |n| n.to_string_lossy().into_owned(),
            )
        } else {
            recorder
                .argument_aliases
                .iter()
                .find(|(actual, _)| actual == a)
                .map_or(a, |(_, declared)| declared)
                .to_string_lossy()
                .into_owned()
        }
    });
    recorder.calls.push(Value::object([
        ("argv", Value::strings(shown)),
        ("exit", Value::int(finished.code)),
        (
            "stdout_sha256",
            Value::str(crate::sha256::hex(&finished.stdout)),
        ),
    ]));
    if finished.code != 0 {
        return Some(Value::object([
            ("status", Value::str("worker-error")),
            ("exit_code", Value::int(finished.code)),
        ]));
    }
    Some(
        match std::str::from_utf8(&finished.stdout)
            .ok()
            .and_then(|t| json::parse(t).ok())
        {
            Some(data) => Value::object([
                ("status", Value::str("ok")),
                ("data", data),
                ("process_elapsed_ns", Value::int(0)),
            ]),
            None => Value::object([("status", Value::str("invalid-worker-output"))]),
        },
    )
}

/// Run `argv` and classify the result as `ok` (a JSON object on stdout),
/// `timeout`, `worker-error` or `invalid-worker-output`.
pub fn execute(argv: &[OsString], timeout: Duration) -> Value {
    if let Some(result) = recorded(argv) {
        return result;
    }
    let started = Instant::now();
    let finished = match run(argv, &[], timeout) {
        Ok(finished) => finished,
        // Python's subprocess raises here; a missing worker is a hard error.
        Err(error) => panic!("cannot start worker: {error}"),
    };
    let Some(finished) = finished else {
        let shown = Value::strings(argv.iter().map(|a| a.to_string_lossy().into_owned()));
        return Value::object([("status", Value::str("timeout")), ("argv", shown)]);
    };
    if finished.code != 0 {
        let text = String::from_utf8_lossy(&finished.stderr);
        let tail: String = text
            .chars()
            .rev()
            .take(8192)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Value::object([
            ("status", Value::str("worker-error")),
            ("exit_code", Value::int(finished.code)),
            ("stderr", Value::str(tail)),
        ]);
    }
    let data = std::str::from_utf8(&finished.stdout)
        .ok()
        .and_then(|text| json::parse(text).ok());
    match data {
        Some(data @ Value::Object(_)) => Value::object([
            ("status", Value::str("ok")),
            ("data", data),
            (
                "process_elapsed_ns",
                Value::int(started.elapsed().as_nanos() as i64),
            ),
        ]),
        _ => Value::object([("status", Value::str("invalid-worker-output"))]),
    }
}

#[cfg(unix)]
fn signal(status: &std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or(0, i64::from)
}

#[cfg(not(unix))]
fn signal(_: &std::process::ExitStatus) -> i64 {
    0
}

/// `status` of an outcome.
pub fn status(outcome: &Value) -> &str {
    outcome
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

pub fn ok(outcome: &Value) -> bool {
    status(outcome) == "ok"
}

/// `data` of an outcome, or JSON null.
pub fn data(outcome: &Value) -> &Value {
    outcome.get("data").unwrap_or(&Value::Null)
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    fn declaring(seconds: &str) -> Value {
        json::parse(&format!(r#"{{"id": "f", "timeout_seconds": {seconds}}}"#)).unwrap()
    }

    #[test]
    fn a_contract_without_a_timeout_takes_the_default() {
        let contract = json::parse(r#"{"id": "f"}"#).unwrap();
        assert_eq!(contract_timeout(&contract), Ok(DEFAULT_TIMEOUT));
    }

    #[test]
    fn a_declared_timeout_is_whole_seconds_within_the_bound() {
        assert_eq!(
            contract_timeout(&declaring("1")),
            Ok(Duration::from_secs(1))
        );
        assert_eq!(
            contract_timeout(&declaring("120")),
            Ok(Duration::from_secs(120))
        );
        assert_eq!(
            contract_timeout(&declaring("3600")),
            Ok(Duration::from_secs(MAXIMUM_TIMEOUT_SECONDS))
        );
        for invalid in [
            "0", "-30", "3601", "1.5", "120.0", "\"120\"", "null", "true",
        ] {
            assert!(contract_timeout(&declaring(invalid)).is_err(), "{invalid}");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<OsString> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn classifies_outcomes() {
        let ok = execute(&sh("echo '{\"a\": 1.50}'"), DEFAULT_TIMEOUT);
        assert_eq!(status(&ok), "ok");
        assert_eq!(data(&ok).get("a"), Some(&Value::float(1.5)));
        let failed = execute(&sh("echo bad >&2; exit 3"), DEFAULT_TIMEOUT);
        assert_eq!(failed.get("exit_code"), Some(&Value::int(3)));
        assert_eq!(failed.get("stderr").and_then(Value::as_str), Some("bad\n"));
        assert_eq!(
            status(&execute(&sh("echo '[1]'"), DEFAULT_TIMEOUT)),
            "invalid-worker-output"
        );
        assert_eq!(
            status(&execute(&sh("echo nope"), DEFAULT_TIMEOUT)),
            "invalid-worker-output"
        );
        let slow = execute(&sh("sleep 5"), Duration::from_millis(100));
        assert_eq!(status(&slow), "timeout");
        assert_eq!(
            slow.get("argv").and_then(Value::as_array).map(<[_]>::len),
            Some(3)
        );
    }
}
