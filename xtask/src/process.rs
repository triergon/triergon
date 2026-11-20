//! Child processes as the codec and capture runners drive them: text-mode
//! runs with a timeout, and fixed-size output records streamed from a child's
//! stdout or from a pipe it opens as its output file.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
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
    reaped: bool,
    recycle: SyncSender<Vec<u8>>,
    frames: Receiver<std::io::Result<(Vec<u8>, usize)>>,
    buffer: Vec<u8>,
    partial_bytes: Option<usize>,
    stderr: Receiver<std::io::Result<Vec<u8>>>,
    deadline: Instant,
    argv: Vec<String>,
}

/// A fully drained frame stream and the process that wrote it.
pub struct FrameExit {
    pub code: i64,
    pub stderr: String,
    pub partial_bytes: usize,
}

impl Frames {
    pub fn spawn(
        argv: &[String],
        size: usize,
        side: bool,
        timeout: Duration,
    ) -> Result<Frames, String> {
        let deadline = Instant::now() + timeout;
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
            #[cfg(unix)]
            std::os::unix::process::CommandExt::process_group(&mut command, 0);
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
        // Each producer owns one reusable frame. Reading from its pipe on a
        // separate thread lets the caller enforce the deadline during a short read.
        let (recycle, buffers) = mpsc::sync_channel::<Vec<u8>>(1);
        let (ready, frames) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut src = src;
            while let Ok(mut frame) = buffers.recv() {
                let read = read_record(&mut *src, &mut frame);
                let last = read.as_ref().map_or(true, |&n| n < frame.len());
                if ready.send(read.map(|n| (frame, n))).is_err() || last {
                    break;
                }
            }
        });
        let mut err = child.stderr.take().expect("piped stderr");
        let (done, stderr) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = err.read_to_end(&mut bytes).map(|_| bytes);
            let _ = done.send(result);
        });
        Ok(Frames {
            child,
            reaped: false,
            recycle,
            frames,
            buffer,
            partial_bytes: None,
            stderr,
            deadline,
            argv,
        })
    }

    pub fn count_stdout(argv: &[String], timeout: Duration) -> Result<(u64, FrameExit), Stop> {
        let mut stream = Self::spawn(argv, 64 * 1024, false, timeout)?;
        let mut bytes = 0;
        while let Some(chunk) = stream.next()? {
            bytes += chunk.len() as u64;
        }
        let end = stream.close()?;
        Ok((bytes + end.partial_bytes as u64, end))
    }

    fn receive<T>(&self, receiver: &Receiver<std::io::Result<T>>) -> Result<T, Stop> {
        let timeout = || Stop::Timeout(self.argv.clone());
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(timeout)?;
        receiver
            .recv_timeout(remaining)
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => timeout(),
                mpsc::RecvTimeoutError::Disconnected => {
                    Stop::Fatal(format!("{}: reader disconnected", self.argv[0]))
                }
            })?
            .map_err(|e| Stop::Fatal(format!("{} output: {e}", self.argv[0])))
    }

    /// Borrow the next full record. At EOF keep the short byte count, including on
    /// subsequent calls while a longer peer is still emitting frames.
    pub fn next(&mut self) -> Result<Option<&[u8]>, Stop> {
        if self.partial_bytes.is_some() {
            return Ok(None);
        }
        if !self.buffer.is_empty() {
            self.recycle
                .send(std::mem::take(&mut self.buffer))
                .map_err(|_| Stop::Fatal(format!("{}: frame reader disconnected", self.argv[0])))?;
        }
        let (frame, filled) = self.receive(&self.frames)?;
        self.buffer = frame;
        if filled < self.buffer.len() {
            self.partial_bytes = Some(filled);
            Ok(None)
        } else {
            Ok(Some(&self.buffer))
        }
    }

    /// Finish reading records and collect both process outputs under the same deadline.
    pub fn close(mut self) -> Result<FrameExit, Stop> {
        while self.next()?.is_some() {}
        // Keep the group leader unreaped until all pipe holders have closed them.
        let stderr = self.receive(&self.stderr)?;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= self.deadline => {
                    return Err(Stop::Timeout(self.argv.clone()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => return Err(Stop::Fatal(e.to_string())),
            }
        };
        self.reaped = true;
        Ok(FrameExit {
            code: exit_code(status),
            stderr: python::decode(&stderr),
            partial_bytes: self.partial_bytes.expect("read to EOF"),
        })
    }
}

fn read_record(src: &mut dyn Read, frame: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < frame.len() {
        match src.read(&mut frame[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

impl Drop for Frames {
    fn drop(&mut self) {
        // Reap a producer even when spawning or reading its peer failed.
        if !self.reaped {
            // A descendant may still hold either pipe after its parent exits.
            #[cfg(unix)]
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.child.id())])
                .stderr(Stdio::null())
                .status();
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
        let mut frames = Frames::spawn(
            &sh("printf abcdefg; echo err >&2"),
            3,
            false,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(frames.next().unwrap(), Some(&b"abc"[..]));
        assert_eq!(frames.next().unwrap(), Some(&b"def"[..]));
        for _ in 0..3 {
            assert_eq!(frames.next().unwrap(), None);
            assert_eq!(&frames.buffer[..frames.partial_bytes.unwrap()], b"g");
        }
        let end = frames.close().unwrap();
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
        let mut frames = Frames::spawn(&argv, 2, true, Duration::from_secs(5)).unwrap();
        assert_eq!(frames.next().unwrap(), Some(&b"wx"[..]));
        assert_eq!(frames.next().unwrap(), Some(&b"yz"[..]));
        assert_eq!(frames.next().unwrap(), None);
        assert_eq!(frames.close().unwrap().code, 0);
    }

    #[test]
    fn partial_picture_cannot_block_a_framed_read_past_its_deadline() {
        let argv = sh("printf a; sleep 30 & exit 0");
        let mut frames = Frames::spawn(&argv, 2, false, Duration::from_millis(250)).unwrap();
        assert!(matches!(frames.next(), Err(Stop::Timeout(command)) if command == argv));
        // The background child inherited both pipes. Dropping the timed-out
        // run must release both readers, including the pending frame buffer.
        let stdout = std::mem::replace(&mut frames.frames, mpsc::channel().1);
        let stderr = std::mem::replace(&mut frames.stderr, mpsc::channel().1);
        drop(frames);
        assert_eq!(
            stdout
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .1,
            1
        );
        assert!(stderr.recv_timeout(Duration::from_secs(5)).unwrap().is_ok());
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
