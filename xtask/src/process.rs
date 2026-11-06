//! Child processes as the codec and capture runners drive them: text-mode
//! runs with a timeout, and fixed-size output records streamed from a child's
//! stdout or from a pipe it opens as its output file.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::harness::worker;
use crate::python;

/// Why a run stopped short: a timeout (recorded with the command line) or an
/// error that ends the whole command.
#[derive(Debug)]
pub enum Stop {
    Timeout(Vec<String>),
    Fatal(String),
}

impl From<String> for Stop {
    fn from(message: String) -> Self {
        Stop::Fatal(message)
    }
}

/// A finished text-mode run: exit code, stdout and stderr with universal newlines.
pub struct Text {
    pub code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// `subprocess.run(argv, capture_output=True, text=True, timeout=...)`.
pub fn run_text(argv: &[String], timeout: Duration) -> Result<Text, Stop> {
    let os: Vec<OsString> = argv.iter().map(OsString::from).collect();
    match worker::run(&os, &[], timeout)? {
        Some(finished) => Ok(Text {
            code: finished.code,
            stdout: python::text(&finished.stdout),
            stderr: python::text(&finished.stderr),
        }),
        None => Err(Stop::Timeout(argv.to_vec())),
    }
}

/// `subprocess.run(argv, check=True, capture_output=True, text=True)`: stdout
/// of a successful run; any failure is fatal.
pub fn check_output(argv: &[String]) -> Result<String, String> {
    let done = match run_text(argv, Duration::from_secs(u64::MAX / 4)) {
        Ok(done) => done,
        Err(Stop::Fatal(message)) => return Err(message),
        Err(Stop::Timeout(_)) => unreachable!("no timeout"),
    };
    if done.code != 0 {
        return Err(format!(
            "Command {} returned non-zero exit status {}.\n{}",
            python::repr_strings(argv),
            done.code,
            done.stderr
        ));
    }
    Ok(done.stdout)
}

/// The placeholder argument a side-output child receives in place of its
/// output path.
pub const OUT: &str = "{out}";

/// Fixed-size records a child writes to stdout, or (`side`) to a pipe it
/// opens as its output file in place of [`OUT`]; read as they arrive, never
/// stored. The side pipe is the child's standard input descriptor, opened by
/// the child as `/dev/fd/0`.
pub struct Frames {
    child: Child,
    src: Box<dyn Read + Send>,
    size: usize,
    err: Option<JoinHandle<Vec<u8>>>,
    /// The bytes of the last short read.
    pub tail: Vec<u8>,
    argv: Vec<String>,
}

impl Frames {
    pub fn spawn(argv: &[String], size: usize, side: bool) -> Result<Frames, String> {
        let argv: Vec<String> = argv
            .iter()
            .map(|a| {
                if side && a == OUT {
                    "/dev/fd/0".to_owned()
                } else {
                    a.clone()
                }
            })
            .collect();
        let mut child;
        let src: Box<dyn Read + Send>;
        {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).stderr(Stdio::piped());
            if side {
                let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
                command.stdin(writer).stdout(Stdio::null());
                child = command.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
                src = Box::new(reader);
            } else {
                command.stdout(Stdio::piped());
                child = command.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
                src = Box::new(child.stdout.take().expect("piped stdout"));
            }
        }
        let mut stderr = child.stderr.take().expect("piped stderr");
        let err = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            buffer
        });
        Ok(Frames {
            child,
            src,
            size,
            err: Some(err),
            tail: Vec::new(),
            argv,
        })
    }

    /// The next full record; `None` at a short read, whose bytes become `tail`.
    pub fn next(&mut self) -> Option<Vec<u8>> {
        let mut buffer = vec![0; self.size];
        let mut filled = 0;
        while filled < self.size {
            match self.src.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if filled < self.size {
            buffer.truncate(filled);
            self.tail = buffer;
            None
        } else {
            Some(buffer)
        }
    }

    /// Read what is left into `tail`, wait for the child and return its exit
    /// code, stderr and the final `tail`.
    pub fn close(mut self, timeout: Duration) -> Result<(i64, String, Vec<u8>), Stop> {
        let _ = self.src.read_to_end(&mut self.tail);
        drop(self.src);
        let started = Instant::now();
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= timeout => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Err(Stop::Timeout(self.argv));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => return Err(Stop::Fatal(e.to_string())),
            }
        };
        let stderr = self
            .err
            .take()
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        Ok((exit_code(status), python::decode(&stderr), self.tail))
    }
}

/// `Popen.returncode`: the exit code, or the negated signal number.
pub fn exit_code(status: std::process::ExitStatus) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .map_or_else(|| -i64::from(status.signal().unwrap_or(0)), i64::from)
    }
    #[cfg(not(unix))]
    {
        status.code().map_or(0, i64::from)
    }
}

/// A fresh temporary directory under `parent` (the system temporary
/// directory when unset), removed when dropped.
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    pub fn new(parent: Option<&std::path::Path>) -> Result<TempDir, String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = parent.map_or_else(std::env::temp_dir, std::path::Path::to_path_buf);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let path = base.join(format!(
            "tmp-triergon-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(TempDir(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn frames_keep_the_last_short_read() {
        let mut frames = Frames::spawn(&sh("printf abcdefg; echo err >&2"), 3, false).unwrap();
        assert_eq!(frames.next().as_deref(), Some(&b"abc"[..]));
        assert_eq!(frames.next().as_deref(), Some(&b"def"[..]));
        assert_eq!(frames.next(), None);
        assert_eq!(frames.tail, b"g");
        // A further read past the end replaces the tail, as Python's does.
        assert_eq!(frames.next(), None);
        assert_eq!(frames.tail, b"");
        let (code, stderr, tail) = frames.close(Duration::from_secs(5)).unwrap();
        assert_eq!((code, stderr.as_str(), tail.len()), (0, "err\n", 0));
    }

    #[test]
    fn side_output_streams_through_a_pipe() {
        let argv = vec![
            "sh".into(),
            "-c".into(),
            "printf wxyz > \"$0\"; echo out".into(),
            OUT.into(),
        ];
        let mut frames = Frames::spawn(&argv, 2, true).unwrap();
        assert_eq!(frames.next().as_deref(), Some(&b"wx"[..]));
        assert_eq!(frames.next().as_deref(), Some(&b"yz"[..]));
        assert_eq!(frames.next(), None);
        assert_eq!(frames.close(Duration::from_secs(5)).unwrap().0, 0);
    }

    #[test]
    fn text_runs_time_out_with_their_command_line() {
        match run_text(&sh("sleep 5"), Duration::from_millis(100)) {
            Err(Stop::Timeout(argv)) => assert_eq!(argv, sh("sleep 5")),
            _ => panic!("expected a timeout"),
        }
        let done = run_text(&sh("printf 'a\\r\\nb' >&2; exit 3"), Duration::from_secs(5)).unwrap();
        assert_eq!((done.code, done.stderr.as_str()), (3, "a\nb"));
    }
}
