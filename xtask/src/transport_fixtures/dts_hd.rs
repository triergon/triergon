//! Specification-authored DTS-HD headers and finite aligned PES controls.
use super::{Codec, Part, R, Transport, cases, part};
use crate::corpus::Paths;
use crate::json::{self, Value};
use crate::sha256;
use std::fs;
use std::path::Path;
use std::process::Command;

const FEATURE: &str = "mpegts-dts-hd-grouped";
const MULTI_FEATURE: &str = "mpegts-dts-hd-extensions";
const DIRECTORY: &str = "fixtures-mpegts-dts-hd-grouped";
const OPERATION: &str = "mpegts-dts-hd-source-access-units";

struct Bits(Vec<bool>);

impl Bits {
    fn put(&mut self, width: usize, value: usize) {
        assert!(
            width <= usize::BITS as usize && (width == usize::BITS as usize || value < 1 << width)
        );
        self.0
            .extend((0..width).rev().map(|bit| value & (1 << bit) != 0));
    }

    fn pad(&mut self, length: usize) {
        assert!(self.0.len() <= length);
        self.0.resize(length, false);
    }

    fn bytes(&self) -> Vec<u8> {
        assert_eq!(self.0.len() % 8, 0);
        self.0
            .chunks_exact(8)
            .map(|bits| {
                bits.iter()
                    .fold(0, |byte, bit| (byte << 1) | u8::from(*bit))
            })
            .collect()
    }
}

/// TS 102 114 V1.6.1 Tables 7-2, 7-5..7-7 and Annex B. Asset bytes are
/// deliberately opaque: these are framing controls, not decoder vectors.
fn extension(
    index: usize,
    long: bool,
    asset_bytes: usize,
    timing: Option<(usize, usize)>,
    marker: u8,
) -> Vec<u8> {
    let header_bytes = if long { 288 } else { 32 };
    let size_bits = if long { 20 } else { 16 };
    let mut header = Bits(Vec::new());
    for (width, value) in [
        (32, 0x6458_2025),
        (8, 0xa5),
        (2, index),
        (1, usize::from(long)),
        (if long { 12 } else { 8 }, header_bytes - 1),
        (size_bits, header_bytes + asset_bytes - 1),
        (1, usize::from(timing.is_some())),
    ] {
        header.put(width, value);
    }
    if let Some((clock, duration)) = timing {
        for (width, value) in [
            (2, clock),
            (3, duration),
            (1, 0), // Reference clock, duration, no timecode.
            (3, 0),
            (3, 0), // One presentation and one asset.
            (index + 1, 1 << index),
            (8, 1),
            (1, 0), // Active asset; no mixing.
        ] {
            header.put(width, value);
        }
    }
    header.put(size_bits, asset_bytes - 1);
    let mut asset = Bits(Vec::new());
    let asset_header_bytes = if timing.is_some() { 10 } else { 6 };
    asset.put(9, asset_header_bytes - 1);
    asset.put(3, 0);
    if timing.is_some() {
        let rate_code = match timing.map(|(clock, _)| clock) {
            Some(0) => 2,
            Some(1) => 6,
            _ => 12,
        };
        for (width, value) in [
            (1, 0),
            (1, 0),
            (1, 0), // No optional descriptions.
            (5, 15),
            (4, rate_code),
            (8, 0), // 16 bits, declared clock rate, mono.
            (1, 1),
            (1, 0),
            (3, 0), // Speaker feed; no remapping.
        ] {
            asset.put(width, value);
        }
    }
    for (width, value) in [
        (1, 0),
        (1, 0),
        (2, 1), // No DRC/DNC; lossless asset navigation.
        (size_bits, asset_bytes - 1),
        (1, 0),
        (3, 0),
        (1, 0),
        (1, 0),
    ] {
        asset.put(width, value);
    }
    asset.pad(asset_header_bytes * 8);
    header.0.extend(asset.0);
    header.put(1, 0); // No backward compatible core.
    header.pad((header_bytes - 2) * 8);
    let mut bytes = header.bytes();
    let mut crc = u16::MAX;
    for &byte in &bytes[5..] {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    bytes.extend(crc.to_be_bytes());
    bytes.resize(header_bytes + asset_bytes, marker);
    bytes
}

#[derive(Clone, Copy)]
enum Timing {
    Known(usize, usize),
    Unknown,
    Late,
}

struct Control {
    name: &'static str,
    m2ts: bool,
    long: bool,
    mask: u8,
    groups: &'static [usize],
    timing: Timing,
}

fn controls() -> [Control; 8] {
    [
        Control {
            name: "extension-single-control",
            m2ts: false,
            long: false,
            mask: 1,
            groups: &[1, 1, 1],
            timing: Timing::Known(2, 1),
        },
        Control {
            name: "extension-grouped-short",
            m2ts: false,
            long: false,
            mask: 1,
            groups: &[3, 1, 2],
            timing: Timing::Known(2, 1),
        },
        Control {
            name: "extension-grouped-long",
            m2ts: true,
            long: true,
            mask: 1,
            groups: &[2, 3, 1],
            timing: Timing::Known(1, 0),
        },
        Control {
            name: "extension-grouped-index-two",
            m2ts: false,
            long: false,
            mask: 4,
            groups: &[2, 1, 2],
            timing: Timing::Known(0, 0),
        },
        Control {
            name: "extension-grouped-no-timing",
            m2ts: true,
            long: false,
            mask: 1,
            groups: &[2, 1, 2],
            timing: Timing::Unknown,
        },
        Control {
            name: "extension-grouped-late-timing",
            m2ts: false,
            long: false,
            mask: 1,
            groups: &[3, 1, 2],
            timing: Timing::Late,
        },
        Control {
            name: "two-extensions-no-core",
            m2ts: false,
            long: false,
            mask: 3,
            groups: &[1, 1, 1],
            timing: Timing::Known(2, 1),
        },
        Control {
            name: "four-extensions-no-core-long",
            m2ts: true,
            long: true,
            mask: 15,
            groups: &[1, 1, 1],
            timing: Timing::Known(2, 1),
        },
    ]
}

fn author(control: &Control) -> R<(Vec<u8>, Value, usize, usize)> {
    let codec = Codec {
        name: "dvb-dts-hd",
        stream_type: 6,
        stream_id: 0xbd,
        dvb: true,
        bytes: Vec::new(),
        first: 0,
        header: 0,
    };
    let mut descriptor = vec![0x7f, 2 + 6 * control.mask.count_ones() as u8, 0x0e, 0];
    let rate_code = match control.timing {
        Timing::Known(0, _) | Timing::Late => 2,
        Timing::Known(1, _) => 6,
        _ => 12,
    };
    for index in 0..4 {
        if control.mask & (1 << index) != 0 {
            // EN 300 468 G.3: one mono lossless asset at the declared rate.
            descriptor[3] |= 0x40 >> index;
            descriptor.extend([5, 1, rate_code << 3, 0x48, 0, 0]);
        }
    }
    let mut ts = Transport::new();
    ts.tables(&codec, &descriptor)?;
    let mut construction = Vec::new();
    let mut ordinal = 0;
    let mut total_bytes = 0;
    for (pes_index, &count) in control.groups.iter().enumerate() {
        assert!(control.mask.is_power_of_two() || count == 1);
        let mut payload = Vec::new();
        let mut lengths = Vec::new();
        for _ in 0..count {
            let start = payload.len();
            for index in 0..4 {
                if control.mask & (1 << index) == 0 {
                    continue;
                }
                payload.resize(payload.len().next_multiple_of(4), 0);
                let timing = match control.timing {
                    Timing::Known(clock, duration) if ordinal == 0 => Some((clock, duration)),
                    Timing::Late if ordinal == 1 => Some((0, 0)),
                    _ => None,
                };
                let asset = if control.long && ordinal == 1 {
                    12 * 1024
                } else {
                    8 + ordinal * 4
                        + if control.mask.is_power_of_two() {
                            0
                        } else {
                            index
                        }
                };
                payload.extend(extension(
                    index,
                    control.long,
                    asset,
                    timing,
                    (ordinal * 4 + index) as u8,
                ));
            }
            lengths.push(Value::int((payload.len() - start) as u64));
            ordinal += 1;
        }
        let pts = (pes_index != 2).then_some(90_000 + pes_index as u64 * 9000);
        let dts = (pes_index == 1).then_some(98_100);
        ts.pes(
            0xbd,
            &Part {
                aligned: true,
                ..part(&payload, pts, dts)
            },
        )?;
        total_bytes += payload.len();
        construction.push(Value::object([
            ("access_unit_bytes", Value::Array(lengths)),
            ("pts_raw33", pts.map_or(Value::Null, Value::int)),
            ("dts_raw33", dts.map_or(Value::Null, Value::int)),
        ]));
    }
    Ok((
        ts.finish(control.m2ts),
        Value::Array(construction),
        ordinal,
        total_bytes,
    ))
}

pub fn generate(paths: &Paths, reference: Option<&Path>) -> R<String> {
    generate_feature(paths, reference, FEATURE, false)?;
    generate_feature(paths, reference, MULTI_FEATURE, true)?;
    Ok(format!(
        "Generated {} DTS-HD access-unit controls",
        controls().len()
    ))
}

fn generate_feature(
    paths: &Paths,
    reference: Option<&Path>,
    feature: &str,
    multiple: bool,
) -> R<()> {
    let file = format!("features/{feature}/corpus.json");
    let previous = if reference.is_none() {
        Some(json::read(&paths.root.join("harness").join(&file))?)
    } else {
        None
    };
    let output = paths.fixtures(DIRECTORY)?;
    let mut rows = Vec::new();
    for control in controls()
        .into_iter()
        .filter(|control| (control.mask.count_ones() > 1) == multiple)
    {
        let (bytes, pes, packets, payload_bytes) = author(&control)?;
        let name = format!(
            "{}.{}",
            control.name,
            if control.m2ts { "m2ts" } else { "ts" }
        );
        let input = output.join(&name);
        fs::write(&input, &bytes).map_err(|e| e.to_string())?;
        let arguments = [if control.m2ts { "192" } else { "188" }, "7", "--dvb"];
        let expected = if let Some(reference) = reference {
            let mode = if multiple {
                "ts-au-fold"
            } else {
                "ts-dts-hd-fold"
            };
            let operation = if multiple {
                "mpegts-access-unit-packets"
            } else {
                OPERATION
            };
            let result = Command::new(reference)
                .arg(mode)
                .arg(&input)
                .args(arguments)
                .output()
                .map_err(|e| e.to_string())?;
            if !result.status.success() {
                return Err(format!(
                    "{}: {}",
                    control.name,
                    String::from_utf8_lossy(&result.stderr)
                ));
            }
            let record =
                json::parse(std::str::from_utf8(&result.stdout).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if record.get("operation").and_then(Value::as_str) != Some(operation)
                || record.get("eof") != Some(&Value::Bool(true))
                || record.get("iterations") != Some(&Value::int(1))
                || record.get("packets") != Some(&Value::int(packets as u64))
                || record.get("bytes") != Some(&Value::int(payload_bytes as u64))
            {
                return Err(format!(
                    "{}: reference did not cover every authored AU and byte",
                    control.name
                ));
            }
            let digest = record
                .get("digest")
                .ok_or("missing DTS-HD reference digest")?;
            if !matches!(digest, Value::Number(word) if word.as_i128().is_some_and(|word| u64::try_from(word).is_ok()))
            {
                return Err("DTS-HD reference digest must be an unsigned 64-bit word".into());
            }
            Value::object([
                ("packets", Value::int(packets as u64)),
                ("bytes", Value::int(payload_bytes as u64)),
                ("digest", digest.clone()),
            ])
        } else {
            let row = cases(previous.as_ref().ok_or("missing DTS-HD pins")?)?
                .iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(control.name))
                .ok_or("missing DTS-HD case pin")?;
            row.get("expected")
                .cloned()
                .ok_or("missing DTS-HD reference fold")?
        };
        rows.push(Value::object([
            ("id", Value::str(control.name)),
            ("profile", Value::str(control.name)),
            ("file", Value::str(format!(".research/{DIRECTORY}/{name}"))),
            ("sha256", Value::str(sha256::hex(&bytes))),
            ("rights", Value::str("authored-synthetic")),
            ("arguments", Value::strings(arguments)),
            ("construction", pes),
            ("expected", expected),
        ]));
    }
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("directory", Value::str(".")),
        ("cases", Value::Array(rows)),
    ]);
    if previous
        .as_ref()
        .is_some_and(|previous| previous != &manifest)
    {
        return Err("DTS-HD authored inputs differ from the pinned manifest".into());
    }
    json::write(&paths.out.join("harness").join(file), &manifest)?;
    Ok(())
}

pub fn main(root: &Path, args: &[String]) -> R<()> {
    let reference = match args {
        [] => None,
        [record, reference] if record == "--record" => Some(Path::new(reference)),
        _ => {
            return Err(
                "usage: cargo xtask fixture transport-dts-hd-grouped [--record REFERENCE]".into(),
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

#[cfg(test)]
mod tests {
    #[test]
    fn regeneration_reproduces_pinned_dts_hd_controls() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let out = crate::walk::scratch("dts-hd-controls");
        super::generate(
            &crate::corpus::Paths {
                root,
                out: out.clone(),
            },
            None,
        )
        .unwrap();
        std::fs::remove_dir_all(out).unwrap();
    }
}
