//! The ASS lane: the parse-level operations and the frame rendering of the
//! pinned libass, the build `cargo xtask raster prepare` makes, and the
//! features' manifests. `cargo xtask build-workers` builds ass-worker and
//! links the reference worker (`harness/reference_ass.h`, with
//! `harness/reference_ass_render.h`) against that libass; the features run
//! through `cargo xtask report`, `trace-identities` and `fuzz --contract`
//! like every other feature.

mod manifest;
mod observed;

pub use manifest::LANE;
pub use observed::observed_sources;

use std::path::Path;

use crate::fetch;

pub const USAGE: &str = "usage: cargo xtask ass prepare [--jobs N]
       cargo xtask ass manifest [--record] [--output DIR]
       cargo xtask ass compare [--all] [REPORT OPTION]...

  prepare   build the pinned libass, as `cargo xtask raster prepare` does: one
            build serves both lanes; and write the copy of its ass_parse.c the
            reference compiles, with each conversion C leaves undefined out of
            range observed, to .research/libass/observed; then `cargo xtask
            build-workers` builds its reference worker
  manifest  write each ASS feature's generated manifest (the parsing cases of
            harness/fixtures/ass, the rendering cases of
            harness/fixtures/ass-render, with the pins of
            harness/features/<feature>/pins.json) to DIR/<feature>/corpus.json
            (default .research/ass/manifests); with --record, first re-record
            every pin from the built reference
  compare   `cargo xtask report` on each ASS feature (ass-parse-scripts,
            ass-parse-chunks, ass-parse-events, ass-parse-host, which fails
            the command only on the targets its contract requires, and
            ass-render; --all adds none today); further report options pass
            through, and each report goes to OUTPUT/<feature> (OUTPUT:
            --output, default .research/ass-report)";

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [help] if help == "--help" || help == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        [command, rest @ ..] if command == "prepare" => {
            let jobs = match rest {
                [] => 4,
                [flag, value] if flag == "--jobs" => {
                    value.parse().map_err(|_| "jobs must be 1..8")?
                }
                _ => return Err(USAGE.into()),
            };
            let built = crate::raster::prepare_libass(root, jobs, &fetch::Network)?;
            println!("{}", built.display());
            println!("{}", observed_sources(root)?.display());
            Ok(())
        }
        [command, rest @ ..] if command == "manifest" => LANE.manifest_command(root, rest, USAGE),
        _ => Err(USAGE.into()),
    }
}
