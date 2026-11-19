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

/// The JSON `program` prints on standard output for `args`.
pub fn check_json(program: &std::path::Path, args: &[&str]) -> Result<crate::json::Value, String> {
    let mut argv = vec![program.display().to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    let output = check_output(&argv)?;
    crate::json::parse(&output).map_err(|error| format!("{}: {error}", program.display()))
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
    buffer: Vec<u8>,
    partial_bytes: Option<usize>,
    err: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    argv: Vec<String>,
}

/// A fully drained frame stream and the process that wrote it.
pub struct FrameExit {
    pub code: i64,
    pub stderr: String,
    pub partial_bytes: usize,
}

impl Frames {
    pub fn spawn(argv: &[String], size: usize, side: bool) -> Result<Frames, String> {
        if size == 0 {
            return Err("a frame must contain at least one byte".into());
        }
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(size)
            .map_err(|e| format!("cannot buffer a {size}-byte frame: {e}"))?;
        buffer.resize(size, 0);
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
            stderr.read_to_end(&mut buffer)?;
            Ok(buffer)
        });
        Ok(Frames {
            child,
            src,
            buffer,
            partial_bytes: None,
            err: Some(err),
            argv,
        })
    }

    /// Count arbitrary stdout bytes when no record size is known. Drain stdout
    /// concurrently so even a producer that never closes the pipe can time out.
    pub fn count_stdout(argv: &[String], timeout: Duration) -> Result<(u64, FrameExit), Stop> {
        let mut stream = Self::spawn(argv, 1, false)?;
        let mut stdout = std::mem::replace(&mut stream.src, Box::new(std::io::empty()));
        let bytes = std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
        let end = stream.close(timeout)?;
        let bytes = bytes
            .join()
            .map_err(|_| Stop::Fatal("stdout reader panicked".into()))?
            .map_err(|e| Stop::Fatal(format!("{} output: {e}", argv[0])))?;
        Ok((bytes, end))
    }

    /// Borrow the next full record. At EOF keep the short byte count, including on
    /// subsequent calls while a longer peer is still emitting frames.
    pub fn next(&mut self) -> Result<Option<&[u8]>, String> {
        if self.partial_bytes.is_some() {
            return Ok(None);
        }
        let mut filled = 0;
        while filled < self.buffer.len() {
            match self.src.read(&mut self.buffer[filled..]) {
                Ok(0) => {
                    self.partial_bytes = Some(filled);
                    return Ok(None);
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("{} output: {e}", self.argv[0])),
            }
        }
        Ok(Some(&self.buffer))
    }

    /// Finish reading records and report the producer's actual exit and tail.
    pub fn close(mut self, timeout: Duration) -> Result<FrameExit, Stop> {
        while self.next()?.is_some() {}
        let started = Instant::now();
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() >= timeout => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return Err(Stop::Timeout(std::mem::take(&mut self.argv)));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => return Err(Stop::Fatal(e.to_string())),
            }
        };
        let stderr = self
            .err
            .take()
            .expect("stderr reader")
            .join()
            .map_err(|_| Stop::Fatal("stderr reader panicked".into()))?
            .map_err(|e| Stop::Fatal(format!("{} stderr: {e}", self.argv[0])))?;
        Ok(FrameExit {
            code: exit_code(status),
            stderr: python::decode(&stderr),
            partial_bytes: self.partial_bytes.expect("read to EOF"),
        })
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // Reap a producer even when spawning or reading its peer failed.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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
        assert_eq!(frames.next().unwrap(), Some(&b"abc"[..]));
        assert_eq!(frames.next().unwrap(), Some(&b"def"[..]));
        for _ in 0..3 {
            assert_eq!(frames.next().unwrap(), None);
            assert_eq!(&frames.buffer[..frames.partial_bytes.unwrap()], b"g");
        }
        let end = frames.close(Duration::from_secs(5)).unwrap();
        assert_eq!(
            (end.code, end.stderr.as_str(), end.partial_bytes),
            (0, "err\n", 1)
        );
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
        assert_eq!(frames.next().unwrap(), Some(&b"wx"[..]));
        assert_eq!(frames.next().unwrap(), Some(&b"yz"[..]));
        assert_eq!(frames.next().unwrap(), None);
        assert_eq!(frames.close(Duration::from_secs(5)).unwrap().code, 0);
    }

    #[test]
    fn counting_unframed_stdout_keeps_the_worker_timeout() {
        let argv = sh("printf x; exec sleep 5");
        assert!(
            matches!(Frames::count_stdout(&argv, Duration::from_millis(100)),
                         Err(Stop::Timeout(command)) if command == argv)
        );
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
