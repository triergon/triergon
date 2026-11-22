//! Complete pinned transport inputs for public-API automatic DTS admission.
use super::R;
use crate::corpus::write_json;
use crate::json::{self, Value};
use crate::sha256;
use std::{fs, path::Path, process::Command};

const FEATURE: &str = "mpegts-dts-discovery";
pub(crate) const FATE_FILE: &str = ".research/fate-samples/dts/dts.ts";
pub(crate) const FATE_SHA: &str =
    "749561299cf838d55f52d5697f78c273a6c381c5e6c8300f859dfe23252ae2c2";
const DIRECTORY: &str = ".research/fixtures-mpegts-dts-discovery";
const AUTHORED: [&str; 2] = [
    "dts-core-unlabelled-private-data",
    "dts-core-pcr-reserved-bits",
];
const REFUSALS: [&str; 4] = ["false-sync", "second-sync", "different-mode", "ambiguous"];

/// Declares the exact files produced after the standard vector fetch.
pub(crate) fn prepared_inputs(root: &Path) -> R<Vec<(String, String)>> {
    let directory = root.join(format!("harness/features/{FEATURE}"));
    let contract = directory.join("contract.json");
    if !contract.is_file() {
        return Ok(Vec::new());
    }
    let contract = crate::checks::read_json(&contract)?;
    let seed = contract
        .get("prepare_from")
        .ok_or("missing DTS preparation seed")?;
    if seed.get("file").and_then(Value::as_str) != Some(FATE_FILE)
        || seed.get("sha256").and_then(Value::as_str) != Some(FATE_SHA)
    {
        return Err("DTS preparation seed differs from the registered fixture source".into());
    }
    let manifest = crate::checks::read_json(&directory.join("corpus.json"))?;
    let cases = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing DTS discovery cases")?;
    let names = ["fate"].into_iter().chain(AUTHORED).chain(REFUSALS);
    if cases.len() != 1 + AUTHORED.len() + REFUSALS.len() {
        return Err("unexpected DTS discovery fixture count".into());
    }
    names
        .zip(cases)
        .map(|(name, case)| {
            let file = format!("{DIRECTORY}/{name}.ts");
            if case.get("id").and_then(Value::as_str) != Some(name)
                || case.get("file").and_then(Value::as_str) != Some(&file)
            {
                return Err(format!("unexpected DTS discovery input for {name}"));
            }
            let hash = case
                .get("sha256")
                .and_then(Value::as_str)
                .ok_or("DTS discovery case without a hash")?;
            Ok((file, hash.to_owned()))
        })
        .collect()
}

pub(crate) fn prepare(root: &Path) -> R<()> {
    if !prepared_inputs(root)?.is_empty() {
        generate(root, &root.join(FATE_FILE), None)?;
    }
    Ok(())
}

fn crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1)
                ^ if crc & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
        }
    }
    crc
}

fn controls(bytes: &[u8]) -> R<Vec<(&'static str, Vec<u8>)>> {
    let mut indices = Vec::new();
    let mut started = false;
    for (packet, ts) in bytes.chunks_exact(188).enumerate() {
        if (u16::from(ts[1] & 31) << 8 | u16::from(ts[2])) != 256 || ts[3] & 16 == 0 {
            continue;
        }
        let mut at = if ts[3] & 32 != 0 {
            5 + usize::from(ts[4])
        } else {
            4
        };
        if ts[1] & 64 != 0 {
            if started {
                break;
            }
            started = true;
            at += 9 + usize::from(ts[at + 8]);
        }
        indices.extend((at..188).map(|at| packet * 188 + at));
    }
    if indices.len() != 2012 {
        return Err("pinned first PES extent changed".into());
    }
    let mut result = Vec::new();
    for (name, element, value) in [
        ("false-sync", 0, 0),
        ("second-sync", 1006, 0),
        ("different-mode", 1013, bytes[indices[1013]] ^ 1),
    ] {
        let mut edited = bytes.to_vec();
        edited[indices[element]] = value;
        result.push((name, edited));
    }
    let mut edited = bytes.to_vec();
    for ts in edited.chunks_exact_mut(188) {
        if (u16::from(ts[1] & 31) << 8 | u16::from(ts[2])) != 4095 || ts[1] & 64 == 0 {
            continue;
        }
        let mut at = if ts[3] & 32 != 0 {
            5 + usize::from(ts[4])
        } else {
            4
        };
        at += 1 + usize::from(ts[at]);
        if ts[at] != 2 {
            continue;
        }
        let length = 3 + (usize::from(ts[at + 1] & 15) << 8) + usize::from(ts[at + 2]);
        let mut section = ts[at..at + length - 4].to_vec();
        section.extend_from_slice(&[6, 0xe1, 1, 0xf0, 0]);
        section[2] += 5;
        section.extend_from_slice(&crc(&section).to_be_bytes());
        ts[at..at + section.len()].copy_from_slice(&section);
    }
    result.push(("ambiguous", edited));
    Ok(result)
}

/// Generate complete inputs outside git; record folds using the public reference.
pub fn main(root: &Path, args: &[String]) -> R<()> {
    let (fate, reference) = match args {
        [fate] => (fate, None),
        [fate, record, reference] if record == "--record" => (fate, Some(Path::new(reference))),
        _ => return Err(
            "usage: cargo xtask fixture transport-dts-discovery FATE_DTS_TS [--record REFERENCE]"
                .into(),
        ),
    };
    generate(root, Path::new(fate), reference)
}

fn generate(root: &Path, fate: &Path, reference: Option<&Path>) -> R<()> {
    let manifest_path = root.join(format!("harness/features/{FEATURE}/corpus.json"));
    let previous = if reference.is_none() {
        Some(crate::checks::read_json(&manifest_path)?)
    } else {
        None
    };
    let bytes = fs::read(fate).map_err(|e| e.to_string())?;
    if sha256::hex(&bytes) != FATE_SHA {
        return Err("FATE dts/dts.ts hash mismatch".into());
    }
    let mut inputs = vec![("fate", bytes.clone(), "1", true)];
    for name in AUTHORED {
        inputs.push((
            name,
            fs::read(root.join(format!("harness/features/mpegts-dts-core/inputs/{name}.ts")))
                .map_err(|e| e.to_string())?,
            "7",
            true,
        ));
    }
    inputs.extend(
        controls(&bytes)?
            .into_iter()
            .map(|(name, bytes)| (name, bytes, "1", false)),
    );
    fs::create_dir_all(root.join(DIRECTORY)).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for (name, bytes, program, eof) in inputs {
        let file = format!("{DIRECTORY}/{name}.ts");
        fs::write(root.join(&file), &bytes).map_err(|e| e.to_string())?;
        let arguments = [
            "188",
            program,
            "--probe-dts-core",
            "--ignore-pcr-reserved-bits",
        ];
        let observed = if let Some(reference) = reference {
            let output = Command::new(reference)
                .arg("ts-au-fold")
                .arg(root.join(&file))
                .args(arguments)
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "{name}: reference failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            json::parse(std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        } else {
            previous
                .as_ref()
                .and_then(|m| m.get("cases"))
                .and_then(Value::as_array)
                .and_then(|rows| {
                    rows.iter()
                        .find(|row| row.get("id").and_then(Value::as_str) == Some(name))
                })
                .and_then(|row| row.get("expected"))
                .cloned()
                .ok_or("missing discovery pin")?
        };
        if observed.get("eof") != Some(&Value::Bool(eof)) {
            return Err(format!("{name}: reference admission changed"));
        }
        let mut expected = Vec::new();
        for key in ["packets", "bytes", "digest"] {
            let value = observed.get(key).ok_or("missing reference fold")?.clone();
            if !eof && value != Value::int(0) {
                return Err("refusal has nonzero coverage".into());
            }
            expected.push((key.to_string(), value));
        }
        expected.push(("eof".into(), Value::Bool(eof)));
        rows.push(Value::object([
            ("id", Value::str(name)),
            ("profile", Value::str(name)),
            ("file", Value::str(file)),
            ("sha256", Value::str(sha256::hex(&bytes))),
            (
                "rights",
                Value::str(if program == "7" {
                    "authored-synthetic"
                } else {
                    "fetched-not-redistributed"
                }),
            ),
            ("arguments", Value::strings(arguments)),
            ("expected", Value::Object(expected)),
        ]));
    }
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("directory", Value::str(".")),
        ("cases", Value::Array(rows)),
    ]);
    if let Some(previous) = previous {
        if previous != manifest {
            return Err("generated discovery inputs differ from their pins".into());
        }
        return Ok(());
    }
    write_json(&manifest_path, &manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_matches_mpeg_check_value() {
        assert_eq!(crc(b"123456789"), 0x0376_e6e7);
    }
}
