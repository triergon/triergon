//! Playback measurement worker; serialization and startup are outside the timed loop.
#![forbid(unsafe_code)]
mod hls;
mod matroska;
mod matroska_audio;
mod matroska_video;
mod mp4_forward;
mod mp4_range;
mod packet_fold;
mod progressive;
mod raw_video;
mod runner;
mod ts;
mod ts_au;
mod ts_program;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match runner::dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("playback-worker: {error}");
            ExitCode::from(2)
        }
    }
}
