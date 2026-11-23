//! Pins of the vectors the codec manifests list as differing.
//!
//! A vector a codec manifest lists under `open_deviations` or `open_cases` differs
//! from the reference in the way its entry states. [`PINS`] records, per manifest,
//! lane and vector, what each side produced when the difference was recorded, and
//! the summary fails a listed vector whose row no longer shows exactly that: a
//! listed vector that only has to fail would keep passing while either side moved
//! to another wrong answer.
//!
//! A pin has three parts taken from the graded row, each a flat object of dotted
//! paths: `candidate` (our counts, exit status, rates, layouts, errors and clocks),
//! `reference` (the reference's) and `comparison` (the verdicts and the first
//! difference). They hold integers, booleans and short strings; a long string or an
//! array is pinned by its length and the SHA-256 of its canonical JSON. These parts
//! are the same on every target. Floating-point statistics are left out, and what
//! depends on a target's arithmetic (PCM digests, LSB differences) is pinned per gate
//! target under `exact`; a target without a recorded `exact` checks the other parts
//! and says so.
//!
//! The file is written by `cargo xtask codecs summarize RUN OUT --record-pins` and
//! never by hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::json::{self, Value};
use crate::sha256;

/// The recorded pins, relative to the repository root.
pub(super) const PINS: &str = "harness/codecs/open-pins.json";
const SCHEMA: &str = "triergon.codecs.open-pins.v1";
const NOTE: &str = "What each side produced for every vector a codec manifest lists under open_deviations or open_cases. Written by `cargo xtask codecs summarize RUN OUT --record-pins`, never by hand.";

/// A string longer than this is pinned by its digest.
const INLINE_TEXT: usize = 96;

/// One row's pinned facts.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Observed {
    candidate: BTreeMap<String, Value>,
    reference: BTreeMap<String, Value>,
    comparison: BTreeMap<String, Value>,
    exact: BTreeMap<String, Value>,
}

#[derive(Clone, Copy)]
enum Side {
    Candidate,
    Reference,
    Comparison,
}

/// Which side a path of a graded row describes, by the names the graders use.
fn side(path: &str) -> Side {
    let named = |test: fn(&str) -> bool| path.split(['.', '_']).any(test);
    if named(|word| word == "ours") {
        Side::Candidate
    } else if named(|word| matches!(word, "ffmpeg" | "reference" | "ref")) {
        Side::Reference
    } else {
        Side::Comparison
    }
}

/// Diagnostics that carry addresses or temporary paths, and float statistics.
fn unpinned(key: &str) -> bool {
    key.contains("stderr")
        || key.ends_with("_probe_errors")
        || matches!(
            key,
            "message" | "argv" | "stats" | "worst_rms" | "worst_abs" | "float_reference"
        )
}

/// Facts that follow a target's floating-point arithmetic.
fn target_exact(key: &str) -> bool {
    key.contains("sha256") || matches!(key, "max_abs_lsb" | "differing_samples" | "exact")
}

fn has_float(value: &Value) -> bool {
    match value {
        Value::Number(number) => !number.is_integer(),
        Value::Array(items) => items.iter().any(has_float),
        Value::Object(members) => members.iter().any(|(_, value)| has_float(value)),
        _ => false,
    }
}

fn digest(length: usize, value: &Value) -> Value {
    Value::object([
        ("length", Value::int(length as u64)),
        (
            "sha256",
            Value::str(sha256::hex(json::canonical(value).as_bytes())),
        ),
    ])
}

impl Observed {
    /// The pinned facts of a graded row's `decoder` and `output` sections, or of its
    /// own `status` where the comparison never started.
    pub(super) fn of(row: &Value) -> Observed {
        let mut observed = Observed::default();
        for section in ["decoder", "output"] {
            if let Some(value) = row.get(section) {
                observed.walk(section, section, value);
            }
        }
        if let Some(status) = row.get("status") {
            observed.walk("status", "status", status);
        }
        observed
    }

    fn walk(&mut self, path: &str, key: &str, value: &Value) {
        if unpinned(key) || has_float(value) && !matches!(value, Value::Object(_)) {
            return;
        }
        let pinned = match value {
            Value::Object(members) => {
                for (key, value) in members {
                    self.walk(&format!("{path}.{key}"), key, value);
                }
                return;
            }
            Value::Array(items) => digest(items.len(), value),
            Value::String(text) if text.len() > INLINE_TEXT => digest(text.len(), value),
            scalar => scalar.clone(),
        };
        let part = if target_exact(key) {
            &mut self.exact
        } else {
            match side(path) {
                Side::Candidate => &mut self.candidate,
                Side::Reference => &mut self.reference,
                Side::Comparison => &mut self.comparison,
            }
        };
        part.insert(path.to_owned(), pinned);
    }

    fn stable(&self) -> [(&'static str, &BTreeMap<String, Value>); 3] {
        [
            ("candidate", &self.candidate),
            ("reference", &self.reference),
            ("comparison", &self.comparison),
        ]
    }
}

fn object(part: &BTreeMap<String, Value>) -> Value {
    Value::Object(
        part.iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

fn part(pin: &Value, key: &str) -> BTreeMap<String, Value> {
    match pin.get(key) {
        Some(Value::Object(members)) => members.iter().cloned().collect(),
        _ => BTreeMap::new(),
    }
}

/// Every path whose pinned and observed values differ, as `path: pinned X, observed Y`.
fn differences(
    name: &str,
    pinned: &BTreeMap<String, Value>,
    observed: &BTreeMap<String, Value>,
) -> Vec<String> {
    let show = |value: Option<&Value>| value.map_or("nothing".to_owned(), json::observations);
    pinned
        .keys()
        .chain(observed.keys().filter(|key| !pinned.contains_key(*key)))
        .filter(|key| pinned.get(*key) != observed.get(*key))
        .map(|key| {
            format!(
                "{name} {key}: pinned {}, observed {}",
                show(pinned.get(key)),
                show(observed.get(key))
            )
        })
        .collect()
}

/// The pins file: manifest path, then lane, then vector name.
pub(super) struct Pins {
    path: PathBuf,
    manifests: BTreeMap<String, BTreeMap<String, BTreeMap<String, Value>>>,
}

impl Pins {
    /// The recorded pins below `root`; none when the file does not exist yet.
    pub(super) fn load(root: &Path) -> Result<Pins, String> {
        let path = root.join(PINS);
        let mut manifests = BTreeMap::new();
        if path.is_file() {
            let document = json::read(&path)?;
            let Some(Value::Object(recorded)) = document.get("pins") else {
                return Err(format!("{}: no pins", path.display()));
            };
            for (manifest, lanes) in recorded {
                let Value::Object(lanes) = lanes else {
                    return Err(format!("{}: {manifest} is not an object", path.display()));
                };
                let lanes: BTreeMap<String, BTreeMap<String, Value>> = lanes
                    .iter()
                    .map(|(lane, vectors)| match vectors {
                        Value::Object(vectors) => {
                            Ok((lane.clone(), vectors.iter().cloned().collect()))
                        }
                        _ => Err(format!(
                            "{}: {manifest} {lane} is not an object",
                            path.display()
                        )),
                    })
                    .collect::<Result<_, String>>()?;
                manifests.insert(manifest.clone(), lanes);
            }
        }
        Ok(Pins { path, manifests })
    }

    /// Every pinned (manifest, lane, vector).
    pub(super) fn recorded(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.manifests.iter().flat_map(|(manifest, lanes)| {
            lanes.iter().flat_map(move |(lane, vectors)| {
                vectors
                    .keys()
                    .map(move |name| (manifest.as_str(), lane.as_str(), name.as_str()))
            })
        })
    }

    /// Compares a listed vector's row with its pin. `Ok` carries a note when the
    /// target's exact outputs are not pinned; `Err` lists every difference.
    pub(super) fn check(
        &self,
        manifest: &str,
        lane: &str,
        name: &str,
        row: &Value,
        target: Option<&str>,
    ) -> Result<Option<String>, Vec<String>> {
        let Some(pin) = self
            .manifests
            .get(manifest)
            .and_then(|lanes| lanes.get(lane))
            .and_then(|vectors| vectors.get(name))
        else {
            return Err(vec![format!(
                "{name}: the listed vector has no pin in {PINS}"
            )]);
        };
        let observed = Observed::of(row);
        let mut changed: Vec<String> = observed
            .stable()
            .into_iter()
            .flat_map(|(key, observed)| differences(name, &part(pin, key), observed))
            .collect();
        let exact = target.and_then(|target| pin.get("exact")?.get(target));
        if let Some(exact) = exact {
            let pinned = match exact {
                Value::Object(members) => members.iter().cloned().collect(),
                _ => BTreeMap::new(),
            };
            changed.extend(differences(name, &pinned, &observed.exact));
        }
        if !changed.is_empty() {
            return Err(changed);
        }
        Ok((exact.is_none() && !observed.exact.is_empty()).then(|| {
            format!(
                "{name}: digests and LSB differences are not pinned on {}",
                target.unwrap_or("this platform")
            )
        }))
    }

    /// Records a listed vector's row. Another target's exact outputs are kept only
    /// while the parts every target shares are unchanged.
    pub(super) fn record(
        &mut self,
        manifest: &str,
        lane: &str,
        name: &str,
        row: &Value,
        target: Option<&str>,
    ) {
        let observed = Observed::of(row);
        let vectors = self
            .manifests
            .entry(manifest.to_owned())
            .or_default()
            .entry(lane.to_owned())
            .or_default();
        let mut exact = match vectors.get(name) {
            Some(pin)
                if observed
                    .stable()
                    .into_iter()
                    .all(|(key, observed)| &part(pin, key) == observed) =>
            {
                part(pin, "exact")
            }
            _ => BTreeMap::new(),
        };
        if let Some(target) = target {
            if observed.exact.is_empty() {
                exact.remove(target);
            } else {
                exact.insert(target.to_owned(), object(&observed.exact));
            }
        }
        let mut pin: Vec<(String, Value)> = observed
            .stable()
            .into_iter()
            .map(|(key, part)| (key.to_owned(), object(part)))
            .collect();
        if !exact.is_empty() {
            pin.push(("exact".to_owned(), object(&exact)));
        }
        vectors.insert(name.to_owned(), Value::Object(pin));
    }

    /// Drops the pins of vectors `listed` no longer names.
    pub(super) fn retain(&mut self, listed: impl Fn(&str, &str) -> bool) {
        for (manifest, lanes) in &mut self.manifests {
            for vectors in lanes.values_mut() {
                vectors.retain(|name, _| listed(manifest, name));
            }
            lanes.retain(|_, vectors| !vectors.is_empty());
        }
        self.manifests.retain(|_, lanes| !lanes.is_empty());
    }

    pub(super) fn write(&self) -> Result<(), String> {
        let pins = Value::Object(
            self.manifests
                .iter()
                .map(|(manifest, lanes)| {
                    let lanes = lanes
                        .iter()
                        .map(|(lane, vectors)| (lane.clone(), object(vectors)))
                        .collect();
                    (manifest.clone(), Value::Object(lanes))
                })
                .collect(),
        );
        let document = Value::object([
            ("schema", Value::str(SCHEMA)),
            ("note", Value::str(NOTE)),
            ("pins", pins),
        ]);
        std::fs::write(&self.path, json::to_python(&document, Some(1)) + "\n")
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Value {
        json::parse(text).unwrap()
    }

    const ROW: &str = r#"{"lane":"session-flac","name":"n","open_case":true,
        "decoder":{"status":"fail","ours_rc":0,"ffmpeg_rc":0,"ours_samples":10,"ffmpeg_samples":12,
        "worst_abs":0.5,"stats":{"rms":[0.1]},"ours_stderr_tail":"0xdead","ffmpeg_frames":[{"nb_samples":4}],
        "ours_info":{"frames":3,"last_error":"CrcMismatch"},"ours_pcm_sha256":"aa","ffmpeg_pcm_sha256":"bb",
        "timeline":{"status":"fail","first_difference":2,"ours_at_difference":{"pts_ns":5},"reference_at_difference":{"pts_ns":7}}},
        "output":{"status":"pass","samples_ours":10,"samples_ref":10,"max_abs_lsb":1}}"#;

    #[test]
    fn a_row_splits_into_sides_and_leaves_out_floats_and_diagnostics() {
        let observed = Observed::of(&row(ROW));
        let keys = |part: &BTreeMap<String, Value>| part.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            keys(&observed.candidate),
            [
                "decoder.ours_info.frames",
                "decoder.ours_info.last_error",
                "decoder.ours_rc",
                "decoder.ours_samples",
                "decoder.timeline.ours_at_difference.pts_ns",
                "output.samples_ours",
            ]
        );
        assert_eq!(
            keys(&observed.reference),
            [
                "decoder.ffmpeg_frames",
                "decoder.ffmpeg_rc",
                "decoder.ffmpeg_samples",
                "decoder.timeline.reference_at_difference.pts_ns",
                "output.samples_ref",
            ]
        );
        assert_eq!(
            keys(&observed.comparison),
            [
                "decoder.status",
                "decoder.timeline.first_difference",
                "decoder.timeline.status",
                "output.status",
            ]
        );
        assert_eq!(
            keys(&observed.exact),
            [
                "decoder.ffmpeg_pcm_sha256",
                "decoder.ours_pcm_sha256",
                "output.max_abs_lsb",
            ]
        );
        assert_eq!(
            observed.reference["decoder.ffmpeg_frames"].get("length"),
            Some(&Value::int(1))
        );
    }

    #[test]
    fn a_pin_fails_when_either_side_or_the_target_digest_moves() {
        let directory = crate::files::scratch("deviation-pins");
        std::fs::create_dir_all(directory.join("harness/codecs")).unwrap();
        let mut pins = Pins::load(&directory).unwrap();
        let recorded = row(ROW);
        let unpinned = pins
            .check("m", "session-flac", "n", &recorded, Some("t"))
            .unwrap_err();
        assert!(unpinned[0].contains("no pin"), "{unpinned:?}");
        pins.record("m", "session-flac", "n", &recorded, Some("t"));
        pins.write().unwrap();
        let pins = Pins::load(&directory).unwrap();
        assert_eq!(
            pins.check("m", "session-flac", "n", &recorded, Some("t")),
            Ok(None)
        );
        let note = pins
            .check("m", "session-flac", "n", &recorded, Some("other"))
            .unwrap();
        assert!(note.unwrap().contains("not pinned on other"));
        for (from, to, path) in [
            (
                r#""ours_samples":10"#,
                r#""ours_samples":11"#,
                "decoder.ours_samples",
            ),
            (
                r#""ffmpeg_samples":12"#,
                r#""ffmpeg_samples":13"#,
                "decoder.ffmpeg_samples",
            ),
            (
                r#""last_error":"CrcMismatch""#,
                r#""last_error":"Invalid""#,
                "decoder.ours_info.last_error",
            ),
            (
                r#""ffmpeg_pcm_sha256":"bb""#,
                r#""ffmpeg_pcm_sha256":"cc""#,
                "decoder.ffmpeg_pcm_sha256",
            ),
            (
                r#""nb_samples":4"#,
                r#""nb_samples":5"#,
                "decoder.ffmpeg_frames",
            ),
        ] {
            let moved = row(&ROW.replace(from, to));
            let changed = pins
                .check("m", "session-flac", "n", &moved, Some("t"))
                .unwrap_err();
            assert!(
                changed.len() == 1 && changed[0].contains(path),
                "{changed:?}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recording_keeps_another_targets_digests_only_while_the_shared_parts_hold() {
        let directory = crate::files::scratch("deviation-pins-targets");
        std::fs::create_dir_all(directory.join("harness/codecs")).unwrap();
        let mut pins = Pins::load(&directory).unwrap();
        let recorded = row(ROW);
        pins.record("m", "l", "n", &recorded, Some("a"));
        pins.record("m", "l", "n", &recorded, Some("b"));
        assert_eq!(pins.check("m", "l", "n", &recorded, Some("a")), Ok(None));
        let moved = row(&ROW.replace(r#""ours_samples":10"#, r#""ours_samples":11"#));
        pins.record("m", "l", "n", &moved, Some("b"));
        assert!(
            pins.check("m", "l", "n", &moved, Some("a"))
                .unwrap()
                .is_some()
        );
        pins.retain(|_, name| name != "n");
        assert_eq!(pins.recorded().count(), 0);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
