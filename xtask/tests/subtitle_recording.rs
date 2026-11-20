//! Process-level checks of publication and of the input handed to the reference.
#![cfg(unix)]
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
if [ -f hold ]; then
    touch ready
    count=0
    while [ -f hold ] && [ "$count" -lt 300 ]; do
        sleep 0.05
        count=$((count+1))
    done
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

#[test]
fn live_writer_excludes_a_competitor_and_process_exit_releases_the_lock() {
    let f = Fixture::new("cancel");
    f.run("record", "subtitle-dvb", None);
    let pins = f.pins();
    fs::write(f.0.join("input.ts"), FOLD.replace("101", "202")).unwrap();
    fs::write(f.0.join("hold"), []).unwrap();
    let mut first = f.start("record", "subtitle-dvb");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.0.join("ready").exists() {
        assert!(Instant::now() < deadline, "first reference did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    f.run("record", "subtitle-dvb", Some("locking"));
    first.kill().unwrap();
    first.wait().unwrap();
    assert_eq!(f.pins(), pins);
    fs::remove_file(f.0.join("hold")).unwrap();
    f.run("record", "subtitle-dvb", None);
    assert_ne!(f.pins(), pins);
    f.run("verify", "subtitle-dvb", None);
}
