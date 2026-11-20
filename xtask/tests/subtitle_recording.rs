//! Process-level checks of publication and of the input handed to the reference.
#![cfg(unix)]
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CASES: &[u8] = br#"[{"id":"one","profile":"one","file":"input.ts","rights":"authored-synthetic","source":"test","arguments":[]}]"#;
const FOLD: &str = r#"{"packets":1,"bytes":3,"digest":101,"eof":true}"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("subtitle-recording-{name}-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("input.ts"), FOLD).unwrap();
        let reference = dir.join("reference");
        fs::write(
            &reference,
            r#"#!/bin/sh
set -eu
if [ -p reference-entered ]; then
    exec 3>reference-entered
    IFS= read -r cancel <cancel-writer
    kill -KILL "$PPID"
    IFS= read -r release <reference-release
    exit 0
fi
if [ -f replacement ]; then mv replacement input.ts; fi
if [ -f override ]; then cat override; else tee received < "$2"; fi
"#,
        )
        .unwrap();
        fs::set_permissions(&reference, fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }

    fn start(&self, mode: &str, feature: &str) -> Child {
        let mut child = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .current_dir(&self.0)
            .env("TMPDIR", &self.0)
            .args([
                "subtitle-fixtures",
                mode,
                feature,
                "./reference",
                "--corpus",
                ".",
                "--manifest",
                "pins.json",
                "--cases-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(CASES).unwrap();
        child
    }

    fn run(&self, mode: &str, feature: &str, error: Option<&str>) {
        let output = self.start(mode, feature).wait_with_output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        match error {
            Some(reason) => assert!(
                !output.status.success() && stderr.contains(reason),
                "{stderr}"
            ),
            None => assert!(output.status.success(), "{stderr}"),
        }
    }

    fn pins(&self) -> Vec<u8> {
        fs::read(self.0.join("pins.json")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn incomplete_broadcast_folds_preserve_the_manifest() {
    let f = Fixture::new("eof");
    for family in ["subtitle-dvb", "subtitle-teletext"] {
        f.run("record", family, None);
        let pins = f.pins();
        for incomplete in [
            FOLD.replace("true", "false"),
            FOLD.replace(r#","eof":true"#, ""),
        ] {
            fs::write(f.0.join("override"), incomplete).unwrap();
            for mode in ["record", "verify"] {
                f.run(mode, family, Some("reference did not reach the end"));
                assert_eq!(f.pins(), pins);
            }
            fs::remove_file(f.0.join("override")).unwrap();
        }
    }
}

#[test]
fn reference_reads_the_hashed_snapshot_when_the_source_is_replaced() {
    let f = Fixture::new("replace");
    f.run("record", "subtitle-dvb", None);
    let pins = f.pins();
    for mode in ["record", "verify"] {
        fs::write(f.0.join("input.ts"), FOLD).unwrap();
        fs::write(f.0.join("replacement"), FOLD.replace("101", "202")).unwrap();
        f.run(
            mode,
            "subtitle-dvb",
            Some("changed during the reference fold"),
        );
        assert_eq!(fs::read_to_string(f.0.join("received")).unwrap(), FOLD);
        assert_eq!(f.pins(), pins);
    }
}

// Bound blocking IPC so a recorder that fails before reaching the reference, or
// a leaked manifest lock, fails the test instead of hanging the workspace.
fn synchronized<T: Send + 'static>(
    action: impl FnOnce() -> T + Send + 'static,
) -> Result<T, mpsc::RecvTimeoutError> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(action());
    });
    receive.recv_timeout(Duration::from_secs(10))
}

#[test]
fn live_writer_excludes_a_competitor_and_process_exit_releases_the_lock() {
    let f = Fixture::new("cancel");
    f.run("record", "subtitle-dvb", None);
    let pins = f.pins();
    fs::write(f.0.join("input.ts"), FOLD.replace("101", "202")).unwrap();
    assert!(
        Command::new("mkfifo")
            .current_dir(&f.0)
            .args(["reference-entered", "cancel-writer", "reference-release"])
            .status()
            .unwrap()
            .success()
    );
    let mut first = f.start("record", "subtitle-dvb");
    let entered = f.0.join("reference-entered");
    let mut reference = synchronized(move || fs::File::open(entered))
        .expect("first recorder did not enter its reference")
        .unwrap();
    // Only this writer uses the rendezvous. A wrongly admitted competitor
    // must finish normally so the refusal assertion can catch it.
    fs::remove_file(f.0.join("reference-entered")).unwrap();
    let competitor = f
        .start("record", "subtitle-dvb")
        .wait_with_output()
        .unwrap();
    // With Cargo's Linux library path, xtask launches a child and waits for
    // it. The reference's parent is the recorder in both launch modes.
    let cancel = f.0.join("cancel-writer");
    let stopped = synchronized(move || {
        fs::write(cancel, b"cancel\n")?;
        first.wait()
    })
    .expect("recorder did not exit")
    .unwrap();

    // Wait on the same inode while the orphaned reference is still alive.
    let path = f.0.join("pins.json.record-lock");
    let released = synchronized(move || {
        let lock = fs::OpenOptions::new().write(true).open(path)?;
        lock.lock()?;
        lock.unlock()
    });
    // Let the reference exit even if the recorder leaked the lock.
    let path = f.0.join("reference-release");
    synchronized(move || {
        fs::write(path, b"done\n").unwrap();
        assert_eq!(reference.read(&mut [0]).unwrap(), 0);
    })
    .expect("reference did not exit");
    released
        .expect("killed recorder retained the lock")
        .unwrap();
    assert!(!stopped.success());
    let stderr = String::from_utf8(competitor.stderr).unwrap();
    assert!(
        !competitor.status.success() && stderr.contains("locking"),
        "{stderr}"
    );
    assert_eq!(f.pins(), pins);
    f.run("record", "subtitle-dvb", None);
    assert_ne!(f.pins(), pins);
    f.run("verify", "subtitle-dvb", None);
}
