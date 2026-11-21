//! Independently authored ATSC framing inputs for an exact packet-boundary probe.
//! These opaque bodies establish framing only, not decoded-audio validity.
use super::{Codec, R, authored_case, part};
use crate::corpus::write_json;
use crate::json::Value;
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
