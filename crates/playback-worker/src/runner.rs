//! The command line and measured loop of every playback runner, and the runners
//! each worker binary carries.
//!
//! Each runner owns one mode prefix. It prepares its input outside the measurement,
//! then [`measure`] runs the operation and prints one JSON result line
//! (`triergon-harness-runner`).

use std::process::ExitCode;
use triergon_harness_runner::command;
pub use triergon_harness_runner::{Invocation, Mode, Result, Totals, measure};

/// The largest input file a runner reads.
pub const MAX_INPUT: u64 = 64 * 1024 * 1024;

/// One runner: the mode prefix it owns and the function that runs its command line.
pub type Run = fn(&[String]) -> Result<()>;

/// Every runner family, for `playback-worker`. Its reference build,
/// `ffmpeg-formats-native`, carries every demuxer these families read.
pub const RUNNERS: [(&str, Run); 8] = [
    (crate::matroska_video::PREFIX, crate::matroska_video::run),
    (crate::matroska_audio::PREFIX, crate::matroska_audio::run),
    (crate::hls::PREFIX, crate::hls::run),
    (crate::mp4_range::PREFIX, crate::mp4_range::run),
    (crate::mp4_forward::PREFIX, crate::mp4_forward::run),
    (crate::progressive::PREFIX, crate::progressive::run),
    (crate::ts_program::PREFIX, crate::ts_program::run),
    (crate::raw_video::PREFIX, crate::raw_video::run),
];

/// The Matroska runners, for `playback-matroska-worker`. Its reference build,
/// `ffmpeg-video-native`, demuxes Matroska alone.
pub const MATROSKA_RUNNERS: [(&str, Run); 2] = [
    (crate::matroska_video::PREFIX, crate::matroska_video::run),
    (crate::matroska_audio::PREFIX, crate::matroska_audio::run),
];

/// Run the mode in `args[0]` with the runner of `runners` that owns its prefix.
///
/// # Errors
///
/// The usage line when no runner owns the mode, or the runner's error.
pub fn dispatch(worker: &str, runners: &[(&str, Run)], args: &[String]) -> Result<()> {
    let mode = args.first().map_or("", String::as_str);
    let Some((_, run)) = runners.iter().find(|(prefix, _)| mode.starts_with(prefix)) else {
        let prefixes: Vec<&str> = runners.iter().map(|(prefix, _)| *prefix).collect();
        return Err(format!(
            "usage: {worker} PREFIX{{fold,bench,memory}} FILE ...; prefixes: {}",
            prefixes.join(" ")
        )
        .into());
    };
    run(args)
}

/// A worker's `main`: dispatch the command line to `runners` and print an error
/// with the worker's name.
// A separate startup frame preserves the measured instruction counts.
#[inline(never)]
pub fn main(worker: &str, runners: &[(&str, Run)]) -> ExitCode {
    command(Some(worker), 2, |args| dispatch(worker, runners, args))
}

#[cfg(test)]
mod tests {
    use super::{MATROSKA_RUNNERS, RUNNERS, dispatch};

    #[test]
    fn the_matroska_worker_runs_only_its_runners_from_the_full_table() {
        for (prefix, run) in MATROSKA_RUNNERS {
            assert!(
                RUNNERS
                    .iter()
                    .any(|(full, other)| *full == prefix && std::ptr::fn_addr_eq(*other, run)),
                "{prefix}"
            );
        }
        let worker = "playback-matroska-worker";
        for (prefix, _) in RUNNERS {
            if MATROSKA_RUNNERS.iter().any(|(own, _)| *own == prefix) {
                continue;
            }
            for mode in ["fold", "bench", "memory", "trace"] {
                let args = [format!("{prefix}{mode}"), "input".to_owned()];
                let error = dispatch(worker, &MATROSKA_RUNNERS, &args).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .starts_with("usage: playback-matroska-worker "),
                    "{error}"
                );
            }
        }
    }
}
