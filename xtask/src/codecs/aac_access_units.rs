//! Prepare the original AAC packets and compare every decoded PCM plane.

use std::fs;
use std::path::Path;

use crate::{checks, corpus, json, process, python, sha256};
use json::Value;

use super::correctness::{Rules, channel_stats};

const OPERATION: &str = "audio-aac-access-units";
const PADDING: [u8; 64] = [0; 64];
const PLANE_BYTES: usize = 1024 * size_of::<f32>();
const MAX_PACKET: usize = 64 << 10;

fn string<'a>(object: &'a Value, key: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn number(object: &Value, key: &str) -> Result<u32, String> {
    object
        .get(key)
        .and_then(python::int)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("invalid {key}"))
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn command(program: &Path, args: &[&str]) -> Result<Value, String> {
    let mut argv = vec![program.display().to_string()];
    argv.extend(args.iter().map(|s| (*s).to_owned()));
    let output = process::check_output(&argv)?;
    json::parse(&output).map_err(|error| error.to_string())
}

fn audio_specific_config(dump: &str) -> Result<Vec<u8>, String> {
    let mut config = Vec::new();
    for line in dump.lines().filter(|line| !line.is_empty()) {
        let (_, text) = line.split_once(':').ok_or("invalid configuration dump")?;
        let (hex, _) = text
            .trim_start()
            .split_once("  ")
            .ok_or("invalid configuration hex")?;
        for word in hex.split_ascii_whitespace() {
            if !word.len().is_multiple_of(2) || !word.is_ascii() {
                return Err("invalid configuration hex word".into());
            }
            for at in (0..word.len()).step_by(2) {
                config.push(
                    u8::from_str_radix(&word[at..at + 2], 16).map_err(|error| error.to_string())?,
                );
            }
        }
    }
    if !(1..=MAX_PACKET).contains(&config.len()) {
        return Err("AAC configuration bounds".into());
    }
    Ok(config)
}

fn access_units(source: &[u8], probe: &Value) -> Result<(Vec<u8>, u32, u32, u32), String> {
    let Some([stream]) = probe.get("streams").and_then(Value::as_array) else {
        return Err("expected one AAC stream".into());
    };
    if string(stream, "codec_name")? != "aac" || string(stream, "profile")? != "LC" {
        return Err("expected an AAC-LC stream".into());
    }
    let rate = number(stream, "sample_rate")?;
    let channels = number(stream, "channels")?;
    let packets = probe
        .get("packets")
        .and_then(Value::as_array)
        .ok_or("missing AAC packets")?;
    let count = u32::try_from(packets.len()).map_err(|error| error.to_string())?;
    if rate == 0 || !(1..=8).contains(&channels) || count == 0 {
        return Err("AAC stream shape".into());
    }
    let config = audio_specific_config(string(stream, "extradata")?)?;
    let mut input = Vec::new();
    for field in [rate, channels, count, config.len() as u32] {
        input.extend_from_slice(&field.to_le_bytes());
    }
    input.extend_from_slice(&config);
    for packet in packets {
        let start = number(packet, "pos")? as usize;
        let size = number(packet, "size")? as usize;
        let bytes = start
            .checked_add(size)
            .and_then(|end| source.get(start..end))
            .filter(|bytes| (1..=MAX_PACKET).contains(&bytes.len()))
            .ok_or("AAC packet outside source")?;
        input.extend_from_slice(&(size as u32).to_le_bytes());
        input.extend_from_slice(bytes);
        input.extend_from_slice(&PADDING);
    }
    Ok((input, rate, channels, count))
}

fn compare_pcm(
    candidate: &[u8],
    reference: &[u8],
    channels: usize,
    packets: usize,
    rules: &Rules,
) -> Result<(bool, Value), String> {
    let frame_bytes = channels * PLANE_BYTES;
    let expected = frame_bytes
        .checked_mul(packets)
        .ok_or("AAC output size overflow")?;
    if candidate.len() != expected || reference.len() != expected {
        return Err(format!(
            "AAC output bytes: expected {expected}, got {} and {}",
            candidate.len(),
            reference.len()
        ));
    }
    let collect_plane = |pcm: &[u8], channel: usize| {
        let start = channel * PLANE_BYTES;
        pcm.chunks_exact(frame_bytes)
            .flat_map(|frame| &frame[start..start + PLANE_BYTES])
            .copied()
            .collect::<Vec<_>>()
    };
    let mut pass = true;
    let mut rows = Vec::new();
    for channel in 0..channels {
        let (rms, abs) = channel_stats(
            &collect_plane(candidate, channel),
            &collect_plane(reference, channel),
            1,
        )?
        .ok_or("non-finite AAC PCM")?;
        pass &= rms[0] <= rules.rms && abs[0] <= rules.abs;
        rows.push(Value::object([
            ("rms", Value::float(rms[0])),
            ("abs", Value::float(abs[0])),
        ]));
    }
    Ok((pass, Value::Array(rows)))
}

pub fn run(root: &Path, out: &Path, record: bool) -> Result<(), String> {
    let manifest = checks::read_json(&root.join("harness/codecs/aac/mov-manifest.json"))?;
    let vectors = manifest
        .get("vectors")
        .and_then(Value::as_array)
        .filter(|vectors| !vectors.is_empty())
        .ok_or("missing AAC vectors")?;
    fs::create_dir(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let mut workloads = Vec::new();
    let mut results = Vec::new();
    let rules = Rules::load(root)?;
    let ffprobe = root.join(".research/reference-decode/install/bin/ffprobe");
    let candidate = root.join("target/release/codec-worker");
    let reference = root.join("target/harness/ffmpeg-decode-worker");
    let mut accepted = true;
    for vector in vectors {
        let source_path = root.join(string(vector, "path")?);
        let source = read(&source_path)?;
        let source_sha = string(vector, "sha256")?;
        if sha256::hex(&source) != source_sha {
            return Err(format!(
                "{}: AAC source hash differs",
                source_path.display()
            ));
        }
        let name = string(vector, "name")?;
        let id = name
            .strip_suffix(".mov")
            .filter(|id| !id.contains('/'))
            .ok_or("invalid AAC source name")?;
        let probe = command(
            &ffprobe,
            &[
                "-v",
                "error",
                "-ignore_editlist",
                "1",
                "-fflags",
                "+noparse+nofillin",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=codec_name,profile,channels,sample_rate,extradata:packet=pos,size",
                "-show_data",
                "-of",
                "json",
                &source_path.to_string_lossy(),
            ],
        )?;
        let (input, rate, channels, count) = access_units(&source, &probe)?;
        let filename = format!("{id}.aac-access-units");
        let input_path = out.join(&filename);
        fs::write(&input_path, &input).map_err(|error| error.to_string())?;
        workloads.push(Value::object([
            ("id", Value::str(id)),
            ("path", Value::str(filename)),
            ("sha256", Value::str(sha256::hex(&input))),
            ("bytes", Value::int(input.len() as u64)),
            ("operation", Value::str(OPERATION)),
            ("iterations", Value::int(4)),
            ("packets", Value::int(count)),
            ("sample_rate", Value::int(rate)),
            ("channels", Value::int(channels)),
            ("source_sha256", Value::str(source_sha)),
        ]));

        let raw = |worker: &Path, role: &str| -> Result<(Value, Vec<u8>), String> {
            let pcm = out.join(format!("{id}.{role}.pcm"));
            let row = command(
                worker,
                &[
                    &format!("{OPERATION}-raw"),
                    &input_path.to_string_lossy(),
                    &pcm.to_string_lossy(),
                ],
            )?;
            let samples = read(&pcm)?;
            if number(&row, "packets")? != count || number(&row, "bytes")? as usize != samples.len()
            {
                return Err(format!("{id}: {role} AAC work differs"));
            }
            Ok((row, samples))
        };
        let (candidate_row, candidate_pcm) = raw(&candidate, "candidate")?;
        let (reference_row, reference_pcm) = raw(&reference, "reference")?;
        let (pass, planes) = compare_pcm(
            &candidate_pcm,
            &reference_pcm,
            channels as usize,
            count as usize,
            &rules,
        )?;
        accepted &= pass;
        results.push(Value::object([
            ("id", Value::str(id)),
            ("pass", Value::Bool(pass)),
            ("planes", planes),
            ("candidate", candidate_row),
            ("reference", reference_row),
        ]));
        println!("{id}: {count} blocks of {channels} planes at {rate} Hz");
    }
    let workloads = Value::object([
        ("schema", Value::str("triergon.codecs.workloads.v1")),
        ("workloads", Value::Array(workloads)),
    ]);
    corpus::write_json(&out.join("workloads.json"), &workloads)?;
    corpus::write_json(&out.join("parity.json"), &Value::Array(results))?;
    if !accepted {
        return Err("AAC PCM exceeds the accuracy bounds".into());
    }
    let pinned = root.join("harness/codecs/aac/access-unit-workloads.json");
    if record {
        corpus::write_json(&pinned, &workloads)?;
    } else if checks::read_json(&pinned)? != workloads {
        return Err("AAC access-unit workload differs from its pins".into());
    }
    Ok(())
}
