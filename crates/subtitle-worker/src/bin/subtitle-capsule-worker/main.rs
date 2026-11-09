//! Recorded PGS cases and text subtitles for the harness, the `pgs-` and
//! `subtitle-text-` operations of the formats build of `harness/reference.c`
//! (`harness/reference_pgs.h`, `harness/reference_subtitle_text.h`).
//!
//! ```text
//! subtitle-capsule-worker pgs-{fold,memory} CAPSULE
//! subtitle-capsule-worker pgs-bench CAPSULE ITERATIONS
//! subtitle-capsule-worker subtitle-text-{fold,memory} FILE KIND [ENCODING]
//! subtitle-capsule-worker subtitle-text-bench FILE ITERATIONS KIND [ENCODING]
//! ```
//!
//! The `pgs-` modes ([`pgs`]) fold recorded PGS case capsules and the
//! `subtitle-text-` modes ([`text`]) text subtitle sessions and documents, each
//! into its own byte transcript. They are a binary of their own so that the
//! subtitle worker's `sub-` operations compile as they would without them.
#![forbid(unsafe_code)]

mod pgs;
mod text;
// The repository's JSON reader, for tests that read a corpus; the parts they
// do not call are unused here.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../../../xtask/src/json.rs"]
mod json;

use std::process::ExitCode;

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let result = match words.first().map(String::as_str) {
        Some(mode) if mode.starts_with(pgs::PREFIX) => pgs::run(&words),
        Some(mode) if mode.starts_with(text::PREFIX) => text::run(&words),
        _ => Err(format!(
            "usage: subtitle-capsule-worker {}{{fold,bench,memory}} | {}{{fold,bench,memory}}",
            pgs::PREFIX,
            text::PREFIX
        )
        .into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("subtitle-capsule-worker: {error}");
            ExitCode::from(2)
        }
    }
}
