//! Running one worker operation. A failed, timed-out or malformed run is a
//! recorded outcome; a successful result is never synthesised.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::{self, Value};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One argument of a worker command line.
pub fn arg(value: impl ToString) -> OsString {
    OsString::from(value.to_string())
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
    let started = Instant::now();
    let mut command = Command::new(&argv[0]);
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

/// Run `argv` and classify the result as `ok` (a JSON object on stdout),
/// `timeout`, `worker-error` or `invalid-worker-output`.
pub fn execute(argv: &[OsString], timeout: Duration) -> Value {
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
