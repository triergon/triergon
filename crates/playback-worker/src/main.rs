//! Playback measurement worker with every runner family; serialization and startup
//! are outside the timed loop.
#![forbid(unsafe_code)]
use playback_worker::runner;
use std::process::ExitCode;

fn main() -> ExitCode {
    runner::main("playback-worker", &runner::RUNNERS)
}
