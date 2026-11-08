//! The ASS parsing features' manifests, generated from the authored fixtures in
//! `harness/fixtures/ass`: every script (`.ass`, `.ssa`) read whole, every
//! stream (`.chunks`) run packet by packet, and the events of every fixture
//! with `;@time` walked at those times. The work each case must fold is
//! pinned in `harness/features/<id>/pins.json`, libass's folds (see
//! [`crate::lanes`]).

use std::path::Path;

use crate::json::Value;
use crate::lanes::Lane;

type R<T> = Result<T, String>;

/// The fixtures, relative to the root; the cases' files are in it.
pub const FIXTURES: &str = "harness/fixtures/ass";

/// The files case rows are generated from, besides the pins and the fixtures,
/// which each row records by hash.
pub const ROW_SOURCES: [&str; 1] = ["xtask/src/ass/manifest.rs"];

/// The cases `cargo xtask check` mutates: the scripts with the most tag,
/// style and header quirks, a stream with every command, and the walks over
/// drawings, karaoke and animation.
pub const SMOKE_CASES: [(&str, &[&str]); 3] = [
    (
        "ass-parse-scripts",
        &[
            "tags-parens-ass",
            "styles-quirks-ass",
            "fonts-extracted-ass",
        ],
    ),
    ("ass-parse-chunks", &["stream-basic-chunks"]),
    (
        "ass-parse-events",
        &["drawings-ass", "karaoke-ass", "animation-ass"],
    ),
];

pub const FEATURES: [&str; 3] = ["ass-parse-scripts", "ass-parse-chunks", "ass-parse-events"];

pub const LANE: Lane = Lane {
    command: "ass",
    main: super::main,
    features: &FEATURES,
    reference: "libass-ass-native",
    pin_note: "libass 0.17.5's folds, recorded by `cargo xtask ass manifest --record`; never edited by hand.",
    report_directory: ".research/ass-report",
    manifest_directory: ".research/ass/manifests",
    row_sources: &ROW_SOURCES,
    rows,
    note,
    directory: |_| FIXTURES,
    write_inputs: crate::lanes::no_inputs,
    check_inputs,
    smoke: &SMOKE_CASES,
};

fn note(id: &str) -> &'static str {
    match id {
        "ass-parse-scripts" => {
            "Every authored script read whole as libass 0.17.5's ass_read_memory reads it, with the style overrides and font extraction its directives set: every track, style and event field and every embedded font."
        }
        "ass-parse-chunks" => {
            "Every authored stream run as a Matroska track: codec private, packets through ass_process_chunk with ReadOrder deduplication, more data, style lines, flushes and prunes, the track recorded after each step that asks and at the end."
        }
        _ => {
            "Every event of every authored script or stream with times, walked at those times: per glyph the state its override tags leave, the event state, drawing and clip outlines and karaoke over synthetic positions."
        }
    }
}

/// How a fixture is read, by its extension.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Script(&'static str),
    Stream,
}

fn kind(name: &str) -> Option<Kind> {
    match name.rsplit_once('.')?.1 {
        "ass" => Some(Kind::Script("ass")),
        "ssa" => Some(Kind::Script("ssa")),
        "chunks" => Some(Kind::Stream),
        _ => None,
    }
}

/// The fixtures: name and contents, by name.
fn fixtures(root: &Path) -> R<Vec<(String, Vec<u8>)>> {
    let directory = root.join(FIXTURES);
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))?
    {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{}: not a UTF-8 name", path.display()))?
            .to_owned();
        let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        files.push((name, data));
    }
    files.sort();
    Ok(files)
}

/// Whether a fixture walks its events: it has a `;@time` line.
fn timed(data: &[u8]) -> bool {
    data.split(|&c| c == b'\n')
        .any(|line| line.starts_with(b";@time "))
}

fn rows(root: &Path, id: &str) -> R<Vec<Value>> {
    let mut rows = Vec::new();
    for (name, data) in fixtures(root)? {
        let kind = kind(&name).ok_or_else(|| format!("{FIXTURES}/{name}: not a fixture"))?;
        let (profile, arguments): (&str, &[&str]) = match (id, kind) {
            ("ass-parse-scripts", Kind::Script(dialect)) => (dialect, &["script"]),
            ("ass-parse-chunks", Kind::Stream) => ("stream", &["chunks"]),
            ("ass-parse-events", Kind::Script(_)) if timed(&data) => {
                ("script", &["script", "events"])
            }
            ("ass-parse-events", Kind::Stream) if timed(&data) => ("stream", &["chunks", "events"]),
            ("ass-parse-scripts" | "ass-parse-chunks" | "ass-parse-events", _) => continue,
            _ => return Err(format!("{id}: not an ASS parsing feature")),
        };
        rows.push(Value::object([
            ("id", Value::str(name.replace('.', "-"))),
            ("profile", Value::str(profile)),
            ("file", Value::str(&name)),
            ("sha256", Value::str(crate::sha256::hex(&data))),
            ("rights", Value::str("authored-synthetic")),
            ("arguments", Value::strings(arguments.iter().copied())),
        ]));
    }
    Ok(rows)
}

/// Every file of the fixture directory is a script or a stream; a tree
/// without the lane's features needs no fixtures.
fn check_inputs(root: &Path) -> Vec<String> {
    let registered = FEATURES.iter().any(|feature| {
        root.join("harness/features")
            .join(feature)
            .join("contract.json")
            .is_file()
    });
    if !registered {
        return Vec::new();
    }
    match fixtures(root) {
        Ok(files) => files
            .iter()
            .filter(|(name, _)| kind(name).is_none())
            .map(|(name, _)| format!("{FIXTURES}/{name}: neither a script nor a stream"))
            .collect(),
        Err(error) => vec![error],
    }
}

#[cfg(test)]
mod tests {
    use super::rows;
    use crate::json::Value;
    use std::path::Path;

    #[test]
    fn every_fixture_is_a_case_and_streams_are_walked_too() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let count = |id: &str| rows(root, id).unwrap().len();
        let fixtures = std::fs::read_dir(root.join(super::FIXTURES))
            .unwrap()
            .count();
        assert_eq!(
            count("ass-parse-scripts") + count("ass-parse-chunks"),
            fixtures
        );
        let events = rows(root, "ass-parse-events").unwrap();
        let profiles: Vec<&str> = events
            .iter()
            .filter_map(|r| r.get("profile").and_then(Value::as_str))
            .collect();
        assert!(profiles.contains(&"script") && profiles.contains(&"stream"));
    }
}
