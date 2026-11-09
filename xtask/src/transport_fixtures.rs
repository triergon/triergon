//! MPEG-TS AU controls, authored independently of the container and codec crates.
//! PES requests remain separate rows; their input bytes never change.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::checks::read_json;
use crate::corpus::{Paths, authored, write_json};
use crate::harness::worker;
use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;

const FEATURE: &str = "features/mpegts-access-units";
const DIRECTORY: &str = "fixtures-mpegts-access-units";
const ORIGINAL: &str = "harness/features/mpegts-pes/corpus.json";
// Provenance of the retained AU observations.
const ORIGINAL_SHA: &str = "acbb8a8e8c6d61239cf91f62f5a61438503b233bf11224166d202379210acd96";
const ORIGINAL_INPUTS_SHA: &str =
    "eadedc92b48fc72550b40076b005779faf0c1fb8ad72d728dc18c1e33540faad";
const OPERATION: &str = "mpegts-access-unit-packets";
const PROGRAM: u16 = 7;
const PID: u16 = 256;
const CLOCK_MODULUS: u64 = 1 << 33;
const DVB_EAC3: &str = "harness/fixtures/hls/dvb-hevc-eac3-32000.ts";

const ATSC_EAC3_SEED: &str = "harness/fixtures/hls/replay-avc-eac3-00.ts";

const SOURCES: [(&str, &str); 9] = [
    (
        "crates/framing/fixtures/raw-annex-b/avc-aud0.h264",
        "96d3d39385d7a86463410e0e4f00377062a815532134805e2a552549954b6796",
    ),
    (
        "crates/framing/fixtures/raw-annex-b/avc-aud0.packets",
        "46d41faee935ed2a0076ac1ed9f3d4ec9ca38f0ffd6ac6374ef34d679454086f",
    ),
    (
        "crates/framing/fixtures/raw-annex-b/hevc-aud0.hevc",
        "7c076acbe5c6cf3db1939534527b1d52fd7f4c31a2063f537440f51c75b08f70",
    ),
    (
        "crates/framing/fixtures/raw-annex-b/hevc-aud0.packets",
        "ee2888ab6e9b6256177708023c8966c615b8feb90a6c52dc6f2a9a1d1c6aa4e1",
    ),
    (
        "crates/media/tests/fixtures/cue-ties/aac-packet.bin",
        "4315bdf0b9ac88eaad1c09d9c4c7a2a934a3500904cace032ef527fbe4a30277",
    ),
    (
        "harness/fixtures/avc-aac-sub-reference.json",
        "531f8e47a7f1f54e8ea8b73ab5b7899ba01b0380c2892cb17a12a2b1c369cde1",
    ),
    (
        "harness/fixtures/hls/replay-avc-ac3-00.ts",
        "7efdc50ca87d044af2af8bfe5afd1e26c25daeca0e948193bc7e180db11a2510",
    ),
    (
        DVB_EAC3,
        "67fbe16dea5d0466b122b69d4d03c46f2740080c5fa176fb4a0c3e4709e87969",
    ),
    (
        ATSC_EAC3_SEED,
        "330ce3642fee3469a310400097b9889d751943d4185e754a160646eaf67ab133",
    ),
];

/// Inputs shared by the worker build and report identities.
pub fn source_inputs() -> Vec<String> {
    [
        "xtask/src/transport_fixtures.rs",
        "harness/features/mpegts-access-units/contract.json",
        "harness/features/mpegts-access-units/corpus.json",
    ]
    .into_iter()
    .chain(SOURCES.iter().map(|(path, _)| *path))
    .map(str::to_owned)
    .collect()
}

fn text<'a>(v: &'a Value, key: &str) -> R<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn cases(v: &Value) -> R<&[Value]> {
    v.get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing cases".into())
}

fn entities(row: &Value) -> &[Value] {
    row.get("entities")
        .and_then(Value::as_array)
        .unwrap_or_default()
}

fn checked_input(root: &Path, record: &Value) -> R<String> {
    let path = text(record, "file")?;
    let raw = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
    if sha256::hex(&raw) != text(record, "sha256")? {
        return Err(format!("original input changed: {path}"));
    }
    Ok(path.to_owned())
}

/// Bind every entity consumed by the reference, including non-primary files.
pub fn entity_inputs(root: &Path) -> R<Vec<String>> {
    let manifest = read_json(&root.join(ORIGINAL))?;
    cases(&manifest)?
        .iter()
        .flat_map(entities)
        .map(|entity| checked_input(root, entity))
        .collect()
}

fn put(v: &mut Value, key: &str, value: Value) -> R<()> {
    let Value::Object(fields) = v else {
        return Err("expected an object".into());
    };
    fields.retain(|(k, _)| k != key);
    fields.push((key.into(), value));
    Ok(())
}

fn source(root: &Path, path: &str) -> R<Vec<u8>> {
    let raw = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
    let expected = SOURCES
        .iter()
        .find(|(p, _)| *p == path)
        .ok_or_else(|| format!("unregistered source: {path}"))?
        .1;
    if sha256::hex(&raw) != expected {
        return Err(format!("fixture source changed: {path}"));
    }
    Ok(raw)
}

struct Case {
    row: Value,
    bytes: Option<Vec<u8>>,
    following: Vec<(String, Vec<u8>)>,
}

fn original_inputs(mut manifest: Value) -> R<Value> {
    let Value::Object(fields) = &mut manifest else {
        return Err("invalid original PES manifest".into());
    };
    let Some((_, Value::Array(rows))) = fields.iter_mut().find(|(key, _)| key == "cases") else {
        return Err("missing original PES cases".into());
    };
    for row in rows {
        let Value::Object(fields) = row else {
            return Err("invalid original PES case".into());
        };
        fields.retain(|(key, _)| !matches!(key.as_str(), "expected" | "reference_refusal"));
    }
    if sha256::hex(json::canonical(&manifest).as_bytes()) != ORIGINAL_INPUTS_SHA {
        return Err("the original PES inputs or request associations changed".into());
    }
    Ok(manifest)
}

fn original(root: &Path) -> R<Vec<Case>> {
    let manifest = original_inputs(read_json(&root.join(ORIGINAL))?)?;
    let rows = cases(&manifest)?;
    let requests: usize = rows
        .iter()
        .map(|c| {
            c.get("requests")
                .and_then(Value::as_array)
                .map_or(0, <[_]>::len)
        })
        .sum();
    if rows.len() != 97 || requests != 272 {
        return Err("all 97 original cases and 272 requests are required".into());
    }
    rows.iter()
        .map(|row| {
            for input in std::iter::once(row).chain(entities(row)) {
                checked_input(root, input)?;
            }
            Ok(Case {
                row: row.clone(),
                bytes: None,
                following: Vec::new(),
            })
        })
        .collect()
}

// Independent fixture writer: H.222.0 PAT/PMT CRC, adaptation stuffing,
// continuity counters and finite PES.
struct Transport {
    bytes: Vec<u8>,
    counters: [u8; 8192],
}

impl Transport {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            counters: [0; 8192],
        }
    }

    fn payload(&mut self, pid: u16, raw: &[u8]) {
        for (index, chunk) in raw.chunks(184).enumerate() {
            let mut packet = [0xff; 188];
            let cc = &mut self.counters[usize::from(pid)];
            packet[..4].copy_from_slice(&[
                0x47,
                ((pid >> 8) as u8) | if index == 0 { 0x40 } else { 0 },
                pid as u8,
                0x10 | *cc,
            ]);
            *cc = (*cc + 1) & 15;
            let at = if chunk.len() == 184 {
                4
            } else {
                packet[3] |= 0x20;
                packet[4] = (183 - chunk.len()) as u8;
                if packet[4] != 0 {
                    packet[5] = 0;
                }
                188 - chunk.len()
            };
            packet[at..].copy_from_slice(chunk);
            self.bytes.extend_from_slice(&packet);
        }
    }

    fn section(&mut self, pid: u16, mut section: Vec<u8>) {
        let mut crc = u32::MAX;
        for byte in &section {
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
        section.extend_from_slice(&crc.to_be_bytes());
        section.insert(0, 0);
        self.payload(pid, &section);
    }

    fn tables(&mut self, codec: &Codec, descriptors: &[u8]) -> R<()> {
        self.tables_for(&[(codec, PID, descriptors)])
    }

    fn tables_for(&mut self, streams: &[(&Codec, u16, &[u8])]) -> R<()> {
        for (codec, _, descriptors) in streams {
            if codec.dvb
                && (descriptors.len() < 3
                    || descriptors[0] != 0x6a
                    || usize::from(descriptors[1]) + 2 != descriptors.len())
            {
                return Err("DVB AC-3 descriptor requires its flags byte".into());
            }
        }
        self.section(
            0,
            vec![0, 0xb0, 13, 0, 9, 0xc1, 0, 0, 0, PROGRAM as u8, 0xf0, 0],
        );
        let mut pmt = vec![
            2,
            0xb0,
            (13 + streams.iter().map(|(_, _, d)| 5 + d.len()).sum::<usize>()) as u8,
            0,
            PROGRAM as u8,
            0xc1,
            0,
            0,
            0xe1,
            0,
            0xf0,
            0,
        ];
        for (codec, pid, descriptors) in streams {
            pmt.extend_from_slice(&[
                codec.stream_type,
                0xe0 | (pid >> 8) as u8,
                *pid as u8,
                0xf0,
                descriptors.len() as u8,
            ]);
            pmt.extend_from_slice(descriptors);
        }
        self.section(4096, pmt);
        Ok(())
    }

    fn pes(&mut self, stream_id: u8, part: &Part<'_>) -> R<()> {
        self.pes_at(PID, stream_id, part)
    }

    fn pes_at(&mut self, pid: u16, stream_id: u8, part: &Part<'_>) -> R<()> {
        let mut optional = Vec::new();
        for (value, prefix) in [
            (part.pts, if part.dts.is_some() { 3 } else { 2 }),
            (part.dts, 1),
        ] {
            if let Some(value) = value {
                if value >= CLOCK_MODULUS {
                    return Err("clock exceeds 33 source bits".into());
                }
                optional.extend_from_slice(&[
                    (prefix << 4) | ((value >> 29) as u8 & 0x0e) | 1,
                    (value >> 22) as u8,
                    ((value >> 14) as u8 & 0xfe) | 1,
                    (value >> 7) as u8,
                    ((value << 1) as u8 & 0xfe) | 1,
                ]);
            }
        }
        let length = if part.unbounded {
            0
        } else {
            u16::try_from(part.body.len() + optional.len() + 3).map_err(|e| e.to_string())?
        };
        let mut pes = vec![
            0,
            0,
            1,
            stream_id,
            (length >> 8) as u8,
            length as u8,
            0x80,
            if part.dts.is_some() {
                0xc0
            } else if part.pts.is_some() {
                0x80
            } else {
                0
            },
            optional.len() as u8,
        ];
        pes.extend(optional);
        pes.extend_from_slice(part.body);
        self.payload(pid, &pes);
        Ok(())
    }

    fn finish(self, m2ts: bool) -> Vec<u8> {
        if !m2ts {
            return self.bytes;
        }
        let mut out = Vec::new();
        for (index, packet) in self.bytes.chunks_exact(188).enumerate() {
            out.extend_from_slice(&(0x8000_0000 | index as u32).to_be_bytes());
            out.extend_from_slice(packet);
        }
        out
    }
}

struct Codec {
    name: &'static str,
    stream_type: u8,
    stream_id: u8,
    dvb: bool,
    bytes: Vec<u8>,
    first: usize,
    header: usize,
}

struct Part<'a> {
    body: &'a [u8],
    pts: Option<u64>,
    dts: Option<u64>,
    unbounded: bool,
}

fn part(body: &[u8], pts: Option<u64>, dts: Option<u64>) -> Part<'_> {
    Part {
        body,
        pts,
        dts,
        unbounded: false,
    }
}

fn authored_case(codec: &Codec, name: &str, parts: &[Part<'_>], m2ts: bool) -> R<Case> {
    let id = format!("au-{}-{name}", codec.name);
    let mut ts = Transport::new();
    let descriptors: &[u8] = match codec.stream_type {
        0x06 if codec.dvb => &[0x6a, 1, 0],
        // A/52 Annex A Table A4.1 and Annex G Table G.1.
        0x81 => &[0x81, 3, 8, 24, 5],
        0x87 => &[0xcc, 2, 0x80, 0xc2],
        _ => &[],
    };
    ts.tables(codec, descriptors)?;
    for p in parts {
        ts.pes(codec.stream_id, p)?;
    }
    let bytes = ts.finish(m2ts);
    let file = format!(
        ".research/{DIRECTORY}/{id}.{}",
        if m2ts { "m2ts" } else { "ts" }
    );
    let mut args = vec![
        if m2ts { "192" } else { "188" }.to_owned(),
        PROGRAM.to_string(),
    ];
    if codec.dvb {
        args.push("--dvb".into());
    } else if matches!(codec.stream_type, 0x81 | 0x87) {
        args.push("--atsc".into());
    }
    let construction = Value::object([
        ("codec", Value::str(codec.name)),
        (
            "pes",
            Value::Array(
                parts
                    .iter()
                    .map(|p| {
                        Value::object(
                            [
                                ("payload_bytes", Value::int(p.body.len() as u64)),
                                ("pts_raw", p.pts.map_or(Value::Null, Value::int)),
                                ("dts_raw", p.dts.map_or(Value::Null, Value::int)),
                            ]
                            .into_iter()
                            .chain(p.unbounded.then_some(("packet_length", Value::int(0)))),
                        )
                    })
                    .collect(),
            ),
        ),
    ]);
    Ok(Case {
        row: authored(
            &id,
            &file,
            &bytes,
            vec![
                ("arguments", Value::strings(args)),
                ("construction", construction),
            ],
        ),
        bytes: Some(bytes),
        following: Vec::new(),
    })
}

fn video(root: &Path, hevc: bool) -> R<Codec> {
    let name = if hevc { "hevc" } else { "avc" };
    let stem = format!("crates/framing/fixtures/raw-annex-b/{name}-aud0");
    let bytes = source(
        root,
        &format!("{stem}.{}", if hevc { "hevc" } else { "h264" }),
    )?;
    let ranges = source(root, &format!("{stem}.packets"))?;
    let ends: Vec<usize> = String::from_utf8(ranges)
        .map_err(|e| e.to_string())?
        .lines()
        .take(2)
        .map(|line| {
            let mut words = line.split_ascii_whitespace();
            let start: usize = words
                .next()
                .ok_or("missing AU offset")?
                .parse()
                .map_err(|e| format!("{e}"))?;
            let length: usize = words
                .next()
                .ok_or("missing AU length")?
                .parse()
                .map_err(|e| format!("{e}"))?;
            start
                .checked_add(length)
                .ok_or_else(|| "AU extent overflow".into())
        })
        .collect::<R<_>>()?;
    if ends.len() != 2 || ends[0] >= ends[1] || ends[1] > bytes.len() {
        return Err("invalid raw AU extents".into());
    }
    let header = (0..ends[0].saturating_sub(4))
        .find(|&i| {
            let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if bytes[i..].starts_with(&[0, 0, 1]) {
                3
            } else {
                return false;
            };
            let kind = if hevc {
                (bytes[i + prefix] >> 1) & 63
            } else {
                bytes[i + prefix] & 31
            };
            if hevc {
                kind < 32
            } else {
                matches!(kind, 1..=5)
            }
        })
        .ok_or("no coded picture in source AU")?;
    Ok(Codec {
        name,
        stream_type: if hevc { 0x24 } else { 0x1b },
        stream_id: 0xe0,
        dvb: false,
        bytes: bytes[..ends[1]].to_vec(),
        first: ends[0],
        header,
    })
}

// Read only the first finite audio PES of this pinned TS seed. This is fixture
// extraction, independent of the production demuxer and the comparison worker.
fn first_audio_pes(raw: &[u8]) -> R<Vec<u8>> {
    let mut pes = Vec::new();
    for packet in raw.chunks_exact(188) {
        if packet[0] != 0x47 {
            return Err("seed lost TS sync".into());
        }
        let pid = (u16::from(packet[1] & 31) << 8) | u16::from(packet[2]);
        if pid != 257 || packet[3] & 0x10 == 0 {
            continue;
        }
        let start = packet[1] & 0x40 != 0;
        if start && !pes.is_empty() {
            break;
        }
        if !start && pes.is_empty() {
            continue;
        }
        let at = 4 + if packet[3] & 0x20 != 0 {
            1 + usize::from(packet[4])
        } else {
            0
        };
        pes.extend_from_slice(packet.get(at..).ok_or("seed adaptation field overrun")?);
    }
    if pes.len() < 9 || !pes.starts_with(&[0, 0, 1, 0xbd]) {
        return Err("seed has no private audio PES".into());
    }
    let end = 6 + usize::from(u16::from_be_bytes([pes[4], pes[5]]));
    let at = 9 + usize::from(pes[8]);
    pes.get(at..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| "incomplete seed audio PES".into())
}

fn audio(root: &Path, ac3: bool) -> R<Codec> {
    let frame = if ac3 {
        let pes = first_audio_pes(&source(root, SOURCES[6].0)?)?;
        if pes.len() < 7 || !pes.starts_with(&[0x0b, 0x77]) || pes[4] >> 6 != 0 {
            return Err("expected a 48 kHz AC-3 syncframe".into());
        }
        let rate = [
            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
        ]
        .get(usize::from((pes[4] & 63) >> 1))
        .copied()
        .ok_or("invalid AC-3 frame size")?;
        pes.get(..rate * 4)
            .ok_or("incomplete AC-3 syncframe")?
            .to_vec()
    } else {
        let body = source(root, SOURCES[4].0)?;
        let trace = source(root, SOURCES[5].0)?;
        let trace = json::parse(std::str::from_utf8(&trace).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let trace = trace.get("data").ok_or("missing source data")?;
        let tracks = trace
            .get("tracks")
            .and_then(Value::as_array)
            .ok_or("missing source tracks")?;
        if !tracks
            .iter()
            .any(|t| t.get("private").and_then(Value::as_str) == Some("118856e500"))
        {
            return Err("AAC source is no longer LC/48kHz/mono".into());
        }
        let packet = trace
            .get("packets")
            .and_then(Value::as_array)
            .and_then(|packets| {
                packets
                    .iter()
                    .find(|packet| packet.get("track") == Some(&Value::int(1)))
            })
            .ok_or("missing source AAC packet")?;
        let payload: String = body.iter().map(|byte| format!("{byte:02x}")).collect();
        if packet.get("payload").and_then(Value::as_str) != Some(payload.as_str()) {
            return Err("AAC raw bytes differ from the recorded LC source".into());
        }
        let length = body.len() + 7;
        let mut frame = vec![
            0xff,
            0xf1,
            0x4c,
            0x40 | (length >> 11) as u8,
            (length >> 3) as u8,
            ((length & 7) << 5) as u8 | 0x1f,
            0xfc,
        ];
        frame.extend(body);
        frame
    };
    Ok(Codec {
        name: if ac3 { "dvb-ac3" } else { "adts-aac-lc" },
        stream_type: if ac3 { 6 } else { 15 },
        stream_id: if ac3 { 0xbd } else { 0xc0 },
        dvb: ac3,
        first: frame.len(),
        header: 7,
        bytes: frame.repeat(2),
    })
}

fn direct_audio_cases(root: &Path) -> R<[Case; 2]> {
    let ac3 = Codec {
        name: "atsc-ac3",
        stream_type: 0x81,
        dvb: false,
        ..audio(root, true)?
    };
    let eac3_pes = first_audio_pes(&source(root, ATSC_EAC3_SEED)?)?;
    let [0x0b, 0x77, size_high, size_low, flags, ..] = eac3_pes.as_slice() else {
        return Err("expected a coded E-AC-3 syncframe".into());
    };
    // Independent substream zero, 48 kHz, six blocks, stereo.
    if *size_high & 0xf8 != 0 || *flags != 0x34 {
        return Err("unexpected E-AC-3 syncframe format".into());
    }
    let frame_bytes = 2 * (1 + usize::from(u16::from_be_bytes([size_high & 7, *size_low])));
    let frame = eac3_pes
        .get(..frame_bytes)
        .ok_or("incomplete E-AC-3 seed syncframe")?;
    let eac3 = Codec {
        name: "atsc-eac3",
        stream_type: 0x87,
        stream_id: 0xbd,
        dvb: false,
        bytes: frame.repeat(2),
        first: frame_bytes,
        header: 7,
    };
    Ok([
        authored_case(
            &ac3,
            "multiple-per-pes",
            &[part(&ac3.bytes, Some(90_000), None)],
            false,
        )?,
        authored_case(
            &eac3,
            "multiple-per-pes",
            &[part(&eac3.bytes, Some(90_000), None)],
            false,
        )?,
    ])
}

fn interleaved_audio_case(ac3: &Codec, aac: &Codec, second_ac3: bool) -> R<Case> {
    let mut ts = Transport::new();
    ts.tables_for(&[(ac3, PID, &[0x6a, 1, 0]), (aac, PID + 1, &[])])?;
    let mut packets = vec![(ac3, PID, 90_000), (aac, PID + 1, 6_000_000)];
    if second_ac3 {
        packets.push((ac3, PID, 92_880));
    }
    let mut construction = Vec::new();
    for (codec, pid, pts) in packets {
        ts.pes_at(
            pid,
            codec.stream_id,
            &part(&codec.bytes[..codec.first], Some(pts), None),
        )?;
        construction.push(Value::object([
            ("pid", Value::int(u64::from(pid))),
            ("codec", Value::str(codec.name)),
            ("payload_bytes", Value::int(codec.first as u64)),
            ("pts_raw", Value::int(pts)),
            ("dts_raw", Value::Null),
        ]));
    }
    let id = if second_ac3 {
        "au-dvb-ac3-aac-interleaved-lookahead"
    } else {
        "au-dvb-ac3-aac-interleaved-eof"
    };
    let bytes = ts.finish(false);
    Ok(Case {
        row: authored(
            id,
            &format!(".research/{DIRECTORY}/{id}.ts"),
            &bytes,
            vec![
                ("arguments", Value::strings(["188", "7", "--dvb"])),
                (
                    "construction",
                    Value::object([("pes", Value::Array(construction))]),
                ),
            ],
        ),
        bytes: Some(bytes),
        following: Vec::new(),
    })
}

fn coded_input<'a>(cases: &'a [Case], id: &str) -> R<&'a [u8]> {
    cases
        .iter()
        .find(|case| case.row.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|case| case.bytes.as_deref())
        .ok_or_else(|| format!("missing coded input {id}"))
}

fn ordered_case(id: &str, stride: usize, dvb: bool, segments: &[(&[u8], bool)]) -> R<Case> {
    let [(primary, false), rest @ ..] = segments else {
        return Err("ordered case needs an initial entity".into());
    };
    let extension = match stride {
        188 => "ts",
        192 => "m2ts",
        _ => return Err("unsupported ordered carrier size".into()),
    };
    if rest.is_empty()
        || segments
            .iter()
            .any(|(bytes, _)| bytes.is_empty() || !bytes.len().is_multiple_of(stride))
    {
        return Err("ordered case needs complete carriers in every entity".into());
    }
    let primary_path = format!(".research/{DIRECTORY}/{id}-0.{extension}");
    let mut args = vec![stride.to_string(), PROGRAM.to_string()];
    if dvb {
        args.push("--dvb".into());
    }
    let mut entity_records = vec![Value::object([
        ("file", Value::str(&primary_path)),
        ("sha256", Value::str(sha256::hex(primary))),
        ("discontinuity", Value::Bool(false)),
    ])];
    let mut following = Vec::with_capacity(rest.len());
    for (index, (bytes, discontinuity)) in rest.iter().enumerate() {
        let file_name = format!("{id}-{}.{extension}", index + 1);
        let path = format!(".research/{DIRECTORY}/{file_name}");
        if *discontinuity {
            args.push("--discontinuity".into());
        }
        args.push(file_name);
        entity_records.push(Value::object([
            ("file", Value::str(&path)),
            ("sha256", Value::str(sha256::hex(bytes))),
            ("discontinuity", Value::Bool(*discontinuity)),
        ]));
        following.push((path, bytes.to_vec()));
    }
    Ok(Case {
        row: authored(
            id,
            &primary_path,
            primary,
            vec![
                ("arguments", Value::strings(args)),
                ("entities", Value::Array(entity_records)),
            ],
        ),
        bytes: Some(primary.to_vec()),
        following,
    })
}

fn ordered_cases(cases: &[Case]) -> R<[Case; 4]> {
    let adts = coded_input(cases, "au-adts-aac-lc-body-across-pes")?;
    let m2ts = coded_input(cases, "au-adts-aac-lc-m2ts192")?;
    let ac3_single = coded_input(cases, "au-dvb-ac3-one")?;
    let ac3_multiple = coded_input(cases, "au-dvb-ac3-multiple-per-pes")?;
    let ac3_next = coded_input(cases, "au-dvb-ac3-final-au-at-eof")?;
    let (initialization, adts) = adts
        .split_at_checked(2 * 188)
        .ok_or("ADTS initialization missing")?;
    let (start, end) = adts
        .split_at_checked(188)
        .ok_or("ADTS continuation missing")?;
    let (map, media) = m2ts
        .split_at_checked(2 * 192)
        .ok_or("M2TS initialization missing")?;
    Ok([
        ordered_case(
            "au-adts-across-entities",
            188,
            false,
            &[(initialization, false), (start, false), (end, false)],
        )?,
        ordered_case(
            "au-m2ts-initialization-entity",
            192,
            false,
            &[(map, false), (media, false)],
        )?,
        ordered_case(
            "au-dvb-ac3-discontinuity-single-frame",
            188,
            true,
            &[(ac3_single, false), (ac3_next, true)],
        )?,
        ordered_case(
            "au-dvb-ac3-discontinuity-two-frames",
            188,
            true,
            &[(ac3_multiple, false), (ac3_next, true)],
        )?,
    ])
}

fn adts_fragment_cases(codec: &Codec) -> R<[Case; 5]> {
    let frame = &codec.bytes[..codec.first];
    let before = b"opaque-before";
    let after = b"opaque-after";
    let prefix = [&before[..], frame].concat();
    let suffix = [frame, &after[..]].concat();
    let first = |bytes| part(bytes, Some(90_000), None);
    let next = |bytes| part(bytes, Some(180_000), None);
    Ok([
        authored_case(
            codec,
            "two-opaque-pes",
            &[first(before), next(after)],
            false,
        )?,
        authored_case(
            codec,
            "opaque-then-coded",
            &[first(before), next(frame)],
            false,
        )?,
        authored_case(
            codec,
            "coded-then-opaque",
            &[first(frame), next(after)],
            false,
        )?,
        authored_case(codec, "opaque-prefix-in-pes", &[first(&prefix)], false)?,
        authored_case(codec, "opaque-suffix-in-pes", &[first(&suffix)], false)?,
    ])
}

fn adts_unbounded_cases(codec: &Codec) -> R<[Case; 4]> {
    let frame = &codec.bytes[..codec.first];
    let mixed = [b"opaque prefix".as_slice(), frame].concat();
    let unbounded = |body| Part {
        unbounded: true,
        ..part(body, Some(90_000), None)
    };
    let finite = |body| part(body, Some(180_000), Some(179_000));
    Ok([
        authored_case(codec, "unbounded-at-eof", &[unbounded(frame)], false)?,
        authored_case(
            codec,
            "unbounded-before-pes",
            &[unbounded(frame), finite(frame)],
            false,
        )?,
        authored_case(
            codec,
            "unbounded-split-frame",
            &[unbounded(&frame[..3]), finite(&frame[3..])],
            false,
        )?,
        authored_case(
            codec,
            "unbounded-opaque-prefix",
            &[unbounded(&mixed)],
            false,
        )?,
    ])
}

fn loas_element(aac: &[u8], reuse_config: bool) -> R<Vec<u8>> {
    // ISO/IEC 14496-3 §1.7.3: one stream, one subframe and a byte length.
    let mut mux = Vec::new();
    let mut used = 0;
    let mut write = |value: u16, width: u8| {
        for shift in (0..width).rev() {
            if used == 0 {
                mux.push(0);
            }
            *mux.last_mut().unwrap() |= ((value >> shift & 1) as u8) << (7 - used);
            used = (used + 1) % 8;
        }
    };
    write(u16::from(reuse_config), 1);
    if !reuse_config {
        write(0, 1); // audioMuxVersion
        write(1, 1); // allStreamsSameTimeFraming
        write(0, 6 + 4 + 3); // subframe, program and layer counts minus one
        write(0x1188, 16); // AAC-LC, 48 kHz, mono, 1024 samples
        write(0, 3); // frameLengthType
        write(255, 8); // latmBufferFullness
        write(0, 2); // no other data or CRC
    }
    let mut chunks = aac.chunks_exact(255);
    for _ in &mut chunks {
        write(255, 8);
    }
    write(chunks.remainder().len() as u16, 8);
    for &byte in aac {
        write(u16::from(byte), 8);
    }
    let length = u16::try_from(mux.len()).map_err(|_| "LOAS element too large")?;
    if length > 0x1fff {
        return Err("LOAS element too large".into());
    }
    let [high, low] = length.to_be_bytes();
    let mut envelope = vec![0x56, 0xe0 | high, low];
    envelope.extend(mux);
    Ok(envelope)
}

fn latm_cases(aac: &Codec) -> R<[Case; 8]> {
    let raw = &aac.bytes[aac.header..aac.first];
    let first = loas_element(raw, false)?;
    let next = loas_element(raw, true)?;
    let codec = Codec {
        name: "latm-aac-lc",
        stream_type: 0x11,
        stream_id: 0xc0,
        dvb: false,
        first: first.len(),
        header: 3,
        bytes: [&first[..], &next, &next].concat(),
    };
    let opaque = b"opaque multiplex bytes";
    let prefix = [&opaque[..], &first].concat();
    let start = |bytes| part(bytes, Some(90_000), None);
    let later = |bytes| part(bytes, Some(180_000), Some(179_000));
    Ok([
        authored_case(&codec, "three-frames-in-pes", &[start(&codec.bytes)], false)?,
        authored_case(
            &codec,
            "header-across-pes",
            &[start(&codec.bytes[..2]), later(&codec.bytes[2..])],
            false,
        )?,
        authored_case(&codec, "opaque-prefix-in-pes", &[start(&prefix)], false)?,
        authored_case(
            &codec,
            "opaque-then-coded",
            &[start(opaque), later(&first)],
            false,
        )?,
        authored_case(
            &codec,
            "coded-then-opaque",
            &[start(&first), later(opaque)],
            false,
        )?,
        authored_case(
            &codec,
            "two-opaque-pes",
            &[start(opaque), part(b"tail", None, None)],
            false,
        )?,
        authored_case(
            &codec,
            "truncated-envelope",
            &[start(&first[..first.len() - 1])],
            false,
        )?,
        authored_case(&codec, "empty-envelope", &[start(&[0x56, 0xe0, 0])], false)?,
    ])
}

fn startup_pointer_case(codec: &Codec, table: &str, offset: usize) -> R<Case> {
    let frame = &codec.bytes[..codec.first];
    let mut case = authored_case(
        codec,
        &format!("recover-{table}-pointer"),
        &[part(frame, Some(90_000), None)],
        false,
    )?;
    let bytes = case.bytes.as_mut().ok_or("missing authored transport")?;
    let clean = bytes
        .get_mut(offset..offset + 188)
        .ok_or("missing PSI carrier")?;
    let mut rejected = clean.to_vec();
    if rejected[3] & 0x30 != 0x30 || rejected[1] & 0x40 == 0 {
        return Err("expected an aligned PSI start with an adaptation field".into());
    }
    let pointer = 5 + usize::from(rejected[4]);
    if rejected.get(pointer) != Some(&0) {
        return Err("expected a zero PSI pointer".into());
    }
    rejected[pointer] = 255;
    clean[3] = (clean[3] & 0xf0) | ((clean[3] + 1) & 15);
    bytes.splice(offset..offset, rejected);
    put(&mut case.row, "sha256", Value::str(sha256::hex(bytes)))?;
    let construction = case
        .row
        .get("construction")
        .cloned()
        .ok_or("missing PES recipe")?;
    let mut construction = construction;
    put(
        &mut construction,
        "discarded_pointer_carrier",
        Value::str(table),
    )?;
    put(&mut case.row, "construction", construction)?;
    Ok(case)
}

fn all_cases(root: &Path) -> R<Vec<Case>> {
    let mut cases = original(root)?;
    for codec in [
        video(root, false)?,
        video(root, true)?,
        audio(root, false)?,
        audio(root, true)?,
    ] {
        let one = &codec.bytes[..codec.first];
        let two = codec.bytes.as_slice();
        let second = &two[codec.first..];
        let middle = one.len() / 2;
        let timed = |body| part(body, Some(90_000), None);
        let untimed = |body| part(body, None, None);
        for (name, parts, m2ts) in [
            ("one", vec![timed(one)], false),
            ("multiple-per-pes", vec![timed(two)], false),
            (
                "header-across-pes",
                vec![timed(&one[..2]), untimed(&one[2..])],
                false,
            ),
            (
                "body-across-pes",
                vec![timed(&one[..middle]), untimed(&one[middle..])],
                false,
            ),
            ("header-only-eof", vec![timed(&one[..codec.header])], false),
            ("final-au-at-eof", vec![timed(one), untimed(second)], false),
            ("truncated-final", vec![timed(&two[..two.len() - 1])], false),
            ("missing-clocks", vec![untimed(two)], false),
            (
                "pts-dts",
                vec![
                    part(one, Some(90_000), Some(87_000)),
                    part(second, Some(93_000), Some(90_000)),
                ],
                false,
            ),
            (
                "clock-wrap",
                vec![
                    part(one, Some(CLOCK_MODULUS - 1000), None),
                    part(second, Some(2000), None),
                ],
                false,
            ),
            ("m2ts192", vec![timed(two)], true),
        ] {
            cases.push(authored_case(&codec, name, &parts, m2ts)?);
        }
    }
    let ac3 = audio(root, true)?;
    let aac = audio(root, false)?;
    for second_ac3 in [false, true] {
        cases.push(interleaved_audio_case(&ac3, &aac, second_ac3)?);
    }
    let ordered = ordered_cases(&cases)?;
    cases.extend(ordered);
    let dvb_eac3 = source(root, DVB_EAC3)?;
    cases.push(Case {
        row: authored(
            "au-dvb-eac3-32000",
            DVB_EAC3,
            &dvb_eac3,
            vec![("arguments", Value::strings(["188", "1", "--dvb"]))],
        ),
        bytes: None,
        following: Vec::new(),
    });
    cases.extend(direct_audio_cases(root)?);
    cases.extend(adts_fragment_cases(&aac)?);
    cases.push(startup_pointer_case(&aac, "pat", 0)?);
    cases.push(startup_pointer_case(&aac, "pmt", 188)?);
    cases.extend(latm_cases(&aac)?);
    cases.extend(adts_unbounded_cases(&aac)?);
    Ok(cases)
}

fn contract(cases: &[Case]) -> R<Value> {
    Ok(Value::object([
        ("schema", Value::int(1)),
        ("id", Value::str("mpegts-access-units")),
        ("operation", Value::str(OPERATION)),
        ("candidate", Value::str("transport-worker")),
        ("worker_operation", Value::str("ts-au-")),
        ("reference", Value::str("ffmpeg-formats-native")),
        (
            "contract",
            Value::strings([
                "Preloaded TS188 or M2TS192 input, SIZE PROGRAM [--dvb|--atsc] and the original ordered media-entity arguments; the candidate uses the public MPEG-TS access-unit reader, with no PES-as-AU fallback.",
                "Reference: the MPEG-TS demuxer with custom AVIO, NOFILLIN, parsers enabled, no stream discovery and no decoders; every parsed packet of the selected program in demux order, including EOF parser drain.",
                "Contiguous entities and an initialization map continue within one reference format context without intermediate EOF. An explicit discontinuity blocks read-ahead before the next entity, flushes parser state through the public API and resumes with fresh AVIO over the same byte buffer. Only the final source EOF drains the parser; packet positions use logical offsets across the ordered original bytes.",
                "Fold stream index, PID, signed PTS and DTS in 90kHz ticks (absent as INT64_MIN), duration, position, flags, payload length and every byte, then every side-data element's type, size and bytes.",
                "Each complete operation reaches EOF and contributes its packet count, byte count and ordered digest. Both workers must match the independently recorded parser-enabled reference fold. All temporary operation storage is released before the memory END marker.",
                "The 97 retained PES input/argument tuples preserve their 272 request associations and use parser-enabled AU expectations. Opaque, damaged and unsupported inputs remain observations that may fail; they are never reclassified as genuine access units or removed.",
                "Additional controls reuse pinned coded AVC, HEVC, AAC-LC and AC-3 bytes with independently authored PAT/PMT, PES packetization and source clocks. They include multiple AUs per PES, one AU across PES, header-only and truncated EOF, missing clocks, PTS/DTS, 33-bit wrap, M2TS192 and interleaved AC-3/AAC source clocks with delayed AC-3 output. Ordered controls cover ADTS across entities, an M2TS initialization section and AC-3 lookahead at a discontinuity. DVB AC-3 has explicit caller-declared descriptor namespace.",
                "The coded 32 kHz DVB E-AC-3 control reuses the complete TS188 fixture. It compares both streams in the selected program with an explicit caller-declared DVB namespace.",
                "The ATSC audio controls use direct private stream types 0x81 and 0x87 with two complete syncframes in one PES and an explicit caller-declared ATSC namespace.",
                "The ADTS fragment controls compare opaque bytes across consecutive PES packets, leading bytes before a complete envelope and trailing bytes at logical EOF, including shared and separate PES timestamps. The candidate uses the ADTS packet policy and keeps fragments separate from complete access units.",
                "The Annex B EOF policy can report an unframed run of validated AVC or HEVC PES bytes with no start code. The candidate folds that observation as opaque parser output, separately from complete video access units.",
                "The startup PSI controls put an out-of-range pointer before a clean PAT or PMT and require the selected program's coded ADTS packet through physical EOF.",
                "LATM controls wrap the pinned AAC-LC payload in independently authored LOAS envelopes and also carry opaque or truncated envelopes. The comparison uses the explicit LATM packet policy: the reference emits one unsplit packet per nonempty finite PES with no parser flags or duration. The reader reports each as a source observation outside its strict LOAS frame stream.",
                "The unbounded ADTS controls use a zero PES packet length with complete coded frames, an opaque prefix and a frame split across PES. The comparison selects the ADTS PES policy, validates the remainder of the original source header and applies the same bounded ADTS parser through the next PES start or logical EOF.",
                "A bounded AVC or HEVC continuity residue uses the original carrier order, source transitions and logical EOF. Selected TS188 AVC transport and PES faults use a sequential replay that reproduces the diagnostic and reaches physical End and source Finished. Both policies require a nonempty run without an Annex B start code and publish the original bytes and clock facts outside the neutral packet stream.",
            ]),
        ),
        (
            "required_profiles",
            Value::strings(
                cases
                    .iter()
                    .map(|c| text(&c.row, "profile"))
                    .collect::<R<Vec<_>>>()?,
            ),
        ),
        (
            "required_targets",
            Value::strings(["aarch64-unknown-linux-gnu"]),
        ),
        ("iterations", Value::int(50)),
        ("minimum_pairs", Value::int(30)),
        ("confidence", Value::float(0.95)),
    ]))
}

#[derive(Clone, Copy)]
struct Recording<'a> {
    worker: &'a Path,
    directory: &'a Path,
}

fn reference(root: &Path, input: &Path, row: &Value, recording: Recording<'_>) -> R<Value> {
    let id = text(row, "id")?;
    let mut argv: Vec<OsString> = vec![recording.worker.into(), "ts-au-fold".into(), input.into()];
    argv.extend(crate::harness::report::case_arguments(row));
    let Some(done) = worker::run_in(&argv, &[], Some(root), worker::DEFAULT_TIMEOUT)? else {
        let timeout = Value::object([("status", Value::str("timeout"))]);
        write_json(&recording.directory.join(format!("{id}.json")), &timeout)?;
        return Ok(timeout);
    };
    save_reference_result(id, recording.directory, &argv, &done)
}

fn save_reference_result(
    id: &str,
    directory: &Path,
    argv: &[OsString],
    done: &worker::Finished,
) -> R<Value> {
    fs::write(directory.join(format!("{id}.stdout")), &done.stdout).map_err(|e| e.to_string())?;
    fs::write(directory.join(format!("{id}.stderr")), &done.stderr).map_err(|e| e.to_string())?;
    let parsed = std::str::from_utf8(&done.stdout)
        .ok()
        .and_then(|s| json::parse(s).ok());
    let observation = Value::object([
        (
            "status",
            Value::str(if done.code == 0 && parsed.is_some() {
                "ok"
            } else {
                "worker-failure"
            }),
        ),
        ("exit", Value::int(done.code)),
        ("stdout", Value::str(String::from_utf8_lossy(&done.stdout))),
        ("stderr", Value::str(String::from_utf8_lossy(&done.stderr))),
        ("data", parsed.unwrap_or(Value::Null)),
    ]);
    let raw = Value::object([
        (
            "argv",
            Value::strings(argv.iter().map(|a| a.to_string_lossy().into_owned())),
        ),
        ("stdout_sha256", Value::str(sha256::hex(&done.stdout))),
        ("stderr_sha256", Value::str(sha256::hex(&done.stderr))),
        ("observation", observation.clone()),
    ]);
    write_json(&directory.join(format!("{id}.json")), &raw)?;
    if done.code == 0
        && observation
            .get("data")
            .filter(|data| **data != Value::Null)
            .is_some_and(|data| text(data, "operation").ok() != Some(OPERATION))
    {
        return Err("reference worker ran a different operation".into());
    }
    Ok(observation)
}

// Bind completion/refusal facts and work, excluding raw logs and timing data.
fn observation_hash(observation: &Value) -> String {
    let mut data = observation.get("data").cloned().unwrap_or(Value::Null);
    if let Value::Object(fields) = &mut data {
        fields.retain(|(key, _)| key != "elapsed_ns");
    }
    let stable = Value::object([
        (
            "status",
            observation.get("status").cloned().unwrap_or(Value::Null),
        ),
        (
            "exit",
            observation.get("exit").cloned().unwrap_or(Value::Null),
        ),
        ("data", data),
    ]);
    sha256::hex(json::canonical(&stable).as_bytes())
}

fn expected(observation: &Value) -> Value {
    if observation.get("exit") != Some(&Value::int(0)) {
        return Value::Null;
    }
    let Some(data) = observation.get("data") else {
        return Value::Null;
    };
    if data.get("operation").and_then(Value::as_str) != Some(OPERATION)
        || data.get("iterations") != Some(&Value::int(1))
    {
        return Value::Null;
    }
    let mut fields = Vec::new();
    for key in ["packets", "bytes", "digest"] {
        let Some(value @ Value::Number(number)) = data.get(key) else {
            return Value::Null;
        };
        if number.as_i128().is_none_or(|n| n < 0) {
            return Value::Null;
        }
        fields.push((key.to_owned(), value.clone()));
    }
    Value::Object(fields)
}

fn produce(paths: &Paths, recording: Option<Recording<'_>>) -> R<String> {
    let mut authored = all_cases(&paths.root)?;
    let pinned_path = paths.root.join("harness").join(FEATURE).join("corpus.json");
    let previous = if recording.is_none() || pinned_path.is_file() {
        Some(read_json(&pinned_path)?)
    } else {
        None
    };
    let pinned = if recording.is_none() {
        previous.as_ref()
    } else {
        None
    };
    if let Some(pinned) = pinned
        && cases(pinned)?.len() != authored.len()
    {
        return Err("AU case membership changed; re-record reference observations".into());
    }
    let reference_sha = recording
        .map(|r| {
            fs::read(r.worker)
                .map(|v| sha256::hex(&v))
                .map_err(|e| e.to_string())
        })
        .transpose()?;
    paths.fixtures(DIRECTORY)?;
    let previous_rows = previous
        .as_ref()
        .map(cases)
        .transpose()?
        .unwrap_or_default();
    let mut rows = Vec::new();
    let mut private_observations = Vec::new();
    for (index, case) in authored.iter_mut().enumerate() {
        if case.bytes.is_none()
            && let Some(refusal) = previous_rows
                .get(index)
                .filter(|pin| pin.get("id") == case.row.get("id"))
                .and_then(|pin| pin.get("single_input_reference_refusal"))
        {
            put(
                &mut case.row,
                "single_input_reference_refusal",
                refusal.clone(),
            )?;
        }
        let file = text(&case.row, "file")?;
        let input = if let Some(bytes) = &case.bytes {
            let input = paths.out.join(file);
            fs::write(&input, bytes).map_err(|e| e.to_string())?;
            input
        } else {
            paths.root.join(file)
        };
        for (file, bytes) in &case.following {
            fs::write(paths.out.join(file), bytes).map_err(|e| format!("{file}: {e}"))?;
        }
        let (work, result_hash) = if let Some(recording) = recording {
            let observation = reference(&paths.root, &input, &case.row, recording)?;
            let id = text(&case.row, "id")?;
            if entities(&case.row).len() > 1
                && observation.get("data").and_then(|data| data.get("eof"))
                    != Some(&Value::Bool(true))
            {
                return Err(format!(
                    "reference did not reach the final entity's EOF: {id}"
                ));
            }
            let result_hash = observation_hash(&observation);
            let observation_path = recording.directory.join(format!("{id}.json"));
            private_observations.push(Value::object([
                ("id", Value::str(id)),
                (
                    "input_sha256",
                    case.row
                        .get("sha256")
                        .cloned()
                        .ok_or("missing input hash")?,
                ),
                ("reference_result_sha256", Value::str(&result_hash)),
                (
                    "private_observation_sha256",
                    Value::str(sha256::hex(
                        &fs::read(observation_path).map_err(|e| e.to_string())?,
                    )),
                ),
            ]));
            (expected(&observation), result_hash)
        } else {
            let pin = &cases(pinned.ok_or("missing pins")?)?[index];
            let mut structural = pin.clone();
            let Value::Object(fields) = &mut structural else {
                return Err("invalid pinned case".into());
            };
            fields.retain(|(k, _)| !matches!(k.as_str(), "expected" | "reference_result_sha256"));
            if structural != case.row {
                return Err(format!(
                    "AU input, arguments or request association changed: {}",
                    text(&case.row, "id")?
                ));
            }
            let digest = text(pin, "reference_result_sha256")?;
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err("invalid reference result hash".into());
            }
            (
                pin.get("expected")
                    .cloned()
                    .ok_or("missing reference fold")?,
                digest.to_owned(),
            )
        };
        let mut row = case.row.clone();
        put(&mut row, "expected", work)?;
        put(&mut row, "reference_result_sha256", Value::str(result_hash))?;
        rows.push(row);
    }
    if let Some(recording) = recording
        && Some(sha256::hex(
            &fs::read(recording.worker).map_err(|e| e.to_string())?,
        )) != reference_sha
    {
        return Err("reference worker changed while recording".into());
    }
    let recorded = match reference_sha {
        Some(sha) => Value::object([
            ("id", Value::str("ffmpeg-formats-native")),
            ("worker_sha256", Value::str(sha)),
            ("mode", Value::str("ts-au-fold")),
        ]),
        None => pinned
            .and_then(|v| v.get("recorded_reference"))
            .cloned()
            .ok_or("missing recorded reference")?,
    };
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("directory", Value::str(".")),
        ("original_pes_manifest_sha256", Value::str(ORIGINAL_SHA)),
        (
            "source_inputs",
            Value::Array(
                SOURCES
                    .iter()
                    .map(|(path, sha)| {
                        Value::object([("path", Value::str(*path)), ("sha256", Value::str(*sha))])
                    })
                    .collect(),
            ),
        ),
        ("recorded_reference", recorded),
        ("cases", Value::Array(rows)),
    ]);
    if let Some(pinned) = pinned
        && &manifest != pinned
    {
        return Err("AU manifest differs from its recorded reference observations".into());
    }
    paths.manifest(&format!("{FEATURE}/corpus.json"), &manifest)?;
    let contract = contract(&authored)?;
    paths.manifest(&format!("{FEATURE}/contract.json"), &contract)?;
    if let Some(recording) = recording {
        write_json(
            &recording.directory.join("INDEX.json"),
            &Value::object([
                ("complete", Value::Bool(true)),
                (
                    "recorded_reference",
                    manifest
                        .get("recorded_reference")
                        .cloned()
                        .ok_or("missing reference")?,
                ),
                (
                    "corpus_sha256",
                    Value::str(sha256::hex(
                        crate::harness::manifest::canonical(&manifest).as_bytes(),
                    )),
                ),
                (
                    "contract_sha256",
                    Value::str(sha256::hex(
                        crate::harness::manifest::canonical(&contract).as_bytes(),
                    )),
                ),
                ("cases", Value::Array(private_observations)),
            ]),
        )?;
    }
    Ok(format!(
        "Generated {} MPEG-TS AU cases from 97 original inputs with 272 requests and {} coded controls",
        authored.len(),
        authored.len() - 97
    ))
}

pub fn generate(paths: &Paths) -> R<String> {
    produce(paths, None)
}

fn private_directory(root: &Path, requested: &Path) -> R<PathBuf> {
    let parent = requested
        .parent()
        .ok_or("observations directory needs a parent")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let directory = parent.join(
        requested
            .file_name()
            .ok_or("invalid observations directory")?,
    );
    if directory.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
        return Err("raw observations must be outside the repository".into());
    }
    fs::create_dir(&directory)
        .map_err(|e| format!("fresh observations directory required: {e}"))?;
    Ok(directory)
}

/// Author or regenerate fixtures; measurement stays in the shared report runner.
pub fn main(root: &Path, args: &[String]) -> R<()> {
    let mut output = None;
    let mut reference = None;
    let mut observations = None;
    let mut record = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--output" if output.is_none() => {
                output = Some(PathBuf::from(args.next().ok_or("missing output root")?));
            }
            "--reference-worker" if reference.is_none() => {
                reference = Some(PathBuf::from(
                    args.next().ok_or("missing reference worker")?,
                ));
            }
            "--observations" if observations.is_none() => {
                observations = Some(PathBuf::from(
                    args.next()
                        .ok_or("missing private observations directory")?,
                ));
            }
            "--record" if !record => record = true,
            _ => {
                return Err(format!(
                    "unknown or repeated transport fixture option: {flag}"
                ));
            }
        }
    }
    if record != reference.is_some() || record != observations.is_some() {
        return Err(
            "--record, --reference-worker and --observations DIR must be supplied together".into(),
        );
    }
    let paths = Paths {
        root: root.to_path_buf(),
        out: output.ok_or("--output ROOT is required")?,
    };
    let reference = reference
        .map(|p| p.canonicalize().map_err(|e| e.to_string()))
        .transpose()?;
    let directory = observations
        .map(|p| private_directory(root, &p))
        .transpose()?;
    let recording = reference
        .as_deref()
        .zip(directory.as_deref())
        .map(|(worker, directory)| Recording { worker, directory });
    println!("{}", produce(&paths, recording)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
    }

    #[test]
    fn private_reference_archive_does_not_replace_the_result_used_for_pins() {
        let out = crate::walk::scratch("transport-reference-result");
        fs::create_dir_all(&out).unwrap();
        let done = worker::Finished {
            code: 0,
            stdout: format!(
                "{{\"operation\":\"{OPERATION}\",\"iterations\":1,\"packets\":2,\"bytes\":579,\"digest\":7,\"eof\":true}}\n"
            ).into_bytes(),
            stderr: b"[parser @ 0x1234] diagnostic\n".to_vec(),
        };
        let result = save_reference_result("sample", &out, &["worker".into()], &done).unwrap();
        assert_eq!(expected(&result).get("packets"), Some(&Value::int(2)));
        let raw = read_json(&out.join("sample.json")).unwrap();
        assert_eq!(raw.get("observation"), Some(&result));
        assert!(result.get("argv").is_none());
        assert_eq!(fs::read(out.join("sample.stdout")).unwrap(), done.stdout);
        assert_eq!(fs::read(out.join("sample.stderr")).unwrap(), done.stderr);
        fs::remove_dir_all(out).unwrap();
    }

    #[test]
    fn original_inputs_arguments_and_all272_request_associations_are_unchanged() {
        let original = read_json(&root().join(ORIGINAL)).unwrap();
        let generated = all_cases(root()).unwrap();
        assert_eq!(generated.len(), 169);
        for (old, new) in cases(&original).unwrap().iter().zip(&generated) {
            assert!(new.bytes.is_none());
            for key in [
                "id",
                "profile",
                "file",
                "sha256",
                "rights",
                "arguments",
                "requests",
            ] {
                assert_eq!(old.get(key), new.row.get(key), "{key}");
            }
        }
        assert_eq!(
            cases(&original)
                .unwrap()
                .iter()
                .map(|v| v.get("requests").unwrap().as_array().unwrap().len())
                .sum::<usize>(),
            272
        );
    }

    #[test]
    fn single_input_pins_stay_identical_when_sequence_reference_outputs_change() {
        let manifest = read_json(&root().join(format!("harness/{FEATURE}/corpus.json"))).unwrap();
        let rows = cases(&manifest).unwrap();
        let mut prior = rows[..141].to_vec();
        let mut sequences = 0;
        for row in &mut prior {
            if entities(row).len() > 1 {
                sequences += 1;
                assert!(row.get("reference_refusal").is_none());
                let Value::Object(fields) = row else {
                    unreachable!()
                };
                for (key, value) in fields {
                    match key.as_str() {
                        "single_input_reference_refusal" => *key = "reference_refusal".into(),
                        "expected" => {
                            *value = Value::object([
                                ("packets", Value::int(0)),
                                ("bytes", Value::int(0)),
                                ("digest", Value::int(0)),
                            ]);
                        }
                        "reference_result_sha256" => {
                            *value = Value::str(
                                "2345439b3fcd5eeec57a4a399ca8f944d7172120478247cbeb53500ff6eee257",
                            );
                        }
                        _ => (),
                    }
                }
            }
        }
        assert_eq!(sequences, 3);
        let prior = Value::Array(prior);
        assert_eq!(
            sha256::hex(crate::harness::manifest::canonical(&prior).as_bytes()),
            "70a05f8e72322c9b4966bc0751397f7cc06a7088c6819800cd22d35f1901ad6e"
        );
        for (row, (suffix, packets)) in rows[141..143].iter().zip([("eof", 2), ("lookahead", 3)]) {
            assert_eq!(
                text(row, "id").unwrap(),
                format!("au-dvb-ac3-aac-interleaved-{suffix}")
            );
            assert_eq!(
                row.get("expected").unwrap().get("packets"),
                Some(&Value::int(packets))
            );
        }
    }

    #[test]
    fn interleaved_audio_preserves_source_order_clocks_and_complete_frames() {
        fn payload(packet: &[u8]) -> &[u8] {
            let at = 4 + if packet[3] & 0x20 != 0 {
                1 + usize::from(packet[4])
            } else {
                0
            };
            &packet[at..]
        }
        let ac3 = audio(root(), true).unwrap();
        let aac = audio(root(), false).unwrap();
        for second in [false, true] {
            let case = interleaved_audio_case(&ac3, &aac, second).unwrap();
            assert_eq!(
                case.row.get("arguments"),
                Some(&Value::strings(["188", "7", "--dvb"]))
            );
            let raw = case.bytes.unwrap();
            let pmt = payload(raw.chunks_exact(188).nth(1).unwrap());
            assert_eq!(&pmt[4..6], &PROGRAM.to_be_bytes());
            assert_eq!(
                &pmt[13..pmt.len() - 4],
                &[6, 0xe1, 0, 0xf0, 3, 0x6a, 1, 0, 15, 0xe1, 1, 0xf0, 0]
            );
            let mut pes: Vec<(u16, Vec<u8>)> = Vec::new();
            for packet in raw.chunks_exact(188).skip(2) {
                let pid = (u16::from(packet[1] & 31) << 8) | u16::from(packet[2]);
                if packet[1] & 0x40 != 0 {
                    pes.push((pid, Vec::new()));
                }
                let (last_pid, bytes) = pes.last_mut().unwrap();
                assert_eq!(*last_pid, pid);
                bytes.extend_from_slice(payload(packet));
            }
            let mut expected = vec![(&ac3, PID, 90_000), (&aac, PID + 1, 6_000_000)];
            if second {
                expected.push((&ac3, PID, 92_880));
            }
            assert_eq!(pes.len(), expected.len());
            for ((pid, bytes), (codec, expected_pid, pts)) in pes.iter().zip(expected) {
                assert_eq!(*pid, expected_pid);
                assert_eq!(&bytes[..4], &[0, 0, 1, codec.stream_id]);
                assert_eq!(
                    usize::from(u16::from_be_bytes([bytes[4], bytes[5]])) + 6,
                    bytes.len()
                );
                assert_eq!(&bytes[6..9], &[0x80, 0x80, 5]);
                let time = &bytes[9..14];
                let raw_pts = (u64::from(time[0] & 0x0e) << 29)
                    | (u64::from(time[1]) << 22)
                    | (u64::from(time[2] & 0xfe) << 14)
                    | (u64::from(time[3]) << 7)
                    | u64::from(time[4] >> 1);
                assert_eq!(raw_pts, pts);
                assert_eq!(&bytes[14..], &codec.bytes[..codec.first]);
            }
        }
    }

    #[test]
    fn coded_controls_keep_complete_source_bytes_and_distinct_pes_boundaries() {
        for codec in [
            video(root(), false).unwrap(),
            video(root(), true).unwrap(),
            audio(root(), false).unwrap(),
            audio(root(), true).unwrap(),
        ] {
            assert!(codec.first > codec.header);
            let one = &codec.bytes[..codec.first];
            let whole =
                authored_case(&codec, "whole", &[part(one, Some(90_000), None)], false).unwrap();
            let split = authored_case(
                &codec,
                "split",
                &[
                    part(&one[..2], Some(90_000), None),
                    part(&one[2..], None, None),
                ],
                false,
            )
            .unwrap();
            assert_ne!(whole.bytes, split.bytes);
            let m2ts = authored_case(&codec, "m2ts", &[part(one, Some(90_000), None)], true)
                .unwrap()
                .bytes
                .unwrap();
            assert_eq!(
                m2ts.chunks_exact(192)
                    .flat_map(|p| p[4..].iter().copied())
                    .collect::<Vec<_>>(),
                whole.bytes.unwrap()
            );
        }
    }

    #[test]
    fn malformed_reference_results_do_not_gain_fabricated_work() {
        for observation in [
            Value::Null,
            Value::object([("status", Value::str("timeout"))]),
            Value::object([("exit", Value::int(2))]),
        ] {
            assert_eq!(expected(&observation), Value::Null);
        }
        let observation = Value::object([
            ("exit", Value::int(0)),
            (
                "data",
                Value::object([
                    ("operation", Value::str(OPERATION)),
                    ("iterations", Value::int(1)),
                    ("packets", Value::int(0)),
                    ("bytes", Value::int(0)),
                    ("digest", Value::int(0)),
                    ("eof", Value::Bool(false)),
                ]),
            ),
        ]);
        assert_eq!(expected(&observation).get("packets"), Some(&Value::int(0)));
        assert_eq!(
            observation.get("data").unwrap().get("eof"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn wrap_control_encodes_all33_source_clock_bits() {
        let authored = all_cases(root()).unwrap();
        let case = authored
            .iter()
            .find(|c| text(&c.row, "id").unwrap() == "au-avc-clock-wrap")
            .unwrap();
        let timestamps: Vec<_> = case
            .bytes
            .as_ref()
            .unwrap()
            .chunks_exact(188)
            .filter(|p| p[1] & 0x40 != 0 && p[1] & 31 == 1 && p[2] == 0)
            .map(|p| {
                let payload = 4 + if p[3] & 0x20 != 0 {
                    1 + usize::from(p[4])
                } else {
                    0
                };
                p[payload + 9..payload + 14].to_vec()
            })
            .collect();
        assert_eq!(
            timestamps,
            [
                vec![0x2f, 0xff, 0xff, 0xf8, 0x31],
                vec![0x21, 0, 1, 0x0f, 0xa1]
            ]
        );
    }

    #[test]
    fn prefix_loading_uses_the_shared_report_and_keeps_all_required_profiles() {
        let feature = root().join("harness").join(FEATURE);
        let au = crate::harness::report::PrefixFeature::load(
            root(),
            &feature.join("contract.json"),
            &feature.join("corpus.json"),
            None,
        )
        .unwrap();
        assert_eq!(au.prefix, "ts-au-");
        assert_eq!(au.candidate, "transport-worker");
        assert_eq!(au.reference, "ffmpeg-formats-native");
        assert_eq!(au.cases.len(), 169);
        let contract = read_json(&au.contract).unwrap();
        assert_eq!(
            contract.get("required_profiles"),
            Some(&Value::strings(
                au.cases.iter().map(|c| text(c, "profile").unwrap())
            ))
        );
        assert!(contract.get("open_cases").is_none());
        let old = crate::harness::report::PrefixFeature::load(
            root(),
            &root().join("harness/features/mpegts-pes/contract.json"),
            &root().join(ORIGINAL),
            None,
        )
        .unwrap();
        assert_eq!(old.prefix, "ts-");
        assert_eq!(old.candidate, "transport-worker");
        assert_eq!(old.cases.len(), 97);
    }

    #[test]
    fn reference_pins_prove_multiple_aus_per_pes_and_keep_raw_logs_private() {
        let manifest = read_json(&root().join(format!("harness/{FEATURE}/corpus.json"))).unwrap();
        let rows = cases(&manifest).unwrap();
        for codec in ["avc", "hevc", "adts-aac-lc", "dvb-ac3"] {
            for (suffix, count) in [
                ("one", 1),
                ("multiple-per-pes", 2),
                ("header-across-pes", 1),
                ("body-across-pes", 1),
                ("final-au-at-eof", 2),
                ("m2ts192", 2),
            ] {
                let id = format!("au-{codec}-{suffix}");
                let row = rows.iter().find(|r| text(r, "id").unwrap() == id).unwrap();
                assert_eq!(
                    row.get("expected").unwrap().get("packets"),
                    Some(&Value::int(count)),
                    "{id}"
                );
            }
        }
        for id in [
            "au-atsc-ac3-multiple-per-pes",
            "au-atsc-eac3-multiple-per-pes",
        ] {
            let row = rows.iter().find(|r| text(r, "id").unwrap() == id).unwrap();
            assert_eq!(
                row.get("arguments"),
                Some(&Value::strings(["188", "7", "--atsc"]))
            );
            assert_eq!(
                row.get("expected").unwrap().get("packets"),
                Some(&Value::int(2))
            );
        }
        for (suffix, count) in [
            ("two-opaque-pes", 1),
            ("opaque-then-coded", 2),
            ("coded-then-opaque", 2),
            ("opaque-prefix-in-pes", 2),
            ("opaque-suffix-in-pes", 2),
            ("recover-pat-pointer", 1),
            ("recover-pmt-pointer", 1),
        ] {
            let id = format!("au-adts-aac-lc-{suffix}");
            let row = rows.iter().find(|r| text(r, "id").unwrap() == id).unwrap();
            assert_eq!(
                row.get("expected").unwrap().get("packets"),
                Some(&Value::int(count))
            );
        }
        for row in rows {
            assert!(row.get("reference_observation").is_none());
            assert!(row.get("stdout").is_none() && row.get("stderr").is_none());
            let digest = text(row, "reference_result_sha256").unwrap();
            assert_eq!(digest.len(), 64);
            assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        let opaque = rows
            .iter()
            .find(|r| text(r, "id").unwrap() == "ts188-basic")
            .unwrap();
        assert!(opaque.get("construction").is_none());
    }

    #[test]
    fn stable_hash_excludes_log_addresses_and_keeps_completion_facts() {
        let mut observation = Value::object([
            ("status", Value::str("ok")),
            ("exit", Value::int(0)),
            ("stderr", Value::str("[h264 @ 0x1234] missing picture")),
            (
                "data",
                Value::object([
                    ("operation", Value::str(OPERATION)),
                    ("iterations", Value::int(1)),
                    ("packets", Value::int(0)),
                    ("bytes", Value::int(0)),
                    ("digest", Value::int(0)),
                    ("eof", Value::Bool(true)),
                ]),
            ),
        ]);
        let first = observation_hash(&observation);
        put(
            &mut observation,
            "stderr",
            Value::str("[h264 @ 0x9876] missing picture"),
        )
        .unwrap();
        assert_eq!(observation_hash(&observation), first);
        let mut data = observation.get("data").unwrap().clone();
        put(&mut data, "eof", Value::Bool(false)).unwrap();
        put(&mut observation, "data", data).unwrap();
        assert_ne!(observation_hash(&observation), first);
    }

    #[test]
    fn authored_dvb_ac3_descriptor_rejects_missing_flags_byte() {
        let codec = audio(root(), true).unwrap();
        let mut ts = Transport::new();
        assert!(ts.tables(&codec, &[0x6a, 0]).is_err());
        assert!(ts.bytes.is_empty());
        ts.tables(&codec, &[0x6a, 1, 0]).unwrap();
        assert_eq!(ts.bytes.len(), 2 * 188);
    }

    #[test]
    fn raw_recording_requires_a_fresh_directory_outside_the_repository() {
        let out = crate::walk::scratch("transport-private-observations");
        let repo = out.join("repo");
        fs::create_dir_all(&repo).unwrap();
        assert!(private_directory(&repo, &repo.join("raw")).is_err());
        assert!(!repo.join("raw").exists());
        let private = out.join("raw");
        private_directory(&repo, &private).unwrap();
        assert!(private_directory(&repo, &private).is_err());
        fs::remove_dir_all(out).unwrap();
    }

    #[test]
    fn pes_input_pin_separates_reference_folds_from_source_associations() {
        let manifest = read_json(&root().join(ORIGINAL)).unwrap();
        let mutate = |key, value| {
            let mut modified = manifest.clone();
            let Value::Object(fields) = &mut modified else {
                unreachable!()
            };
            let Some((_, Value::Array(rows))) = fields.iter_mut().find(|(key, _)| key == "cases")
            else {
                unreachable!()
            };
            put(&mut rows[0], key, value).unwrap();
            original_inputs(modified)
        };
        assert!(mutate("expected", Value::Null).is_ok());
        assert!(mutate("reference_refusal", Value::str("different PES result")).is_ok());
        assert!(mutate("sha256", Value::str("different source pin")).is_err());
        assert!(
            mutate(
                "arguments",
                Value::Array(vec![Value::str("192"), Value::str("1")])
            )
            .is_err()
        );
        let mut requests = cases(&manifest).unwrap()[0]
            .get("requests")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        put(&mut requests[0], "ordinal", Value::int(272)).unwrap();
        assert!(mutate("requests", Value::Array(requests)).is_err());
    }

    #[test]
    fn regenerating_the_au_corpus_preserves_every_pin_and_file_hash() {
        let out = crate::walk::scratch("transport-fixtures");
        let paths = Paths {
            root: root().to_path_buf(),
            out: out.clone(),
        };
        generate(&paths).unwrap();
        for name in ["contract.json", "corpus.json"] {
            let file = format!("harness/{FEATURE}/{name}");
            assert_eq!(
                fs::read(root().join(&file)).unwrap(),
                fs::read(out.join(&file)).unwrap()
            );
        }
        let manifest = read_json(&out.join(format!("harness/{FEATURE}/corpus.json"))).unwrap();
        for case in &cases(&manifest).unwrap()[97..] {
            let file = Path::new(text(case, "file").unwrap());
            let base = if file.starts_with(".research") {
                out.as_path()
            } else {
                root()
            };
            assert_eq!(
                sha256::hex(&fs::read(base.join(file)).unwrap()),
                text(case, "sha256").unwrap()
            );
        }
        fs::remove_dir_all(out).unwrap();
    }
}
