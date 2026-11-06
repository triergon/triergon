//! Integration tests of the built workers: `cargo xtask worker-tests [FILTER]`.
//!
//! The tests run the release `media-worker` and the FFmpeg reference workers that
//! `cargo xtask build-workers` puts under `target/`, on the authored corpus in
//! `.research/`. Expectations come from the reference workers, from the
//! independent Python oracles (`python3 -m harness.oracles`), or from scalar
//! computations written here, never from the candidate under test.
//!
//! A failed check ends its test, or only its subtest where a test checks
//! several cases. Every test runs; the command fails if any check failed.

mod capture_contract;
mod catalogue;
mod checksum;
mod colour;
mod content;
mod header;
mod reference_encodings;
mod seek;
mod timing;
mod zlib;

use std::ffi::OsString;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harness::diff;
use crate::harness::oracle::{self, Kind};
use crate::harness::worker::{self, arg};
use crate::json::{self, Value};

/// The outcome of a test or subtest body: `Err` holds the failed check.
pub type Outcome = Result<(), String>;

struct Test {
    name: &'static str,
    run: fn(&mut Context<'_>) -> Outcome,
}

const TESTS: &[Test] = &[
    Test {
        name: "capture_contract::encoder_rate_loop_matches_the_approved_contract",
        run: capture_contract::encoder_rate_loop_matches_the_approved_contract,
    },
    Test {
        name: "catalogue::native_catalogue_exact_source_and_all_checksum_work",
        run: catalogue::native_catalogue_exact_source_and_all_checksum_work,
    },
    Test {
        name: "catalogue::original_hdr_vector_is_not_stripped_and_matches_complete_protocol",
        run: catalogue::original_hdr_vector_is_not_stripped_and_matches_complete_protocol,
    },
    Test {
        name: "checksum::both_workers_match_the_original_ordered_checksum",
        run: checksum::both_workers_match_the_original_ordered_checksum,
    },
    Test {
        name: "checksum::reviewed_packet_views_checksum_all_their_selected_work",
        run: checksum::reviewed_packet_views_checksum_all_their_selected_work,
    },
    Test {
        name: "colour::every_frozen_colour_profile_matches_native_and_exact_source",
        run: colour::every_frozen_colour_profile_matches_native_and_exact_source,
    },
    Test {
        name: "colour::metadata_benchmarks_consume_every_canonical_field",
        run: colour::metadata_benchmarks_consume_every_canonical_field,
    },
    Test {
        name: "colour::seeded_mastering_rationals_match_the_public_native_representation",
        run: colour::seeded_mastering_rationals_match_the_public_native_representation,
    },
    Test {
        name: "content::optional_pcm_decoder_view_preserves_the_full_source_event",
        run: content::optional_pcm_decoder_view_preserves_the_full_source_event,
    },
    Test {
        name: "content::ordered_chain_empty_and_private_bytes_against_independent_oracle",
        run: content::ordered_chain_empty_and_private_bytes_against_independent_oracle,
    },
    Test {
        name: "content::all_selected_compression_bytes_match_an_independent_source_oracle",
        run: content::all_selected_compression_bytes_match_an_independent_source_oracle,
    },
    Test {
        name: "header::header_encoding_does_not_rewrite_source_timing",
        run: header::header_encoding_does_not_rewrite_source_timing,
    },
    Test {
        name: "reference_encodings::combined_private_scope_is_an_explicit_unsupported_reference_case",
        run: reference_encodings::combined_private_scope_is_an_explicit_unsupported_reference_case,
    },
    Test {
        name: "reference_encodings::deflate_lacing_empty_and_encoding_storage_order_against_source_oracle",
        run: reference_encodings::deflate_lacing_empty_and_encoding_storage_order_against_source_oracle,
    },
    Test {
        name: "seek::codec_delay_audio_is_retained_without_repeating_initial_trim",
        run: seek::codec_delay_audio_is_retained_without_repeating_initial_trim,
    },
    Test {
        name: "seek::reference_unknown_cluster_fresh_and_after_eof_agree",
        run: seek::reference_unknown_cluster_fresh_and_after_eof_agree,
    },
    Test {
        name: "timing::exact_source_oracle_keeps_sub_tick_and_negative_values",
        run: timing::exact_source_oracle_keeps_sub_tick_and_negative_values,
    },
    Test {
        name: "timing::independent_source_oracle_keeps_fractional_lace_duration",
        run: timing::independent_source_oracle_keeps_fractional_lace_duration,
    },
    Test {
        name: "zlib::bad_header_checksum_dictionary_truncation_and_limits",
        run: zlib::bad_header_checksum_dictionary_truncation_and_limits,
    },
    Test {
        name: "zlib::mutations_never_panic_or_hang",
        run: zlib::mutations_never_panic_or_hang,
    },
    Test {
        name: "zlib::stored_fixed_dynamic_windows_blocks_and_overlap",
        run: zlib::stored_fixed_dynamic_windows_blocks_and_overlap,
    },
];

/// Run every test whose name contains `filter`; fails if any check failed.
pub fn run(root: &Path, filter: Option<&str>) -> Result<(), String> {
    let selected: Vec<&Test> = TESTS
        .iter()
        .filter(|t| filter.is_none_or(|f| t.name.contains(f)))
        .collect();
    if selected.is_empty() {
        return Err(format!("no worker test matches {}", filter.unwrap_or("")));
    }
    let workers = Workers::new(root);
    if let Some(missing) = workers.all().iter().find(|w| !w.is_file()) {
        return Err(format!(
            "{} is missing; run `cargo xtask build-workers` first",
            missing.display()
        ));
    }
    let mut failed = Vec::new();
    let (mut subtests, mut checks) = (0, 0);
    for test in &selected {
        let mut context = Context {
            root,
            workers: &workers,
            checks: 0,
            subtests: 0,
            failures: Vec::new(),
        };
        if let Err(message) = (test.run)(&mut context) {
            context.failures.push(message);
        }
        let verdict = if context.failures.is_empty() {
            "ok  "
        } else {
            "FAIL"
        };
        println!(
            "{verdict} {} ({} subtests, {} checks)",
            test.name, context.subtests, context.checks
        );
        for failure in &context.failures {
            println!("     {failure}");
        }
        subtests += context.subtests;
        checks += context.checks;
        if !context.failures.is_empty() {
            failed.push(test.name);
        }
    }
    println!(
        "{} tests, {subtests} subtests, {checks} checks",
        selected.len()
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} of {} worker tests failed: {}",
            failed.len(),
            selected.len(),
            failed.join(", ")
        ))
    }
}

/// The built workers.
pub struct Workers {
    /// The Triergon release worker.
    pub candidate: PathBuf,
    /// The FFmpeg reference worker of the Matroska build.
    pub reference: PathBuf,
    /// The FFmpeg reference worker that also links the decoders.
    pub combined: PathBuf,
}

impl Workers {
    fn new(root: &Path) -> Self {
        Workers {
            candidate: root.join("target/release/media-worker"),
            reference: root.join("target/harness/ffmpeg-worker"),
            combined: root.join("target/harness/ffmpeg-combined-worker"),
        }
    }

    fn all(&self) -> [&Path; 3] {
        [&self.candidate, &self.reference, &self.combined]
    }
}

/// One test's root, workers and tally.
pub struct Context<'a> {
    pub root: &'a Path,
    pub workers: &'a Workers,
    checks: usize,
    subtests: usize,
    failures: Vec<String>,
}

impl Context<'_> {
    /// One check: `Err(message)` unless `ok`.
    pub fn check(&mut self, ok: bool, message: impl FnOnce() -> String) -> Outcome {
        self.checks += 1;
        if ok { Ok(()) } else { Err(message()) }
    }

    /// `actual == expected` for plain values.
    pub fn same<T: PartialEq + std::fmt::Debug>(
        &mut self,
        actual: T,
        expected: T,
        what: impl Display,
    ) -> Outcome {
        let ok = actual == expected;
        self.check(ok, || {
            format!(
                "{what}: {} != {}",
                shorten(&format!("{actual:?}")),
                shorten(&format!("{expected:?}"))
            )
        })
    }

    /// Python `==` on JSON values.
    pub fn equal(&mut self, actual: &Value, expected: &Value, what: impl Display) -> Outcome {
        let ok = diff::equal(actual, expected);
        self.check(ok, || {
            let delta = diff::differences(actual, expected);
            format!(
                "{what}: values differ: {}",
                shorten(&json::canonical(&Value::Array(delta)))
            )
        })
    }

    /// The harness's strict typed comparison finds no difference.
    pub fn no_differences(
        &mut self,
        actual: &Value,
        expected: &Value,
        what: impl Display,
    ) -> Outcome {
        let delta = diff::differences(actual, expected);
        self.check(delta.is_empty(), || {
            format!(
                "{what}: {}",
                shorten(&json::canonical(&Value::Array(delta)))
            )
        })
    }

    /// A worker outcome of `worker::execute` has status `ok`.
    pub fn ok(&mut self, outcome: &Value, what: impl Display) -> Outcome {
        self.check(worker::ok(outcome), || {
            format!("{what}: {}", shorten(&json::canonical(outcome)))
        })
    }

    /// Run `body` as one subtest: a failed check ends the subtest, not the test.
    pub fn subtest(&mut self, label: impl Display, body: impl FnOnce(&mut Self) -> Outcome) {
        self.subtests += 1;
        if let Err(message) = body(self) {
            self.failures.push(format!("[{label}] {message}"));
        }
    }
}

fn shorten(text: &str) -> String {
    const LIMIT: usize = 600;
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes)", &text[..end], text.len())
}

/// The longest any single worker run may take.
const TIMEOUT: Duration = Duration::from_secs(120);

/// The command line `worker operation input extra...`.
pub fn argv(worker: &Path, operation: &str, input: &Path, extra: &[&dyn Display]) -> Vec<OsString> {
    [
        worker.as_os_str().to_owned(),
        operation.into(),
        input.as_os_str().to_owned(),
    ]
    .into_iter()
    .chain(extra.iter().map(arg))
    .collect()
}

/// A finished run within `timeout`; a run that cannot start or times out is an error.
pub fn finish(argv: &[OsString], timeout: Duration) -> Result<worker::Finished, String> {
    let shown = || {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    worker::run(argv, &[], timeout)?.ok_or_else(|| format!("{}: timed out", shown()))
}

/// The JSON document a run writes to stdout; a non-zero exit or output that is
/// not JSON is an error.
pub fn output(argv: &[OsString]) -> Result<Value, String> {
    let finished = finish(argv, TIMEOUT)?;
    let shown = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    if finished.code != 0 {
        return Err(format!(
            "{shown}: exit {}: {}",
            finished.code,
            shorten(&String::from_utf8_lossy(&finished.stderr))
        ));
    }
    let text = std::str::from_utf8(&finished.stdout).map_err(|e| format!("{shown}: {e}"))?;
    json::parse(text).map_err(|e| format!("{shown}: {e}"))
}

/// `worker::execute` with the harness's default timeout.
pub fn execute(argv: &[OsString]) -> Value {
    worker::execute(argv, worker::DEFAULT_TIMEOUT)
}

/// The oracle's expectation for `file`; an oracle error is a failure.
pub fn expected(root: &Path, kind: Kind, file: &Path) -> Result<Value, String> {
    oracle::run(root, kind, file)?.map_err(|e| format!("oracle error on {}: {e}", file.display()))
}

/// The bytes of a hex string.
pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return Err(format!("not hex: {}", shorten(text)));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| format!("{e}: {i}")))
        .collect()
}

/// The bytes of the hex string at `value`.
pub fn hex_bytes(value: Option<&Value>) -> Result<Vec<u8>, String> {
    unhex(
        value
            .and_then(Value::as_str)
            .ok_or("expected a hex string")?,
    )
}

/// `value[key]` as an array.
pub fn items<'v>(value: &'v Value, key: &str) -> Result<&'v [Value], String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("no array {key}"))
}

/// The `cases` of a feature corpus manifest.
pub fn corpus_cases(root: &Path, feature: &str) -> Result<Vec<Value>, String> {
    let manifest =
        crate::checks::read_json(&root.join(format!("harness/features/{feature}/corpus.json")))?;
    Ok(items(&manifest, "cases")?.to_vec())
}

/// A case's `file` under `.research/<directory>`.
pub fn case_file(root: &Path, directory: &str, case: &Value) -> Result<PathBuf, String> {
    let file = case
        .get("file")
        .and_then(Value::as_str)
        .ok_or("corpus case without a file")?;
    Ok(root.join(".research").join(directory).join(file))
}

/// A private directory, removed with everything in it when dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Result<Self, String> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("triergon-worker-tests-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Scratch(dir))
    }

    /// `name` in this directory, holding `data`.
    pub fn file(&self, name: &str, data: &[u8]) -> Result<PathBuf, String> {
        let path = self.0.join(name);
        std::fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
