//! The command line and measured loop of every playback-worker mode.
//!
//! Each runner owns one mode prefix. It prepares its input outside the measurement,
//! then [`measure`] runs the operation and prints one JSON result line
//! (`triergon-harness-runner`).

pub use triergon_harness_runner::{Invocation, Mode, Refusal, Result, Totals, measure};

/// The largest input file a runner reads.
pub const MAX_INPUT: u64 = 64 * 1024 * 1024;

type Run = fn(&[String]) -> Result<()>;

const RUNNERS: [(&str, Run); 9] = [
    (crate::matroska_video::PREFIX, crate::matroska_video::run),
    (crate::matroska_audio::PREFIX, crate::matroska_audio::run),
    (crate::hls::PREFIX, crate::hls::run),
    (crate::mp4_range::PREFIX, crate::mp4_range::run),
    (crate::mp4_forward::PREFIX, crate::mp4_forward::run),
    (crate::progressive::PREFIX, crate::progressive::run),
    (crate::ts_program::PREFIX, crate::ts_program::run),
    (crate::ts::PREFIX, crate::ts::run),
    (crate::raw_video::PREFIX, crate::raw_video::run),
];

/// Run the mode in `args[0]` with the runner that owns its prefix.
pub fn dispatch(args: &[String]) -> Result<()> {
    let mode = args.first().map_or("", String::as_str);
    let Some((_, run)) = RUNNERS.iter().find(|(prefix, _)| mode.starts_with(prefix)) else {
        let prefixes: Vec<&str> = RUNNERS.iter().map(|(prefix, _)| *prefix).collect();
        return Err(format!(
            "usage: playback-worker PREFIX{{fold,bench,memory}} FILE ...; prefixes: {}",
            prefixes.join(" ")
        )
        .into());
    };
    run(args)
}
