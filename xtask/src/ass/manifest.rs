//! The ASS parsing features' manifests, generated from the authored fixtures in
//! `harness/fixtures/ass`: every script (`.ass`, `.ssa`) read whole, every
//! stream (`.chunks`) run packet by packet, and the events of every fixture
//! with `;@time` walked at those times; the operations of [`PER_TARGET`] are
//! `ass-parse-host`'s instead. The work each case must fold is pinned in
//! `harness/features/<id>/pins.json`, libass's folds (see [`crate::lanes`]),
//! per target in `ass-parse-host`.

use std::path::Path;

use crate::json::Value;
use crate::lanes::Lane;

type R<T> = Result<T, String>;

/// The fixtures, relative to the root; the cases' files are in it.
pub const FIXTURES: &str = "harness/fixtures/ass";

/// How a case runs its fixture.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// The script or stream read (`ass-parse-scripts`, `ass-parse-chunks`).
    Read,
    /// Its events walked (`ass-parse-events`).
    Walk,
}

/// The operations whose libass result differs by target, each one fixture's,
/// in fixture order: those the reference flags (`harness/reference_ass.h`),
/// where a conversion from `double` to `int` is out of range or NaN (a drawing
/// coordinate in 26.6, `\pbo`, an animated `\clip` or `\iclip`, `\fade`'s
/// alpha, a `Banner` or `Scroll` delay) or a drawing coordinate lands on
/// `INT_MIN`; and values read through C `long` beyond `int` (an event time, a
/// ReadOrder). They are `ass-parse-host`'s cases, pinned per target; `cargo
/// xtask check-design` fails if one of them is the same on every target, and a
/// flagged case fails any other feature.
pub const PER_TARGET: [(&str, Operation); 15] = [
    ("drawings-clip-overflow.ass", Operation::Walk),
    ("drawings-infinite.ass", Operation::Walk),
    ("drawings-int-min-x.ass", Operation::Walk),
    ("drawings-int-min-y.ass", Operation::Walk),
    ("drawings-int-min.ass", Operation::Walk),
    ("drawings-nan.ass", Operation::Walk),
    ("effects-banner-delay-overflow.ass", Operation::Walk),
    ("effects-scroll-delay-overflow.ass", Operation::Walk),
    ("events-time-overflow.ass", Operation::Read),
    ("stream-read-order-overflow.chunks", Operation::Read),
    ("tags-clip-animation-overflow.ass", Operation::Walk),
    ("tags-fade-in-overflow.ass", Operation::Walk),
    ("tags-fade-out-overflow.ass", Operation::Walk),
    ("tags-iclip-animation-overflow.ass", Operation::Walk),
    ("tags-pbo-overflow.ass", Operation::Walk),
];

/// The files case rows are generated from, besides the pins and the fixtures,
/// which each row records by hash.
pub const ROW_SOURCES: [&str; 1] = ["xtask/src/ass/manifest.rs"];

/// The cases `cargo xtask check` mutates: the scripts with the most tag,
/// style and header quirks, a stream with every command, and the walks over
/// drawings, karaoke, animation and out-of-range coordinates and values.
pub const SMOKE_CASES: [(&str, &[&str]); 4] = [
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
    (
        "ass-parse-host",
        &[
            "drawings-infinite-ass",
            "drawings-int-min-ass",
            "events-time-overflow-ass",
        ],
    ),
];

pub const FEATURES: [&str; 4] = [
    "ass-parse-scripts",
    "ass-parse-chunks",
    "ass-parse-events",
    "ass-parse-host",
];

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
        "ass-parse-host" => {
            "The walks whose result C leaves to the host, as ass-parse-events walks them: drawing coordinates converted out of long's range or landing on INT_MIN; pinned from the AArch64 reference."
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
        let walk = || match kind {
            Kind::Script(_) => ("script", &["script", "events"][..]),
            Kind::Stream => ("stream", &["chunks", "events"][..]),
        };
        let read = || match kind {
            Kind::Script(dialect) => (dialect, &["script"][..]),
            Kind::Stream => ("stream", &["chunks"][..]),
        };
        let per_target = |operation| PER_TARGET.contains(&(name.as_str(), operation));
        let (profile, arguments): (&str, &[&str]) = match (id, kind) {
            ("ass-parse-scripts", Kind::Script(_)) | ("ass-parse-chunks", Kind::Stream)
                if !per_target(Operation::Read) =>
            {
                read()
            }
            ("ass-parse-events", _) if timed(&data) && !per_target(Operation::Walk) => walk(),
            ("ass-parse-host", _) if per_target(Operation::Read) => read(),
            ("ass-parse-host", _) if timed(&data) && per_target(Operation::Walk) => walk(),
            (
                "ass-parse-scripts" | "ass-parse-chunks" | "ass-parse-events" | "ass-parse-host",
                _,
            ) => continue,
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
    let files = match fixtures(root) {
        Ok(files) => files,
        Err(error) => return vec![error],
    };
    let mut errors: Vec<String> = files
        .iter()
        .filter(|(name, _)| kind(name).is_none())
        .map(|(name, _)| format!("{FIXTURES}/{name}: neither a script nor a stream"))
        .collect();
    for (name, operation) in PER_TARGET {
        let walked = operation == Operation::Walk;
        if !files
            .iter()
            .any(|(file, data)| file == name && (!walked || timed(data)))
        {
            errors.push(format!("{FIXTURES}/{name}: no fixture of that name to run"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::{Operation, rows};
    use crate::json::Value;
    use std::path::Path;

    #[test]
    fn every_operation_is_one_features_case() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let files = |id: &str| -> Vec<String> {
            rows(root, id)
                .unwrap()
                .iter()
                .filter_map(|r| r.get("file").and_then(Value::as_str).map(str::to_owned))
                .collect()
        };
        let fixtures = std::fs::read_dir(root.join(super::FIXTURES))
            .unwrap()
            .count();
        let reads = super::PER_TARGET
            .iter()
            .filter(|(_, operation)| *operation == Operation::Read)
            .count();
        assert_eq!(
            files("ass-parse-scripts").len() + files("ass-parse-chunks").len() + reads,
            fixtures
        );
        let profiles: Vec<String> = rows(root, "ass-parse-events")
            .unwrap()
            .iter()
            .filter_map(|r| r.get("profile").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert!(profiles.iter().any(|p| p == "script") && profiles.iter().any(|p| p == "stream"));
        // Each per-target operation is ass-parse-host's alone.
        let host = files("ass-parse-host");
        assert_eq!(host, super::PER_TARGET.map(|(name, _)| name));
        for (name, operation) in super::PER_TARGET {
            let other = match operation {
                Operation::Read => ["ass-parse-scripts", "ass-parse-chunks"],
                Operation::Walk => ["ass-parse-events", "ass-parse-events"],
            };
            assert!(other.iter().all(|id| !files(id).iter().any(|f| f == name)));
        }
    }
}
