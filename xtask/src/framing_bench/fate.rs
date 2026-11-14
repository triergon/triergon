//! The Annex B byte projections of the upstream AVC and HEVC framing cases.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::harness::worker::{DEFAULT_TIMEOUT, arg, run};
use crate::{checks::read_json, corpus::write_json, fetch, json::Value, sha256};

const MANIFEST: &str = "harness/features/packet-framing/fate.json";
const MAXIMUM_BYTES: usize = 16 << 20;
const USAGE: &str = "usage: cargo xtask framing fate --ffmpeg FILE --candidate FILE --output DIR\n\
                     Optional: --samples DIR (default: .research/fate-samples)";

struct Options {
    ffmpeg: PathBuf,
    candidate: PathBuf,
    output: PathBuf,
    samples: PathBuf,
}

impl Options {
    fn read(root: &Path, args: &[String]) -> Result<Self, String> {
        let mut ffmpeg = None;
        let mut candidate = None;
        let mut output = None;
        let mut samples = None;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--ffmpeg" => &mut ffmpeg,
                "--candidate" => &mut candidate,
                "--output" => &mut output,
                "--samples" => &mut samples,
                _ => return Err(USAGE.into()),
            };
            if slot.is_some() {
                return Err(format!("{flag} specified twice"));
            }
            *slot = Some(PathBuf::from(args.next().ok_or(USAGE)?));
        }
        Ok(Self {
            ffmpeg: ffmpeg.ok_or(USAGE)?,
            candidate: candidate.ok_or(USAGE)?,
            output: output.ok_or(USAGE)?,
            samples: samples.unwrap_or_else(|| root.join(".research/fate-samples")),
        })
    }
}

#[derive(Clone, Copy)]
enum Projection {
    Avc,
    AvcExplicit,
    AvcMetadata,
    Hevc,
}

impl Projection {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "avc" => Ok(Self::Avc),
            "avc-explicit" => Ok(Self::AvcExplicit),
            "avc-metadata" => Ok(Self::AvcMetadata),
            "hevc" => Ok(Self::Hevc),
            _ => Err(format!("unknown Annex B projection {value}")),
        }
    }

    fn reference(self, ffmpeg: &Path, input: &Path, output: &Path) -> Result<(), String> {
        let mut command = ffmpeg_args(ffmpeg);
        command.extend([arg("-err_detect"), arg("ignore_err"), arg("-i")]);
        command.push(input.into());
        command.extend([arg("-map"), arg("0:v:0"), arg("-c"), arg("copy")]);
        if let Some(filter) = match self {
            Self::Avc | Self::Hevc => None,
            Self::AvcExplicit => Some("h264_mp4toannexb"),
            Self::AvcMetadata => Some("h264_mp4toannexb,h264_metadata"),
        } {
            command.extend([arg("-bsf:v"), arg(filter)]);
        }
        let format = if matches!(self, Self::Hevc) {
            "hevc"
        } else {
            "h264"
        };
        command.extend([arg("-f"), arg(format), arg("-fs"), arg(MAXIMUM_BYTES)]);
        command.push(output.into());
        command_output(&command).map(|_| ())
    }
}

struct Case<'a> {
    id: &'a str,
    sample: &'a str,
    source: &'a str,
    sha256: &'a str,
    expected: &'a str,
    projection: Projection,
}

impl<'a> Case<'a> {
    fn read(row: &'a Value, pins: &'a [Value]) -> Result<Self, String> {
        let id = field(row, "id")?;
        let sample = field(row, "sample")?;
        if !id.starts_with("fate-")
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || sample.is_empty()
            || !Path::new(sample)
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(format!("invalid Annex B case path {id}: {sample}"));
        }
        let pin = pins
            .iter()
            .find(|pin| pin.get("path").and_then(Value::as_str) == Some(sample))
            .ok_or_else(|| format!("{sample}: no FATE sample pin"))?;
        Ok(Self {
            id,
            sample,
            source: field(pin, "source")?,
            sha256: field(pin, "sha256")?,
            expected: field(row, "annex_b_md5")?,
            projection: Projection::parse(field(row, "projection")?)?,
        })
    }

    fn sample(&self, directory: &Path) -> Result<PathBuf, String> {
        let path = directory.join(self.sample);
        if path.is_file() {
            if sha256::file_hex(&path).map_err(|e| e.to_string())? != self.sha256 {
                return Err(format!("{}: cached sample hash differs", path.display()));
            }
        } else {
            let bytes = fetch::pinned(
                &fetch::Network,
                &[fetch::Source::Url(self.source)],
                self.sha256,
                MAXIMUM_BYTES,
                "triergon-fate",
            )?;
            fetch::publish(&path, &bytes)?;
        }
        Ok(path)
    }

    fn compare(&self, options: &Options, directory: &Path) -> Result<String, String> {
        let sample = self.sample(&options.samples)?;
        let input = if matches!(self.projection, Projection::Hevc) {
            let mov = directory.join("input.mov");
            let mut command = ffmpeg_args(&options.ffmpeg);
            command.extend([arg("-i"), sample.into_os_string()]);
            command.extend([arg("-c"), arg("copy"), arg("-flags"), arg("+bitexact")]);
            command.push(mov.clone().into_os_string());
            command_output(&command)?;
            mov
        } else {
            sample
        };
        let reference = directory.join("reference.annexb");
        self.projection
            .reference(&options.ffmpeg, &input, &reference)?;
        let digest = md5(&reference)?;
        if digest != self.expected {
            return Err(format!(
                "reference MD5 {digest} differs from {}",
                self.expected
            ));
        }
        let candidate = directory.join("candidate.annexb");
        command_output(&[
            options.candidate.clone().into_os_string(),
            input.into_os_string(),
            candidate.clone().into_os_string(),
        ])?;
        let expected = sha256::file_hex(&reference).map_err(|e| e.to_string())?;
        let actual = sha256::file_hex(&candidate).map_err(|e| e.to_string())?;
        if expected != actual {
            return Err(format!("Annex B SHA-256 {actual} differs from {expected}"));
        }
        Ok(actual)
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {name}"))
}

fn ffmpeg_args(ffmpeg: &Path) -> Vec<OsString> {
    vec![
        ffmpeg.into(),
        arg("-nostdin"),
        arg("-v"),
        arg("error"),
        arg("-n"),
    ]
}

fn command_output(command: &[OsString]) -> Result<String, String> {
    let program = command[0].to_string_lossy();
    let result =
        run(command, &[], DEFAULT_TIMEOUT)?.ok_or_else(|| format!("{program}: timeout"))?;
    if result.code != 0 {
        return Err(format!(
            "{program}: exit {}: {}",
            result.code,
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    String::from_utf8(result.stdout).map_err(|error| format!("{program}: {error}"))
}

fn md5(path: &Path) -> Result<String, String> {
    let mut command = if cfg!(target_os = "macos") {
        vec![arg("md5"), arg("-q")]
    } else {
        vec![arg("md5sum"), arg("--")]
    };
    command.push(path.into());
    command_output(&command)?
        .split_ascii_whitespace()
        .next()
        .filter(|value| value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or_else(|| format!("{}: invalid upstream checksum output", path.display()))
}

pub(super) fn compare(root: &Path, args: &[String]) -> Result<i32, String> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(0);
    }
    let options = Options::read(root, args)?;
    let manifest = read_json(&root.join(MANIFEST))?;
    let pins = read_json(&root.join("harness/fate/samples.json"))?;
    let pins = pins
        .get("samples")
        .and_then(Value::as_array)
        .ok_or("no FATE sample pins")?;
    let rows = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("no Annex B cases")?;
    let cases = rows
        .iter()
        .map(|row| Case::read(row, pins))
        .collect::<Result<Vec<_>, _>>()?;
    let ids: BTreeSet<_> = cases.iter().map(|case| case.id).collect();
    if ids.is_empty() || ids.len() != rows.len() {
        return Err("Annex B cases must have unique IDs".into());
    }
    let version = command_output(&[options.ffmpeg.clone().into_os_string(), arg("-version")])?;
    let version = version.lines().next().ok_or("missing FFmpeg version")?;
    let reference_sha = sha256::file_hex(&options.ffmpeg).map_err(|e| e.to_string())?;
    let candidate_sha = sha256::file_hex(&options.candidate).map_err(|e| e.to_string())?;
    fs::create_dir(&options.output).map_err(|e| format!("{}: {e}", options.output.display()))?;
    let mut passed = 0;
    let mut comparisons = Vec::with_capacity(cases.len());
    for case in &cases {
        let directory = options.output.join(case.id);
        fs::create_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        let (status, digest, error) = match case.compare(&options, &directory) {
            Ok(sha) => {
                passed += 1;
                ("pass", Value::str(sha), Value::Null)
            }
            Err(error) => ("fail", Value::Null, Value::str(error)),
        };
        println!("{} {status}", case.id);
        comparisons.push(Value::object([
            ("id", Value::str(case.id)),
            ("sample_sha256", Value::str(case.sha256)),
            ("expected_md5", Value::str(case.expected)),
            ("output_sha256", digest),
            ("status", Value::str(status)),
            ("error", error),
        ]));
    }
    write_json(
        &options.output.join("report.json"),
        &Value::object([
            ("operation", Value::str("annex-b-byte-projection")),
            (
                "upstream_revision",
                Value::str(field(&manifest, "revision")?),
            ),
            ("reference_version", Value::str(version)),
            ("reference_sha256", Value::str(reference_sha)),
            ("candidate_sha256", Value::str(candidate_sha)),
            ("cases", Value::Array(comparisons)),
        ]),
    )?;
    println!("{passed}/{} Annex B byte projections match", cases.len());
    Ok(i32::from(passed != cases.len()))
}
