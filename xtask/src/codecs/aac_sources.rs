//! Author the pinned audio-only sources for fixed AAC-LC decoder comparisons.

use std::fs;
use std::path::Path;

use crate::{checks, corpus, json::Value, prepare, process};

use super::correctness::file_sha;

const LAYOUTS: &[(u8, u8, &str)] = &[
    (1, 1, "mono"),
    (2, 2, "stereo"),
    (3, 3, "3.0"),
    (4, 4, "4.0"),
    (5, 5, "5.0"),
    (6, 6, "5.1"),
    (7, 8, "7.1"),
    (11, 7, "6.1(back)"),
    (12, 8, "7.1"),
    (14, 8, "5.1.2(back)"),
];

const RATES: [u32; 12] = [
    7_350, 8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 64_000, 88_200, 96_000,
];

pub(super) fn run(root: &Path, out: &Path) -> Result<(), String> {
    let reference = root.join(".research/reference-decode");
    let build = checks::read_json(&reference.join("build.json"))?;
    let ffmpeg = reference.join("install/bin/ffmpeg");
    let hash = file_sha(&ffmpeg)?;
    if build.get("profile").and_then(Value::as_str) != Some("decode")
        || build.get("release").and_then(Value::as_str) != Some(prepare::RELEASE)
        || build.get("archive_sha256").and_then(Value::as_str) != Some(prepare::RELEASE_SHA256)
        || build
            .get("installed")
            .and_then(|v| v.get("install/bin/ffmpeg"))
            .and_then(Value::as_str)
            != Some(hash.as_str())
    {
        return Err("prepare the pinned FFmpeg decode profile before authoring AAC sources".into());
    }
    fs::create_dir(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let inputs = out.join("inputs");
    fs::create_dir(&inputs).map_err(|error| format!("{}: {error}", inputs.display()))?;
    let layouts = LAYOUTS.iter().map(|&(configuration, channels, layout)| {
        (
            format!("aac-layout-{configuration}"),
            48_000,
            configuration,
            channels,
            layout,
        )
    });
    let rates = RATES
        .into_iter()
        .map(|rate| (format!("aac-rate-{rate}"), rate, 2, 2, "stereo"));
    let mut vectors = Vec::new();
    for (id, rate, configuration, channels, layout) in layouts.chain(rates) {
        let signal = (0..u32::from(channels))
            .map(|channel| {
                let base = 79 + u32::from(configuration) * 7 + channel * 53;
                format!(
                    "0.13*sin(2*PI*{base}*t)+0.035*sin(2*PI*{}*t)*sin(2*PI*13*t)",
                    base * 3
                )
            })
            .collect::<Vec<_>>()
            .join("|");
        let name = format!("{id}.mov");
        let path = inputs.join(&name);
        let arguments = [
            ffmpeg.display().to_string(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("aevalsrc={signal}:s={rate}:c={layout}"),
            "-frames:a".into(),
            "96".into(),
            "-c:a".into(),
            "aac".into(),
            "-threads:a".into(),
            "1".into(),
            "-flags:a".into(),
            "+bitexact".into(),
            "-fflags".into(),
            "+bitexact".into(),
            "-b:a".into(),
            (32_000 * u32::from(channels)).to_string(),
            path.display().to_string(),
        ];
        process::check_output(&arguments)?;
        vectors.push(Value::object([
            ("name", Value::str(name)),
            ("sha256", Value::str(file_sha(&path)?)),
            ("sample_rate", Value::int(rate)),
            ("channels", Value::int(channels)),
            ("configuration", Value::int(configuration)),
        ]));
    }
    let manifest = Value::object([
        ("license", Value::str("MIT OR Apache-2.0")),
        ("vectors", Value::Array(vectors)),
    ]);
    corpus::write_json(&out.join("sources.json"), &manifest)
}
