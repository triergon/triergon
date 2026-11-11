//! Recorded PGS cases for the harness, the `pgs-` operations of the formats
//! build of `harness/reference.c` (`harness/reference_pgs.h`).
//!
//! ```text
//! subtitle-capsule-worker pgs-{fold,memory} CAPSULE
//! subtitle-capsule-worker pgs-bench CAPSULE ITERATIONS
//! ```
//!
//! The `pgs-` modes ([`pgs`]) fold recorded PGS case capsules into their byte
//! transcript. They are a binary of their own so that the subtitle worker's
//! `sub-` operations compile as they would without them: a second caller of the
//! PGS receiver changes how its code is inlined.
#![forbid(unsafe_code)]

mod pgs;

use std::process::ExitCode;

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let result = match words.first().map(String::as_str) {
        Some(mode) if mode.starts_with(pgs::PREFIX) => pgs::run(&words),
        _ => Err(format!(
            "usage: subtitle-capsule-worker {}{{fold,bench,memory}}",
            pgs::PREFIX
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
