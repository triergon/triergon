//! Checks a test run did not run. A test suite whose inputs are kept outside the
//! repository (the codec crates' FATE, conformance and FLAC vector tests) passes without
//! them, and says so on the test process's own standard error, one `skipped: WHAT` line
//! for each check it left out. A run of the workspace tests passes its standard error
//! through and keeps those lines by the test binary that wrote them, which cargo names in
//! the `Running SOURCE (EXECUTABLE)` line it prints before each binary runs.
//!
//! The gate's test step runs on a gate host, which holds the inputs of every suite
//! [`SUITES`] lists, so any check a suite left out fails it ([`gate_failures`]); CI
//! fetches none of them, and its test run says how many checks it left out
//! ([`summary`]).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::json::{self, Value};

/// A test suite that reports the checks it leaves out for absent inputs.
pub struct Suite {
    /// Its test source, from the repository root.
    pub source: &'static str,
    /// The directories it reads its inputs from, from the repository root, each of
    /// which the evidence sets a gate image is built with ([`IMAGE_INPUTS`]) hold.
    pub inputs: &'static [&'static str],
}

/// The ADTS streams of the AAC numeric comparison beside the decode profile's float
/// output of each, which the gate's test step names in `AAC_REF_DIR`.
pub const AAC_REFERENCES: &str = ".research/aac-references";

/// Every suite that reports checks it leaves out.
pub const SUITES: [Suite; 7] = [
    Suite {
        source: "crates/codec-aac/tests/fate_vectors.rs",
        inputs: &[".research/fate-samples/aac", AAC_REFERENCES],
    },
    Suite {
        source: "crates/codec-ac3/tests/vectors.rs",
        inputs: &[".research/fate-samples/ac3", ".research/fate-samples/eac3"],
    },
    Suite {
        source: "crates/codec-dca/tests/vectors.rs",
        inputs: &[".research/fate-samples/dts"],
    },
    Suite {
        source: "crates/codec-flac/tests/vectors.rs",
        inputs: &[".research/flac-test-files", ".research/flac-generated"],
    },
    Suite {
        source: "crates/codec-h264/tests/conformance.rs",
        inputs: &[".research/fate-samples/h264-conformance"],
    },
    Suite {
        source: "crates/codec-h264/tests/fuzz_regressions.rs",
        inputs: &[".research/fate-samples/h264-conformance"],
    },
    Suite {
        source: "crates/codec-hevc/tests/conformance.rs",
        inputs: &[".research/fate-samples/hevc-conformance"],
    },
];

/// The evidence sets of media inputs a gate image is built with (docs/quality.md,
/// "Gate hosts").
const IMAGE_INPUTS: [&str; 7] = [
    "gate-media-inputs",
    "codec-file-workload-inputs",
    "ogg-reader-inputs",
    "subtitle-cc-inputs",
    "codec-derived-streams",
    "codec-vector-inputs",
    "opus-testvectors",
];

/// One check a test binary reported it did not run.
#[derive(Debug, PartialEq, Eq)]
pub struct Skip {
    /// The binary's executable as cargo's `Running` line names it.
    pub executable: String,
    /// What did not run, and which input it lacks, as the test wrote it.
    pub what: String,
}

/// Runs `command`, a `cargo test`, with its standard error passed through line by line,
/// and returns its exit code and the checks its test binaries left out.
pub fn run(command: &mut Command) -> Result<(i32, Vec<Skip>), String> {
    let mut child = command
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{command:?}: {e}"))?;
    let errors = child
        .stderr
        .take()
        .ok_or("cargo test without a standard error")?;
    let mut through = std::io::stderr();
    let mut executable = String::new();
    let mut skips = Vec::new();
    let mut reader = BufReader::new(errors);
    let mut line = Vec::new();
    while reader
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("{command:?}: {e}"))?
        > 0
    {
        let _ = through.write_all(&line);
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if let Some(named) = running(text) {
            named.clone_into(&mut executable);
        } else if let Some(what) = text.strip_prefix("skipped: ") {
            skips.push(Skip {
                executable: executable.clone(),
                what: what.to_owned(),
            });
        }
        line.clear();
    }
    let status = child.wait().map_err(|e| format!("{command:?}: {e}"))?;
    Ok((status.code().unwrap_or(1), skips))
}

/// The executable of cargo's `Running SOURCE (EXECUTABLE)` line.
fn running(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("Running ")?;
    rest.strip_suffix(')')?
        .rsplit_once(" (")
        .map(|(_, executable)| executable)
}

/// The test source of each test executable `cargo ARGS --no-run --message-format=json`
/// builds, by the executable's file name, relative to `root`: what names a suite in a
/// report of its skipped checks. `args` is the test command without its `--` part.
pub fn sources(
    root: &Path,
    cargo: &mut Command,
    args: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let output = cargo
        .args(args)
        .args(["--no-run", "--message-format=json"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cargo test --no-run: {e}"))?;
    let mut sources = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(message) = json::parse(line) else {
            continue;
        };
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
        let (Some(executable), Some(source)) = (
            text(message.get("executable")),
            text(message.get("target").and_then(|t| t.get("src_path"))),
        ) else {
            continue;
        };
        let name = file_name(&executable).to_owned();
        let source = Path::new(&source)
            .strip_prefix(root)
            .map_or(source.clone(), |relative| relative.display().to_string());
        sources.insert(name, source);
    }
    Ok(sources)
}

fn file_name(executable: &str) -> &str {
    executable.rsplit(['/', '\\']).next().unwrap_or(executable)
}

/// The test source that reported `skip`, or its executable where `sources` lacks it.
fn suite<'a>(skip: &'a Skip, sources: &'a BTreeMap<String, String>) -> &'a str {
    sources
        .get(file_name(&skip.executable))
        .map_or(skip.executable.as_str(), String::as_str)
}

/// One line for each check the gate's test step left out, naming its suite and what it
/// lacks: a gate host holds the inputs of every suite [`SUITES`] lists, and a suite it
/// does not list says nothing of its inputs.
pub fn gate_failures(skips: &[Skip], sources: &BTreeMap<String, String>) -> Vec<String> {
    skips
        .iter()
        .map(|skip| {
            let source = suite(skip, sources);
            if SUITES.iter().any(|suite| suite.source == source) {
                format!("{source}: did not run on a gate host: {}", skip.what)
            } else {
                format!(
                    "{source}: did not run, and test_skips::SUITES does not list its inputs: {}",
                    skip.what
                )
            }
        })
        .collect()
}

/// Why a gate host lacks an input of [`SUITES`], by the kept sets of [`IMAGE_INPUTS`]
/// in `data`, the parsed JSON files under `harness/`.
pub fn design_errors(data: &[(String, Value)]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut listed: Vec<&str> = Vec::new();
    for name in IMAGE_INPUTS {
        let path = format!("{}/{name}.json", crate::evidence::SETS);
        match data.iter().find(|(file, _)| *file == path) {
            Some((_, set)) if set.get("retention").and_then(Value::as_str) == Some("retained") => {
                listed.extend(
                    set.get("files")
                        .and_then(Value::as_array)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|file| file.get("path").and_then(Value::as_str)),
                );
            }
            _ => errors.push(format!("{path}: a gate image's inputs are a kept set")),
        }
    }
    for suite in &SUITES {
        for input in suite.inputs {
            let held = listed.iter().any(|path| {
                path.strip_prefix(input)
                    .is_some_and(|rest| rest.starts_with('/'))
            });
            if !held {
                errors.push(format!(
                    "test_skips::SUITES: {}: a gate image lacks {input}",
                    suite.source
                ));
            }
        }
    }
    errors
}

/// How many checks a test run left out, and how many of each suite.
pub fn summary(skips: &[Skip], sources: &BTreeMap<String, String>) -> String {
    let mut by_suite: BTreeMap<&str, usize> = BTreeMap::new();
    for skip in skips {
        *by_suite.entry(suite(skip, sources)).or_default() += 1;
    }
    let mut lines = vec![format!(
        "{} checks did not run for absent inputs; the gate's test step runs them",
        skips.len()
    )];
    lines.extend(
        by_suite
            .iter()
            .map(|(suite, count)| format!("  {suite}: {count}")),
    );
    lines.join("\n")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn skip(executable: &str, what: &str) -> Skip {
        Skip {
            executable: executable.into(),
            what: what.into(),
        }
    }

    #[test]
    fn a_skipped_check_is_kept_by_the_binary_cargo_named_before_it() {
        let script = "echo '   Compiling triergon-codec-ac3 v0.1.0' >&2
echo '     Running unittests src/lib.rs (target/debug/deps/triergon_codec_ac3-0a)' >&2
echo 'test a ... ok'
echo '     Running tests/vectors.rs (target/debug/deps/vectors-1b)' >&2
echo 'skipped: the AC-3 vector checks: .research/fate-samples/ac3 absent' >&2
printf 'skipped: the AC-3 corruption test: x absent' >&2
exit 3";
        let (code, skips) = run(Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::null()))
        .unwrap();
        assert_eq!(code, 3);
        assert_eq!(
            skips,
            [
                skip(
                    "target/debug/deps/vectors-1b",
                    "the AC-3 vector checks: .research/fate-samples/ac3 absent"
                ),
                skip(
                    "target/debug/deps/vectors-1b",
                    "the AC-3 corruption test: x absent"
                ),
            ]
        );
    }

    #[test]
    fn a_gate_host_runs_every_check_of_the_suites_whose_inputs_it_holds() {
        let sources = BTreeMap::from([
            (
                "vectors-1b".to_owned(),
                "crates/codec-ac3/tests/vectors.rs".to_owned(),
            ),
            (
                "fate_vectors-2c".to_owned(),
                "crates/codec-aac/tests/fate_vectors.rs".to_owned(),
            ),
        ]);
        let skips = [
            skip(
                "target/debug/deps/vectors-1b",
                "the AC-3 vector checks: x absent",
            ),
            skip(
                "/t/debug/deps/fate_vectors-2c",
                "the AAC numeric comparison: AAC_REF_DIR is not set",
            ),
            skip("target/debug/deps/other-3d", "the Y check: z absent"),
        ];
        assert_eq!(
            gate_failures(&skips, &sources),
            [
                "crates/codec-ac3/tests/vectors.rs: did not run on a gate host: \
                 the AC-3 vector checks: x absent",
                "crates/codec-aac/tests/fate_vectors.rs: did not run on a gate host: \
                 the AAC numeric comparison: AAC_REF_DIR is not set",
                "target/debug/deps/other-3d: did not run, and test_skips::SUITES does not list \
                 its inputs: the Y check: z absent",
            ]
        );
        assert_eq!(
            summary(&skips, &sources),
            "3 checks did not run for absent inputs; the gate's test step runs them\n  \
             crates/codec-aac/tests/fate_vectors.rs: 1\n  \
             crates/codec-ac3/tests/vectors.rs: 1\n  target/debug/deps/other-3d: 1"
        );
    }

    /// The kept sets a gate image is built with, listing one file below each input
    /// `held` names.
    pub(crate) fn image_sets(held: &[&str]) -> Vec<(String, Value)> {
        IMAGE_INPUTS
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let files: Vec<String> = held
                    .iter()
                    .filter(|_| index == 0)
                    .map(|input| format!(r#"{{"path": "{input}/a", "sha256": "0"}}"#))
                    .collect();
                (
                    format!("{}/{name}.json", crate::evidence::SETS),
                    json::parse(&format!(
                        r#"{{"name": "{name}", "retention": "retained", "files": [{}]}}"#,
                        files.join(", ")
                    ))
                    .unwrap(),
                )
            })
            .collect()
    }

    /// The inputs of every suite, which a gate host holds.
    pub(crate) fn held() -> Vec<&'static str> {
        SUITES
            .iter()
            .flat_map(|suite| suite.inputs.iter().copied())
            .collect()
    }

    #[test]
    fn the_suites_say_which_inputs_a_gate_image_holds() {
        assert_eq!(design_errors(&image_sets(&held())), Vec::<String>::new());
        let mut fewer = held();
        fewer.retain(|input| *input != ".research/fate-samples/dts");
        assert_eq!(
            design_errors(&image_sets(&fewer)),
            [
                "test_skips::SUITES: crates/codec-dca/tests/vectors.rs: a gate image lacks \
              .research/fate-samples/dts"
            ]
        );
        let mut sets = image_sets(&held());
        sets.retain(|(path, _)| !path.ends_with("/codec-derived-streams.json"));
        assert_eq!(
            design_errors(&sets),
            [format!(
                "{}/codec-derived-streams.json: a gate image's inputs are a kept set",
                crate::evidence::SETS
            )]
        );
    }
}
