//! Synthetic 16×16 I/P/B picture carriage, authored from H.262 syntax tables.
use super::{Codec, Input, PID, R, Transport, manifest, part};
use crate::corpus::write_json;
use crate::json::Value;
use crate::sha256;
use std::path::Path;

fn bits(fields: &[(u32, u8)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut used = 0usize;
    for &(value, width) in fields {
        for shift in (0..width).rev() {
            if used.is_multiple_of(8) {
                bytes.push(0);
            }
            *bytes.last_mut().unwrap() |= (((value >> shift) & 1) as u8) << (7 - used % 8);
            used += 1;
        }
    }
    bytes
}

fn unit(out: &mut Vec<u8>, code: u8, fields: &[(u32, u8)]) {
    out.extend_from_slice(&[0, 0, 1, code]);
    out.extend(bits(fields));
}

fn picture(mpeg2: bool, temporal_reference: u32, rate: u32) -> Vec<u8> {
    typed_picture(mpeg2, temporal_reference, rate, 1)
}

fn typed_picture(mpeg2: bool, temporal_reference: u32, rate: u32, kind: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    // H.262 §6.2.2.1: dimensions, aspect, rate, bitrate, marker, VBV,
    // constrained parameters and both default quantisation matrices.
    unit(
        &mut bytes,
        0xb3,
        &[
            (16, 12),
            (16, 12),
            (1, 4),
            (rate, 4),
            (2500, 18),
            (1, 1),
            (20, 10),
            (0, 1),
            (0, 1),
            (0, 1),
        ],
    );
    if mpeg2 {
        // §6.2.2.3: Main Profile / Main Level, progressive 4:2:0.
        unit(
            &mut bytes,
            0xb5,
            &[
                (1, 4),
                (0x48, 8),
                (1, 1),
                (1, 2),
                (0, 2),
                (0, 2),
                (0, 12),
                (1, 1),
                (0, 8),
                (0, 1),
                (0, 2),
                (0, 5),
            ],
        );
    }
    // §6.2.3: temporal reference, picture coding type, unspecified VBV delay,
    // no extra information; byte-alignment zero bits.
    let mut header = vec![(temporal_reference, 10), (kind, 3), (65535, 16)];
    if kind >= 2 {
        header.extend([(0, 1), (7, 3)]);
    }
    if kind == 3 {
        header.extend([(0, 1), (7, 3)]);
    }
    header.push((0, 1));
    unit(&mut bytes, 0, &header);
    if mpeg2 {
        unit(
            &mut bytes,
            0xb5,
            &[
                (8, 4),
                (if kind == 1 { 65535 } else { 0x1111 }, 16),
                (0, 2),
                (3, 2),
                (0, 1),
                (1, 1),
                (0, 1),
                (0, 1),
                (0, 1),
                (0, 1),
                (0, 1),
                (1, 1),
                (1, 1),
                (0, 1),
            ],
        );
    }
    // §6.2.4 and ISO/IEC 11172-2: qscale=2, no extra slice information,
    // address increment=1, intra macroblock; zero DC differences and EOB.
    unit(
        &mut bytes,
        1,
        &[
            (2, 5),
            (0, 1),
            (1, 1),
            (
                if kind == 1 { 1 } else { 0b00011 },
                if kind == 1 { 1 } else { 5 },
            ),
            (0b10010, 5),
            (0b10010, 5),
            (0b10010, 5),
            (0b10010, 5),
            (0b0010, 4),
            (0b0010, 4),
        ],
    );
    unit(&mut bytes, 0xb7, &[]);
    bytes
}

pub fn author(root: &Path, args: &[String]) -> R<()> {
    let [record, reference, libraries @ ..] = args else {
        return Err("usage: cargo xtask fixture transport-mpeg-video --record REFERENCE".into());
    };
    if record != "--record" {
        return Err("--record required".into());
    }
    let mut library_pins = Vec::new();
    for pair in libraries.chunks(2) {
        let [flag, path] = pair else {
            return Err("--reference-library PATH required".into());
        };
        if flag != "--reference-library" {
            return Err("unknown reference option".into());
        }
        let path = Path::new(path).canonicalize().map_err(|e| e.to_string())?;
        library_pins.push(Value::object([
            (
                "file",
                Value::str(path.file_name().ok_or("library name")?.to_string_lossy()),
            ),
            (
                "sha256",
                Value::str(sha256::file_hex(&path).map_err(|e| e.to_string())?),
            ),
        ]));
    }
    let reference = Path::new(reference)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    const FEATURE: &str = "mpegts-mpeg-video";
    let mut cases = Vec::new();
    for (mpeg2, name) in [(false, "mpeg1"), (true, "mpeg2")] {
        for (m2ts, suffix) in [(false, "ts188"), (true, "m2ts192")] {
            for (rate, rate_name) in [(3, "25"), (4, "30000-1001")] {
                let codec = Codec {
                    name,
                    stream_type: if mpeg2 { 2 } else { 1 },
                    stream_id: 0xe0,
                    dvb: false,
                    bytes: Vec::new(),
                    first: 0,
                    header: 0,
                };
                let mut ts = Transport::new();
                ts.tables(&codec, &[])?;
                for (index, pts, dts) in [(0, 90000, 87000), (1, 93600, 90600)] {
                    let bytes = picture(mpeg2, index, rate);
                    let mut packet = part(&bytes, Some(pts), Some(dts));
                    packet.aligned = true;
                    ts.pes_at(PID, 0xe0, &packet)?;
                }
                let name = format!("{name}-{suffix}-{rate_name}");
                let input = Input {
                    name: match (mpeg2, m2ts, rate) {
                        (false, false, 3) => "mpeg1-ts188-25",
                        (false, false, _) => "mpeg1-ts188-30000-1001",
                        (false, true, 3) => "mpeg1-m2ts192-25",
                        (false, true, _) => "mpeg1-m2ts192-30000-1001",
                        (true, false, 3) => "mpeg2-ts188-25",
                        (true, false, _) => "mpeg2-ts188-30000-1001",
                        (true, true, 3) => "mpeg2-m2ts192-25",
                        (true, true, _) => "mpeg2-m2ts192-30000-1001",
                    },
                    m2ts,
                    data: ts.finish(m2ts),
                };
                let row = input.record(root, FEATURE, &name, &[], &reference, "ts-au-fold")?;
                if row.get("reference_refusal").is_some() {
                    return Err(format!("{name}: reference did not reach EOF"));
                }
                cases.push(row);
            }
        }
    }
    for (m2ts, name) in [
        (false, "mpeg2-ipb-ts188-25"),
        (true, "mpeg2-ipb-m2ts192-25"),
        (false, "mpeg2-ipb-ts188-30000-1001"),
        (true, "mpeg2-ipb-m2ts192-30000-1001"),
    ] {
        let rate = if name.ends_with("-25") { 3 } else { 4 };
        let codec = Codec {
            name: "mpeg2",
            stream_type: 2,
            stream_id: 0xe0,
            dvb: false,
            bytes: Vec::new(),
            first: 0,
            header: 0,
        };
        let mut ts = Transport::new();
        ts.tables(&codec, &[])?;
        let tick = if rate == 3 { 3600 } else { 3003 };
        // Decode order I, P, B; presentation order I, B, P.
        for (index, kind, temporal) in [(0, 1, 0), (1, 2, 2), (2, 3, 1)] {
            let bytes = typed_picture(true, temporal, rate, kind);
            let mut packet = part(
                &bytes,
                Some(90000 + temporal as u64 * tick),
                Some(84000 + index * tick),
            );
            packet.aligned = true;
            ts.pes_at(PID, 0xe0, &packet)?;
        }
        let input = Input {
            name,
            m2ts,
            data: ts.finish(m2ts),
        };
        let row = input.record(root, FEATURE, name, &[], &reference, "ts-au-fold")?;
        if row.get("reference_refusal").is_some() {
            return Err(format!("{name}: reference did not reach EOF"));
        }
        cases.push(row);
    }
    manifest(root, FEATURE, cases)?;
    write_json(
        &root.join(format!("harness/features/{FEATURE}/reference.json")),
        &Value::object([
            (
                "worker_sha256",
                Value::str(sha256::file_hex(&reference).map_err(|e| e.to_string())?),
            ),
            ("mode", Value::str("ts-au-fold")),
            ("libraries", Value::Array(library_pins)),
            (
                "adapter_sha256",
                Value::str(
                    sha256::file_hex(&root.join("harness/reference_ts_worker.c"))
                        .map_err(|e| e.to_string())?,
                ),
            ),
        ]),
    )
}
