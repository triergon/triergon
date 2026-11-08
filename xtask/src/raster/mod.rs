//! The rasterising lane: the pinned libass reference and the rasterising
//! features' manifests. `cargo xtask build-workers` builds raster-worker and links
//! the reference worker (`harness/reference_raster.h`) against the installed
//! libass; the features run through `cargo xtask report`, `trace-identities` and
//! `fuzz --contract` like every other feature.

mod manifest;
mod reference;

pub use manifest::LANE;

use std::path::Path;

use crate::fetch;

pub const USAGE: &str = "usage: cargo xtask raster prepare [--jobs N]
       cargo xtask raster manifest [--record] [--output DIR]
       cargo xtask raster compare [--all | --diagnostic] [REPORT OPTION]...

  prepare   build the pinned libass release into .research/libass against the
            FreeType, FriBidi and HarfBuzz references (`cargo xtask fonts prepare`,
            `cargo xtask shape prepare`); then `cargo xtask build-workers` builds
            its reference worker
  manifest  write the generated case files into .research/raster/inputs and each
            rasterising feature's generated manifest (glyph cases from the font
            corpus, line cases from the case files, with the pins of
            harness/features/<feature>/pins.json) to DIR/<feature>/corpus.json
            (default .research/raster/manifests); with --record, first re-record
            every pin from the built reference
  compare   `cargo xtask report` on each required rasterising feature
            (raster-glyphs, raster-outlines, raster-bitmaps), with --all also on
            raster-sweep; with --diagnostic only on raster-simd, the required cases
            against the host's SIMD engine, which fails where that engine departs from
            the gated C engine; further report options pass through, and each report
            goes to OUTPUT/<feature> (OUTPUT: --output, default .research/raster-report)";

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
            let built = reference::prepare(root, jobs, &fetch::Network)?;
            println!("{}", built.display());
            Ok(())
        }
        [command, rest @ ..] if command == "manifest" => LANE.manifest_command(root, rest, USAGE),
        _ => Err(USAGE.into()),
    }
}
