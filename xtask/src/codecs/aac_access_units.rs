//! Prepare the original AAC packets and compare every decoded PCM plane.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{checks, corpus, harness, json, prepare, process, python, sha256};
use json::Value;

use super::correctness::{Rules, channel_stats};

const OPERATION: &str = "audio-aac-access-units";
const FEATURE: &str = "harness/features/aac-access-units";
const PARITY_FILE: &str = "parity.json";
const FFPROBE: &str = "install/bin/ffprobe";
const NUMERIC_RULES: &str = "harness/codecs/numeric-rules.json";
const PADDING: [u8; 64] = [0; 64];
const HEADER_BYTES: usize = 4 * size_of::<u32>();
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

fn file_sha(path: &Path) -> Result<String, String> {
    sha256::file_hex(path).map_err(|error| format!("{}: {error}", path.display()))
}

struct Comparison {
    candidate: PathBuf,
    reference: PathBuf,
    ffprobe: PathBuf,
    binding: Value,
    iterations: u32,
}

impl Comparison {
    fn load(root: &Path, paths: Option<(&Path, &Path)>) -> Result<Self, String> {
        let contract_path = root.join(FEATURE).join("contract.json");
        let contract = checks::read_json(&contract_path)?;
        if string(&contract, "operation")? != OPERATION {
            return Err("invalid AAC worker operation".into());
        }
        let identity = checks::read_json(&root.join("target/harness/identity.json"))?;
        let candidate_name = string(&contract, "candidate")?;
        let candidate_record = identity
            .get("additional_candidates")
            .and_then(|workers| workers.get(candidate_name))
            .ok_or("build codec-worker before comparing AAC access units")?;
        let reference_record =
            harness::comparison::reference_named(&identity, string(&contract, "reference")?)?;
        let profile = reference_record
            .get("reference_build")
            .ok_or("missing AAC reference build")?;
        if string(profile, "profile")? != "decode"
            || string(profile, "release")? != prepare::RELEASE
            || string(profile, "archive_sha256")? != prepare::RELEASE_SHA256
        {
            return Err("AAC reference must use the pinned decode profile".into());
        }
        let (candidate, reference) = match paths {
            Some((candidate, reference)) => (candidate.to_path_buf(), reference.to_path_buf()),
            None => (
                root.join(string(candidate_record, "path")?),
                root.join(string(&reference_record, "path")?),
            ),
        };
        let ffprobe = root.join(".research/reference-decode").join(FFPROBE);
        let candidate_sha = file_sha(&candidate)?;
        let reference_sha = file_sha(&reference)?;
        let probe_sha = file_sha(&ffprobe)?;
        if candidate_sha != string(candidate_record, "sha256")?
            || reference_sha != string(&reference_record, "sha256")?
            || probe_sha
                != string(
                    profile.get("installed").ok_or("missing decode tools")?,
                    FFPROBE,
                )?
        {
            return Err("AAC comparison tools differ from the recorded build".into());
        }
        let binding = Value::object([
            ("candidate_sha256", Value::str(candidate_sha)),
            ("reference_sha256", Value::str(reference_sha)),
            ("ffprobe_sha256", Value::str(probe_sha)),
            (
                "numeric_rules_sha256",
                Value::str(file_sha(&root.join(NUMERIC_RULES))?),
            ),
            ("contract_sha256", Value::str(file_sha(&contract_path)?)),
        ]);
        Ok(Self {
            candidate,
            reference,
            ffprobe,
            binding,
            iterations: number(&contract, "iterations")?,
        })
    }
}

/// Require matching PCM parity before either performance collector admits these workloads.
pub(super) fn measurement_parity(
    root: &Path,
    candidate: &Path,
    reference: &Path,
    workload_path: &Path,
    inputs: &Path,
    manifest: &Value,
) -> Result<Option<String>, String> {
    let cases = manifest
        .get("workloads")
        .and_then(Value::as_array)
        .unwrap_or_default();
    if !cases
        .iter()
        .any(|case| case.get("operation").and_then(Value::as_str) == Some(OPERATION))
    {
        return Ok(None);
    }
    if *manifest != checks::read_json(&root.join(FEATURE).join("corpus.json"))? {
        return Err("AAC measurement requires the complete pinned workload set".into());
    }
    let comparison = Comparison::load(root, Some((candidate, reference)))?;
    let parity_path = inputs.join(PARITY_FILE);
    let parity = checks::read_json(&parity_path)?;
    if parity.get("comparison") != Some(&comparison.binding)
        || string(&parity, "workloads_sha256")? != file_sha(workload_path)?
    {
        return Err("AAC parity inputs, tools or rules changed".into());
    }
    let rows = parity
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing AAC parity cases")?;
    if rows.len() != cases.len()
        || rows.iter().zip(cases).any(|(row, case)| {
            row.get("pass") != Some(&Value::Bool(true))
                || row.get("id") != case.get("id")
                || row.get("input_sha256") != case.get("sha256")
        })
    {
        return Err("AAC measurement requires passing parity for every workload".into());
    }
    Ok(Some(file_sha(&parity_path)?))
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
    if !(2..=MAX_PACKET).contains(&config.len()) {
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

fn compare_speakers(
    candidate: &Value,
    reference: &Value,
    configuration: u8,
    channels: u32,
) -> Result<(bool, bool), String> {
    fn ids(row: &Value, channels: u32) -> Result<Vec<u8>, String> {
        row.get("speakers")
            .and_then(Value::as_array)
            .filter(|speakers| speakers.len() == channels as usize)
            .ok_or("missing AAC speaker order")?
            .iter()
            .map(|id| {
                python::int(id)
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| "invalid AAC speaker".to_owned())
            })
            .collect()
    }
    let ours = ids(candidate, channels)?;
    let theirs = ids(reference, channels)?;
    // AVChannel IDs, in plane order: FL, FR, FC, optional LFE, and one surround pair.
    let surround_pair_labels = matches!(
        (configuration, ours.as_slice(), theirs.as_slice()),
        (5, [0, 1, 2, 9, 10], [0, 1, 2, 4, 5]) | (6, [0, 1, 2, 3, 9, 10], [0, 1, 2, 3, 4, 5])
    );
    Ok((ours == theirs || surround_pair_labels, surround_pair_labels))
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
    let comparison = Comparison::load(root, None)?;
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
            &comparison.ffprobe,
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
        let configuration = input[HEADER_BYTES + 1] >> 3 & 15;
        let input_sha = sha256::hex(&input);
        let filename = format!("{id}.aac-access-units");
        let input_path = out.join(&filename);
        fs::write(&input_path, &input).map_err(|error| error.to_string())?;
        workloads.push(Value::object([
            ("id", Value::str(id)),
            ("path", Value::str(filename)),
            ("sha256", Value::str(&input_sha)),
            ("bytes", Value::int(input.len() as u64)),
            ("operation", Value::str(OPERATION)),
            ("iterations", Value::int(comparison.iterations)),
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
        let (candidate_row, candidate_pcm) = raw(&comparison.candidate, "candidate")?;
        let (reference_row, reference_pcm) = raw(&comparison.reference, "reference")?;
        let (layout, surround_pair_labels) =
            compare_speakers(&candidate_row, &reference_row, configuration, channels)?;
        let (pcm, planes) = compare_pcm(
            &candidate_pcm,
            &reference_pcm,
            channels as usize,
            count as usize,
            &rules,
        )?;
        accepted &= layout && pcm;
        results.push(Value::object([
            ("id", Value::str(id)),
            ("input_sha256", Value::str(input_sha)),
            ("pass", Value::Bool(layout && pcm)),
            ("layout_matches", Value::Bool(layout)),
            ("surround_pair_labels", Value::Bool(surround_pair_labels)),
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
    let pinned = root.join(FEATURE).join("corpus.json");
    if !record && checks::read_json(&pinned)? != workloads {
        return Err("AAC access-unit workload differs from its pins".into());
    }
    let workload_path = out.join("workloads.json");
    corpus::write_json(&workload_path, &workloads)?;
    let parity = Value::object([
        ("comparison", comparison.binding),
        ("workloads_sha256", Value::str(file_sha(&workload_path)?)),
        ("cases", Value::Array(results)),
    ]);
    corpus::write_json(&out.join(PARITY_FILE), &parity)?;
    if !accepted {
        return Err("AAC speaker order or PCM differs from the reference".into());
    }
    if record {
        corpus::write_json(&pinned, &workloads)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_single_fixed_surround_pair_accepts_different_labels() {
        let speakers = |ids: &[u8]| {
            Value::object([(
                "speakers",
                Value::Array(ids.iter().map(|id| Value::int(*id)).collect()),
            )])
        };
        let side = speakers(&[0, 1, 2, 3, 9, 10]);
        let back = speakers(&[0, 1, 2, 3, 4, 5]);
        assert_eq!(compare_speakers(&side, &side, 6, 6).unwrap(), (true, false));
        assert_eq!(compare_speakers(&side, &back, 6, 6).unwrap(), (true, true));
        assert_eq!(
            compare_speakers(&side, &back, 0, 6).unwrap(),
            (false, false)
        );
        let reversed = speakers(&[0, 1, 2, 3, 5, 4]);
        assert_eq!(
            compare_speakers(&side, &reversed, 6, 6).unwrap(),
            (false, false)
        );
    }
}
