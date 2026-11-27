//! Differential worker for codec views.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]

mod audio;
mod audio_session_lifecycle;
mod blocks;
mod core_reader_protocol;
mod mpeg1_budget;
mod oracles;
mod readers;
mod video;

use triergon_harness_runner::{Prepare, failed, serve, smoke};

const OPERATIONS: &[(&str, Prepare)] = &[
    ("views-smoke-", smoke),
    ("media-probe-", readers::operation),
    ("elementary-metadata-", readers::operation),
    ("flac-integer-", readers::operation),
    ("ogg-tracks-", readers::operation),
    ("ogg-pages-", readers::operation),
    ("picture-props-", video::operation),
    ("h264-inspect-", video::operation),
    ("video-session-clock-", video::operation),
    (
        "audio-session-lifecycle-",
        audio_session_lifecycle::operation,
    ),
    ("hevc-access-units-", video::operation),
    ("video-nal-", video::operation),
    ("adts-sync-", audio::operation),
    ("ac3-header-", audio::operation),
    ("ac3-frame-info-", audio::operation),
    ("ac3-features-", audio::operation),
    ("layouts-", audio::operation),
    ("crc-", blocks::operation),
    ("rescale-", blocks::operation),
    ("bits-", oracles::operation),
    ("cabac-", oracles::operation),
    ("palette-", oracles::operation),
    ("rotations-", oracles::operation),
    ("rbsp-", oracles::operation),
    ("windows-", oracles::operation),
    ("divide-", oracles::operation),
    ("budget-", oracles::operation),
    ("core-reader-protocol-", core_reader_protocol::operation),
    ("mpeg1-budget-", mpeg1_budget::operation),
    ("opus-range-", oracles::operation),
];

// Codec lane commands keep their reader and decode protocols.
type Command = fn(&str, &[std::ffi::OsString]) -> Result<(), String>;

fn command(name: &str) -> Option<Command> {
    match name {
        "packets-push" | "packets-stream" | "seek" => Some(readers::command),
        "dsp-raw" => Some(oracles::command),
        name => match name.rsplit_once('-') {
            Some(("video-h264" | "video-hevc", "fold" | "raw" | "bench" | "memory")) => {
                Some(video::command)
            }
            Some((
                "audio-aac" | "audio-ac3" | "audio-eac3",
                "fold" | "raw" | "bench" | "memory",
            )) => Some(readers::command),
            Some((
                "audio-dts" | "audio-flac" | "audio-mp3" | "audio-vorbis" | "audio-opus",
                "fold" | "raw" | "bench" | "memory",
            )) => Some(audio::command),
            _ => None,
        },
    }
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    if let [_, name, arguments @ ..] = args.as_slice()
        && let Some(name) = name.to_str()
        && let Some(run) = command(name)
    {
        return match run(name, arguments) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => failed(None, 2, error),
        };
    }
    serve(args, OPERATIONS)
}

/// C's entry point, the worker's one exception to `deny(unsafe_code)`.
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}
