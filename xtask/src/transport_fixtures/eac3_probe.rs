//! Independently authored ATSC framing inputs for an exact packet-boundary probe.
//! These opaque bodies establish framing only, not decoded-audio validity.
use super::{Codec, R, authored_case, part};
use crate::corpus::{Paths, write_json};
use crate::json::Value;
use crate::{checks::read_json, json};
use std::process::Command;
use std::{fs, path::Path};

fn frame(code: u8, dependent: Option<u8>, sync: bool) -> Vec<u8> {
    let mut bytes = vec![0; 32];
    // A/52 Table E1.2: stereo, no optional metadata; short-frame convsync bit 53.
    bytes[..8].copy_from_slice(&[0x0b, 0x77, 0, 15, 5 | (code << 4), 0x87, 0xc0, 0]);
    if let Some(id) = dependent {
        bytes[2] = 0x40 | (id << 3);
    } else if sync && code != 3 {
        bytes[6] |= 4;
    }
    bytes
}

pub fn main(_root: &Path, args: &[String]) -> R<()> {
    let [output] = args else {
        return Err("usage: cargo xtask fixture transport-eac3-probe OUTPUT".into());
    };
    let output = Path::new(output);
    fs::create_dir_all(output).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for (name, codes) in [
        ("one-block", &[0; 6][..]),
        ("two-block", &[1; 3]),
        ("three-block", &[2; 2]),
        ("mixed-block", &[0, 1, 2]),
        ("six-block", &[3]),
    ] {
        for dependents in [0, 2] {
            let mut group = Vec::new();
            for (index, &code) in codes.iter().enumerate() {
                group.extend(frame(code, None, index == 0));
                for id in 0..dependents {
                    group.extend(frame(code, Some(id), false));
                }
            }
            let codec = Codec {
                name: "atsc-eac3",
                stream_type: 0x87,
                stream_id: 0xbd,
                dvb: false,
                bytes: Vec::new(),
                first: 32,
                header: 7,
            };
            // Two complete AUs within a PES, then a separately timestamped AU.
            let pair = group.repeat(2);
            let case = authored_case(
                &codec,
                &format!("{name}-dep{dependents}"),
                &[
                    part(&pair, Some(90_000), Some(80_000)),
                    part(&group, Some(180_000), None),
                ],
                false,
            )?;
            let id = case
                .row
                .get("id")
                .and_then(Value::as_str)
                .ok_or("missing id")?;
            let file = format!("{id}.ts");
            fs::write(output.join(&file), case.bytes.ok_or("missing bytes")?)
                .map_err(|e| e.to_string())?;
            let input = ["id", "sha256", "rights", "arguments", "construction"]
                .into_iter()
                .map(|key| {
                    case.row
                        .get(key)
                        .cloned()
                        .map(|value| (key, value))
                        .ok_or_else(|| format!("missing {key}"))
                })
                .collect::<R<Vec<_>>>()?;
            rows.push(Value::object([
                ("input", Value::object(input)),
                ("file", Value::str(file)),
                (
                    "primary_block_codes",
                    Value::Array(codes.iter().map(|&v| Value::int(v)).collect()),
                ),
                ("dependents_per_primary", Value::int(dependents)),
                ("access_units", Value::int(3)),
                ("bytes_per_access_unit", Value::int(group.len() as u64)),
                ("duration_per_access_unit", Value::int(2880)),
            ]));
        }
    }
    write_json(
        &output.join("inputs.json"),
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(rows))]),
    )
}

const FEATURE: &str = "mpegts-atsc-eac3-six-block";
const DIRECTORY: &str = "fixtures-mpegts-eac3-six-block";

pub fn generate(paths: &Paths, reference: Option<&Path>) -> R<String> {
    let output = paths.fixtures(DIRECTORY)?;
    main(&paths.root, &[output.display().to_string()])?;
    let inputs = read_json(&output.join("inputs.json"))?;
    let manifest_path = format!("features/{FEATURE}/corpus.json");
    let previous = if reference.is_none() {
        Some(read_json(&paths.root.join("harness").join(&manifest_path))?)
    } else {
        None
    };
    let mut rows = Vec::new();
    for case in super::cases(&inputs)? {
        let input = case.get("input").ok_or("missing input")?;
        let id = input
            .get("id")
            .and_then(Value::as_str)
            .ok_or("missing id")?;
        let file = case
            .get("file")
            .and_then(Value::as_str)
            .ok_or("missing file")?;
        let bytes = match case.get("bytes_per_access_unit") {
            Some(Value::Number(n)) => n
                .as_i128()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or("invalid size")?,
            _ => return Err("missing size".into()),
        } * 3;
        let expected = if let Some(reference) = reference {
            let result = Command::new(reference)
                .arg("ts-eac3-six-block-fold")
                .arg(output.join(file))
                .args(["188", "7", "--atsc"])
                .output()
                .map_err(|e| e.to_string())?;
            if !result.status.success() {
                return Err(format!("{id}: {}", String::from_utf8_lossy(&result.stderr)));
            }
            let record =
                json::parse(std::str::from_utf8(&result.stdout).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if record.get("operation").and_then(Value::as_str) != Some(FEATURE)
                || record.get("iterations") != Some(&Value::int(1))
                || record.get("eof") != Some(&Value::Bool(true))
                || record.get("packets") != Some(&Value::int(3))
                || record.get("bytes") != Some(&Value::int(bytes))
            {
                return Err(format!("{id}: incomplete reference fold"));
            }
            let digest = record.get("digest").ok_or("missing digest")?;
            if !matches!(digest, Value::Number(n) if n.as_i128().is_some_and(|n| u64::try_from(n).is_ok()))
            {
                return Err("invalid digest".into());
            }
            Value::object([
                ("packets", Value::int(3)),
                ("bytes", Value::int(bytes)),
                ("digest", digest.clone()),
            ])
        } else {
            super::cases(previous.as_ref().ok_or("missing pins")?)?
                .iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
                .and_then(|row| row.get("expected"))
                .cloned()
                .ok_or("missing case pin")?
        };
        rows.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(id)),
            ("file", Value::str(format!(".research/{DIRECTORY}/{file}"))),
            (
                "sha256",
                input.get("sha256").cloned().ok_or("missing hash")?,
            ),
            (
                "rights",
                input.get("rights").cloned().ok_or("missing rights")?,
            ),
            (
                "arguments",
                input.get("arguments").cloned().ok_or("missing arguments")?,
            ),
            (
                "construction",
                input
                    .get("construction")
                    .cloned()
                    .ok_or("missing construction")?,
            ),
            ("expected", expected),
        ]));
    }
    if rows.len() != 10 {
        return Err("expected ten E-AC-3 cases".into());
    }
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("directory", Value::str(".")),
        ("cases", Value::Array(rows)),
    ]);
    if previous.as_ref().is_some_and(|old| old != &manifest) {
        return Err("E-AC-3 authored inputs differ from pinned manifest".into());
    }
    write_json(&paths.out.join("harness").join(manifest_path), &manifest)?;
    Ok("Generated 10 E-AC-3 access-unit controls".into())
}

pub fn report_main(root: &Path, args: &[String]) -> R<()> {
    let reference = match args {
        [] => None,
        [flag, path] if flag == "--record" => Some(Path::new(path)),
        _ => {
            return Err(
                "usage: cargo xtask fixture transport-eac3-six-block [--record REFERENCE]".into(),
            );
        }
    };
    println!(
        "{}",
        generate(
            &Paths {
                root: root.into(),
                out: root.into()
            },
            reference
        )?
    );
    Ok(())
}
