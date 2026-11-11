//! Summary of a correctness run against the declared lane selection.
//!
//! A vector is out of set only when our decoder refused it with a typed
//! Unsupported error naming a feature the lane declares refused (the crate
//! READMEs and `harness/codecs/STATUS.md`), or when it carries no stream the
//! selected decoders take (DTS without a core substream). Everything else that
//! is not a pass at both levels is a failure and is listed with its numbers;
//! known dispositions are attached, never used to drop a row.

use std::fs;
use std::path::Path;

use crate::json::{self, Value};
use crate::python;

/// Summary schema written by [`summarize`].
pub const SUMMARY_SCHEMA: &str = "triergon.codecs.correctness-summary.v1";

fn declared_refusals(lane: &str) -> &'static [&'static str] {
    match lane {
        "h264" => &[
            "bit depth",
            "4:2:2",
            "4:4:4",
            "chroma",
            "lossless",
            "flexible macroblock ordering",
            "SP/SI",
            "data partitioning",
        ],
        "hevc" => &[
            "RExt",
            "range extension",
            "chroma_format",
            "bit depth",
            "persistent",
            "extended_precision",
            "high throughput",
            "cross-component",
            "implicit rdpcm",
            "explicit rdpcm",
            "transform_skip_rotation",
            "transform skip context",
            "cabac bypass alignment",
            "palette",
            "SCC",
        ],
        _ => &[],
    }
}

/// Recorded explanations, by vector name or lane; they never change a status.
fn disposition(key: &str) -> Option<&'static str> {
    Some(match key {
        "aac-sce-in-stereo.adts" => {
            "FFmpeg leaves the FR plane of SCE-only frames unwritten (stale pooled buffer); ours is silent there (harness/codecs/aac/STATUS.md)"
        }
        "ac3" | "eac3" => {
            "AC-3/E-AC-3 dither and SPX/AHT noise: FFmpeg random generator (harness/codecs/STATUS.md)"
        }
        "test8b43.264" | "DELTAQP_A_BRCM_4.bit" => {
            "RGBA near-tie in the reference horizontal filter coefficients (harness/codecs/rgba-contract.json)"
        }
        "PICSIZE_A_Bossen_1.bit" => {
            "RGBA near-tie in the odd-width output stage (harness/codecs/rgba-contract.json)"
        }
        "01 - changing samplerate.flac"
        | "02 - increasing number of channels.flac"
        | "03 - decreasing number of channels.flac"
        | "he_mode.bit" => {
            "the stream changes sample rate or channel count; the CLI resamples later frames to the first frame's format and its device path stops at the change, while each run decodes exactly (crates/codec-flac, crates/codec-mp3)"
        }
        "flac_3ch_odd_mask.flac" => {
            "FFmpeg refuses this channel mask's layout on the stereo device path; the decoder output matches exactly"
        }
        _ => return None,
    })
}

fn empty() -> Value {
    Value::Object(Vec::new())
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

fn text(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

/// The declared refusals a row's decoder errors name.
fn refusal(row: &Value, lane: &str) -> Vec<&'static str> {
    let empty = empty();
    let decoder = get(row, "decoder").unwrap_or(&empty);
    let errors = get(decoder, "ours_errors")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let tail = text(get(decoder, "ours_stderr_tail")).unwrap_or_default();
    let all = format!("{}{tail}", json::to_python(&errors, None)).to_lowercase();
    declared_refusals(lane)
        .iter()
        .copied()
        .filter(|k| all.contains(&k.to_lowercase()))
        .collect()
}

fn classify(row: &Value, lane: &str) -> String {
    let empty = empty();
    let decoder = get(row, "decoder").unwrap_or(&empty);
    let output = get(row, "output").unwrap_or(&empty);
    let ds = get(decoder, "status").or_else(|| get(row, "status"));
    let os = get(output, "status");
    let is = |v: Option<&Value>, s: &str| text(v) == Some(s);
    if lane == "aac-mov" {
        return if is(ds, "pass") { "pass" } else { "FAIL" }.into();
    }
    let os_none = matches!(os, None | Some(Value::Null));
    if is(ds, "ffmpeg-refused") && (is(os, "reference-produced-nothing") || os_none) {
        // ours_frames counts every non-error JSON line, the example's summary included.
        let typed = get(decoder, "ours_errors")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .any(|e| json::to_python(e, None).contains("nsupported"));
        let frames = get(decoder, "ours_frames").map_or(0.0, |v| v.as_f64().unwrap_or(0.0));
        return if typed || frames <= 1.0 {
            "refused-by-both".into()
        } else {
            "ffmpeg-refused-ours-decoded".into()
        };
    }
    let info = get(decoder, "ours_info").unwrap_or(&empty);
    let zero = |v: Option<&Value>| v.is_some_and(|v| python::equals_int(v, 0));
    if lane == "dts"
        && zero(get(info, "frames"))
        && zero(get(decoder, "ours_rc"))
        && !get(info, "errors").is_some_and(Value::truthy)
    {
        return "out-of-set (no core substream)".into();
    }
    if is(ds, "refused-unsupported") || is(ds, "fail") {
        let named = refusal(row, lane);
        if !named.is_empty() {
            return format!("out-of-set (typed refusal: {})", named.join(", "));
        }
    }
    if is(ds, "pass") && is(os, "pass") {
        return "pass".into();
    }
    "FAIL".into()
}

fn numbers(section: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|&k| {
                get(section, k)
                    .filter(|v| !matches!(v, Value::Null))
                    .map(|v| (k.to_owned(), v.clone()))
            })
            .collect(),
    )
}

/// `summarize RUN.jsonl OUT.json`: classify every row, write the summary and
/// print the per-lane counts and each failure.
pub fn summarize(run: &str, out: &Path) -> Result<(), String> {
    let source = fs::read_to_string(run).map_err(|e| format!("{run}: {e}"))?;
    let mut rows = Vec::new();
    for line in python::splitlines(&source) {
        rows.push(python::loads(line).ok_or_else(|| format!("{run}: not a JSON row"))?);
    }
    let mut per_lane: Vec<(String, Vec<(String, i64)>)> = Vec::new();
    let mut failures = Vec::new();
    let mut listed = Vec::new();
    for row in &rows {
        let lane = text(get(row, "lane")).ok_or("row without lane")?.to_owned();
        let c = classify(row, &lane);
        let class = c.split(" (").next().unwrap_or_default().to_owned();
        let at = per_lane
            .iter()
            .position(|(l, _)| *l == lane)
            .unwrap_or_else(|| {
                per_lane.push((lane.clone(), Vec::new()));
                per_lane.len() - 1
            });
        let counts = &mut per_lane[at].1;
        match counts.iter_mut().find(|(k, _)| *k == class) {
            Some(slot) => slot.1 += 1,
            None => counts.push((class, 1)),
        }
        let empty = empty();
        let decoder = get(row, "decoder").unwrap_or(&empty);
        let output = get(row, "output").unwrap_or(&empty);
        let status = |v: &Value| get(v, "status").cloned().unwrap_or(Value::Null);
        let name = get(row, "name").cloned().unwrap_or(Value::Null);
        if c == "FAIL" {
            let disposition = text(Some(&name))
                .and_then(disposition)
                .or_else(|| disposition(&lane));
            failures.push(Value::object([
                ("lane", Value::str(lane.as_str())),
                ("name", name.clone()),
                ("decoder", status(decoder)),
                ("output", status(output)),
                (
                    "decoder_numbers",
                    numbers(
                        decoder,
                        &[
                            "frames_ref",
                            "frames_ours",
                            "frames_equal",
                            "first_diff",
                            "worst_rms",
                            "worst_abs",
                            "ffmpeg_samples",
                            "ours_samples",
                        ],
                    ),
                ),
                (
                    "output_numbers",
                    numbers(
                        output,
                        &[
                            "pictures_ref",
                            "pictures_ours",
                            "differing_pictures",
                            "max_abs",
                            "first_diff",
                            "max_abs_lsb",
                            "differing_samples",
                            "samples_ours",
                            "samples_ref",
                        ],
                    ),
                ),
                ("disposition", disposition.map_or(Value::Null, Value::str)),
            ]));
        }
        listed.push(Value::object([
            ("lane", Value::str(lane.as_str())),
            ("name", name),
            ("sha256", get(row, "sha256").cloned().unwrap_or(Value::Null)),
            ("classification", Value::str(c)),
            ("decoder", status(decoder)),
            ("output", status(output)),
        ]));
    }
    per_lane.sort_by(|a, b| a.0.cmp(&b.0));
    let counts = |c: &[(String, i64)]| {
        Value::Object(c.iter().map(|(k, n)| (k.clone(), Value::int(*n))).collect())
    };
    let summary = Value::object([
        ("schema", Value::str(SUMMARY_SCHEMA)),
        ("run", Value::str(run)),
        ("vectors", Value::int(rows.len() as i64)),
        (
            "per_lane",
            Value::Object(
                per_lane
                    .iter()
                    .map(|(l, c)| (l.clone(), counts(c)))
                    .collect(),
            ),
        ),
        ("failures", Value::Array(failures.clone())),
        ("rows", Value::Array(listed)),
    ]);
    fs::write(out, json::to_python(&summary, Some(1)) + "\n")
        .map_err(|e| format!("{}: {e}", out.display()))?;
    for (lane, c) in &per_lane {
        println!("{lane} {}", python::repr(&counts(c)));
    }
    for f in &failures {
        let numbers = get(f, "output_numbers").cloned().unwrap_or(Value::Null);
        let worst = get(&numbers, "max_abs")
            .or_else(|| get(&numbers, "max_abs_lsb"))
            .cloned()
            .unwrap_or(Value::Null);
        let disposition: String = text(get(f, "disposition"))
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect();
        let field = |k: &str| python::str(get(f, k).unwrap_or(&Value::Null));
        println!(
            "FAIL {} {} {} {} {} {disposition}",
            field("lane"),
            field("name"),
            field("decoder"),
            field("output"),
            python::str(&worst),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Value {
        json::parse(text).unwrap()
    }

    #[test]
    fn classifies_refusals_and_failures() {
        assert_eq!(
            classify(&row(r#"{"decoder":{"status":"pass"}}"#), "aac-mov"),
            "pass"
        );
        assert_eq!(
            classify(
                &row(r#"{"decoder":{"status":"ffmpeg-refused"}}"#),
                "aac-mov"
            ),
            "FAIL"
        );
        assert_eq!(
            classify(
                &row(
                    r#"{"decoder": {"status": "ffmpeg-refused", "ours_errors": [], "ours_frames": 3}, "output": {"status": "reference-produced-nothing"}}"#
                ),
                "h264"
            ),
            "ffmpeg-refused-ours-decoded"
        );
        assert_eq!(
            classify(
                &row(
                    r#"{"decoder": {"status": "refused-unsupported", "ours_errors": [{"error": "Unsupported 4:2:2 Chroma"}]}, "output": {"status": "fail"}}"#
                ),
                "h264"
            ),
            "out-of-set (typed refusal: 4:2:2, chroma)"
        );
        assert_eq!(
            classify(
                &row(
                    r#"{"decoder": {"status": "fail", "ours_rc": 0, "ours_info": {"frames": 0}}, "output": {"status": "pass"}}"#
                ),
                "dts"
            ),
            "out-of-set (no core substream)"
        );
        assert_eq!(
            classify(&row(r#"{"status": "input-hash-mismatch"}"#), "aac"),
            "FAIL"
        );
    }
}
