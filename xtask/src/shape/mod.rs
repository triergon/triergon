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

pub use manifest::{FEATURES, ROW_SOURCES, SMOKE_CASES, check, manifest, write_inputs};

use std::path::{Path, PathBuf};

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
            the tables in crates/shape/src/unicode; --check fails if they would
            change";

/// The `cargo xtask report` arguments `shape compare ARGS` stands for, one list per
/// feature: the required features, or with `--all` every shaping feature; `None`
/// for the other commands.
pub fn reports(args: &[String]) -> Option<Vec<Vec<String>>> {
    let (command, rest) = args.split_first()?;
    if command != "compare" {
        return None;
    }
    let (mut output, mut all) = (PathBuf::from(".research/shape-report"), false);
    let mut options = Vec::new();
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match (arg.as_str(), rest.clone().next()) {
            ("--all", _) => all = true,
            ("--output", Some(value)) => {
                output = PathBuf::from(value);
                rest.next();
            }
            _ => options.push(arg.clone()),
        }
    }
    Some(
        FEATURES
            .iter()
            .filter(|feature| all || feature.required)
            .map(|feature| {
                let feature = feature.id;
                let path = |file: &str| format!("harness/features/{feature}/{file}.json");
                let mut list = vec![
                    "--contract".to_owned(),
                    path("contract"),
                    "--manifest".to_owned(),
                    path("corpus"),
                    "--output".to_owned(),
                    output.join(feature).display().to_string(),
                ];
                list.extend(options.iter().cloned());
                list
            })
            .collect(),
    )
}

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
            let (mut record, mut output) = (false, root.join(".research/shape/manifests"));
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--record" => record = true,
                    "--output" => output = PathBuf::from(rest.next().ok_or(USAGE)?),
                    _ => return Err(USAGE.into()),
                }
            }
            if record {
                manifest::record(root)?;
            }
            manifest::write(root, &output)
        }
        [command, rest @ ..] if command == "ucd" => ucd::main(root, rest),
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{FEATURES, reports};

    #[test]
    fn compare_reports_the_required_features_or_all() {
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let required = reports(&args(&["compare", "--output", "out", "--memory"])).unwrap();
        assert_eq!(required.len(), 5);
        assert_eq!(
            required[0],
            args(&[
                "--contract",
                "harness/features/shape-text/contract.json",
                "--manifest",
                "harness/features/shape-text/corpus.json",
                "--output",
                "out/shape-text",
                "--memory",
            ])
        );
        assert_eq!(
            reports(&args(&["compare", "--all"])).unwrap().len(),
            FEATURES.len()
        );
        assert!(reports(&args(&["ucd"])).is_none());
    }
}
