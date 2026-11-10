//! The shaping lane: the pinned HarfBuzz and FriBidi reference, the generated
//! Unicode tables of triergon-shape and the shaping features' manifests. `cargo
//! xtask build-workers` builds shape-worker and links the reference worker
//! (`harness/reference_shape.h`) against the installed libraries; the features run
//! through `cargo xtask report`, `trace-identities` and `fuzz --contract` like every
//! other feature.

mod manifest;
mod reference;
mod texts;
mod ucd;

pub use manifest::GENERATOR;

use std::path::Path;

use crate::fetch;

pub const USAGE: &str = "usage: cargo xtask shape prepare [--jobs N]
       cargo xtask shape manifest [--record] [--output DIR]
       cargo xtask shape compare [--all] [REPORT OPTION]...
       cargo xtask shape ucd [--check]

  prepare   build the pinned FriBidi and HarfBuzz releases into .research/harfbuzz;
            then `cargo xtask build-workers` builds their reference worker
  manifest  write the generated text files into .research/shape/texts and each
            shaping feature's generated manifest (its cases from the font corpus,
            the texts and the feature's settings, with the pins of
            harness/features/<feature>/pins.json) to DIR/<feature>/corpus.json
            (default .research/shape/manifests); with --record, first re-record
            every pin from the built reference
  compare   `cargo xtask report` on each required shaping feature (shape-text,
            shape-libass, shape-bidi, shape-simple, shape-unicode), with --all also
            on shape-text-all; further report options pass through, and each report
            goes to OUTPUT/<feature> (OUTPUT: --output, default
            .research/shape-report)
  ucd       fetch the hash-pinned Unicode Character Database files and regenerate
            the tables in crates/shape/src/unicode, and the language tags in
            crates/shape/src/language from the unpacked HarfBuzz release; --check
            fails if they would change";

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
        [command, rest @ ..] if command == "manifest" => {
            GENERATOR.manifest_command(root, rest, USAGE)
        }
        [command, rest @ ..] if command == "ucd" => ucd::main(root, rest),
        _ => Err(USAGE.into()),
    }
}
