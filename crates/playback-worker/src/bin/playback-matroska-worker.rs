//! Playback measurement worker with the Matroska runners alone: Matroska startup and
//! seek, and Matroska audio clocks. Its reference build demuxes Matroska alone.
#![forbid(unsafe_code)]
use playback_worker::runner;
use std::process::ExitCode;

fn main() -> ExitCode {
    runner::main("playback-matroska-worker", &runner::MATROSKA_RUNNERS)
}
