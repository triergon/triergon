//! Independently authored TS controls for selected-track seek and audio framing.
mod mpeg_video;
mod xll;
pub use mpeg_video::author as author_mpeg_video;

use super::{Codec, PID, PROGRAM, Part, R, Transport, part, source};
use crate::corpus::write_json;
use crate::json::{self, Value};
use crate::sha256;
use std::fs;
use std::path::Path;
use std::process::Command;

const AUDIO: &str = "harness/fixtures/mpegts/free-format-mono.mp3";
const AUDIO_SHA256: &str = "2d049635dceb38a5dc7c442f272c064b8fdbcebe1c9b218c7b6ea1c72fcfbe6d";

struct Input {
    name: &'static str,
    m2ts: bool,
    data: Vec<u8>,
}

impl Input {
    fn record(
        &self,
        root: &Path,
        feature: &str,
        case: &str,
        options: &[&str],
        reference: &Path,
        mode: &str,
    ) -> R<Value> {
        let extension = if self.m2ts { "m2ts" } else { "ts" };
        let file = format!(
            "harness/features/{feature}/inputs/{}.{extension}",
            self.name
        );
        let path = root.join(&file);
        fs::create_dir_all(path.parent().ok_or("fixture path")?).map_err(|e| e.to_string())?;
        fs::write(&path, &self.data).map_err(|e| e.to_string())?;
        let mut args = vec![
            if self.m2ts { "192" } else { "188" }.into(),
            PROGRAM.to_string(),
        ];
        args.extend(options.iter().copied().map(str::to_owned));
        let output = Command::new(reference)
            .arg(mode)
            .arg(&path)
            .args(&args)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "{case}: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
        let record = json::parse(stdout).map_err(|e| e.to_string())?;
        let expected = ["packets", "bytes", "digest"]
            .into_iter()
            .map(|key| Ok((key.to_string(), record.get(key).ok_or(key)?.clone())))
            .collect::<Result<_, String>>()?;
        let mut entry = vec![
            ("id", Value::str(case)),
            ("profile", Value::str(case)),
            ("file", Value::str(file)),
            ("sha256", Value::str(sha256::hex(&self.data))),
            ("rights", Value::str("authored-synthetic")),
            ("arguments", Value::strings(args)),
            ("expected", Value::Object(expected)),
        ];
        if record.get("eof") != Some(&Value::Bool(true)) {
            let refusal = record
                .get("refusal")
                .ok_or("missing reference refusal")?
                .clone();
            eprintln!("{case}: reference stopped before EOF ({refusal:?})");
            entry.push(("reference_refusal", refusal));
        }
        Ok(Value::object(entry))
    }
}

fn pictures(root: &Path, hevc: bool) -> R<(Codec, Vec<Vec<u8>>)> {
    let codec = super::video(root, hevc)?;
    let stem = format!("crates/framing/fixtures/raw-annex-b/{}-aud0", codec.name);
    let raw = source(
        root,
        &format!("{stem}.{}", if hevc { "hevc" } else { "h264" }),
    )?;
    let index =
        String::from_utf8(source(root, &format!("{stem}.packets"))?).map_err(|e| e.to_string())?;
    let pictures = index
        .lines()
        .map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let offset = fields
                .next()
                .ok_or("picture offset")?
                .parse::<usize>()
                .map_err(|e| e.to_string())?;
            let length = fields
                .next()
                .ok_or("picture extent")?
                .parse::<usize>()
                .map_err(|e| e.to_string())?;
            let bytes = raw
                .get(offset..offset.checked_add(length).ok_or("picture extent")?)
                .ok_or("picture outside input")?;
            Ok(bytes.to_vec())
        })
        .collect::<R<Vec<_>>>()?;
    Ok((codec, pictures))
}

fn video(root: &Path, hevc: bool, repeat: usize) -> R<Input> {
    let (codec, pictures) = pictures(root, hevc)?;
    let mut ts = Transport::new();
    ts.tables(&codec, &[])?;
    let mut i = 0_u64;
    for _ in 0..repeat {
        for bytes in &pictures {
            ts.pes(
                0xe0,
                &part(bytes, Some(90_000 + i * 3000), Some(84_000 + i * 3000)),
            )?;
            i += 1;
        }
    }
    Ok(Input {
        name: if hevc {
            "hevc"
        } else if repeat > 1 {
            "avc-many-pictures"
        } else {
            "avc"
        },
        m2ts: hevc,
        data: ts.finish(hevc),
    })
}

fn physical_video_seek_cases(root: &Path, reference: &Path, cases: &mut Vec<Value>) -> R<()> {
    let pid = PID.to_string();
    for (hevc, name, first, second) in [
        (false, "avc-physical-repeated-preroll", "105121", "92161"),
        (true, "hevc-physical-repeated-preroll", "92161", "105121"),
    ] {
        let (codec, pictures) = pictures(root, hevc)?;
        let picture = pictures.first().ok_or("missing first coded picture")?;
        // Keep the first coded picture's complete parameter chain and all IDR
        // slices. Omit SEI explicitly; its state is outside the direct profile.
        let starts: Vec<_> = picture
            .windows(3)
            .enumerate()
            .filter_map(|(at, bytes)| (bytes == [0, 0, 1]).then_some(at + 3))
            .collect();
        let mut body = Vec::new();
        let mut nals = 0;
        for (index, &start) in starts.iter().enumerate() {
            let end = starts.get(index + 1).map_or(picture.len(), |next| next - 3);
            let mut nal = &picture[start..end];
            while nal.last() == Some(&0) {
                nal = &nal[..nal.len() - 1];
            }
            let Some(&header) = nal.first() else { continue };
            let kind = if hevc {
                (header >> 1) & 63
            } else {
                header & 31
            };
            if matches!((hevc, kind), (false, 5 | 7 | 8) | (true, 20 | 32..=34)) {
                body.extend_from_slice(&[0, 0, 0, 1]);
                body.extend_from_slice(nal);
                nals += 1;
            }
        }
        if nals != 4 {
            return Err("physical video parameter chain or coded picture changed".into());
        }
        let mut ts = Transport::new();
        for _ in 0..17 {
            ts.payload(0x1fff, &[0xff; 184]);
        }
        for index in 0..10 {
            ts.tables(&codec, &[])?;
            let pts = 90_000 + index * 2160;
            ts.pes(
                0xe0,
                &Part {
                    aligned: true,
                    ..part(
                        &body,
                        Some(pts),
                        (index % 2 != 0).then_some(85_000 + index * 2160),
                    )
                },
            )?;
        }
        cases.push(
            Input {
                name,
                m2ts: false,
                data: ts.finish(false),
            }
            .record(
                root,
                "mpegts-previous-picture",
                name,
                &[&pid, first, "preroll", second],
                reference,
                "ts-seek-fold",
            )?,
        );
    }
    Ok(())
}

fn epoch_video(root: &Path, configuration: bool, m2ts: bool) -> R<(Input, usize)> {
    let (codec, pictures) = pictures(root, false)?;
    if pictures.len() != 12 {
        return Err("epoch control requires all twelve authored source pictures".into());
    }
    let mut ts = Transport::new();
    ts.tables(&codec, &[])?;
    ts.pcr(89_000, false);
    let mut boundary = 0;
    for (epoch, base) in [99_000, 90_000].into_iter().enumerate() {
        if epoch == 1 {
            if configuration {
                ts.tables_for_version(&[(&codec, PID, &[0x0a, 4, b'e', b'n', b'g', 0])], 1)?;
            } else {
                ts.pcr(80_000, true);
                ts.pcr(81_000, false);
            }
            boundary = ts.bytes.len() / 188 * if m2ts { 192 } else { 188 };
        }
        for (i, bytes) in pictures.iter().enumerate() {
            // Reordered PTS and increasing encoded DTS in each epoch. An
            // unfiltered scan would choose the earlier epoch's 111,000-tick
            // keyframe over the selected epoch's 102,000-tick keyframe.
            let presentation = match i % 4 {
                1 => i + 2,
                2 | 3 => i - 1,
                _ => i,
            } as u64;
            ts.pes(
                0xe0,
                &part(
                    bytes,
                    Some(base + presentation * 3000),
                    Some(base - 9000 + i as u64 * 3000),
                ),
            )?;
        }
    }
    Ok((
        Input {
            name: if configuration {
                "avc-configuration-epoch"
            } else {
                "avc-clock-epoch"
            },
            m2ts,
            data: ts.finish(m2ts),
        },
        boundary,
    ))
}

fn epoch_cases(root: &Path, reference: &Path, cases: &mut Vec<Value>) -> R<()> {
    for configuration in [false, true] {
        for m2ts in [false, true] {
            let (input, boundary) = epoch_video(root, configuration, m2ts)?;
            let start = format!("epoch-start={boundary}");
            let pair = if configuration {
                "epoch=0:3"
            } else {
                "epoch=1:2"
            };
            let unfiltered = input.record(
                root,
                "mpegts-previous-picture",
                "unfiltered-epoch-control",
                &["256", "112000", "previous"],
                reference,
                "ts-seek-fold",
            )?;
            if unfiltered.get("reference_refusal").is_some()
                || unfiltered.get("expected").and_then(|v| v.get("packets"))
                    != Some(&Value::int(20))
            {
                return Err("unfiltered epoch control must land in the earlier epoch".into());
            }
            for policy in ["previous", "preroll"] {
                let id = format!(
                    "{}-{}-{policy}",
                    input.name,
                    if m2ts { "m2ts" } else { "ts188" }
                );
                let row = input.record(
                    root,
                    "mpegts-previous-picture",
                    &id,
                    &["256", "112000", policy, pair, &start],
                    reference,
                    "ts-seek-fold",
                )?;
                if row.get("reference_refusal").is_some()
                    || row.get("expected").and_then(|v| v.get("packets")) != Some(&Value::int(8))
                    || row.get("expected") == unfiltered.get("expected")
                {
                    return Err(format!(
                        "{id}: epoch control must deliver all eight suffix pictures"
                    ));
                }
                cases.push(row);
            }
        }
    }
    Ok(())
}

fn audio(name: &'static str, pieces: &[&[u8]], m2ts: bool, version: u8) -> R<Input> {
    audio_with_clock(name, pieces, m2ts, version, false)
}

fn audio_with_clock(
    name: &'static str,
    pieces: &[&[u8]],
    m2ts: bool,
    version: u8,
    decode: bool,
) -> R<Input> {
    let codec = Codec {
        name,
        stream_type: if version == 1 { 0x03 } else { 0x04 },
        stream_id: 0xc0,
        dvb: false,
        bytes: Vec::new(),
        first: 0,
        header: 0,
    };
    let mut ts = Transport::new();
    ts.tables(&codec, &[])?;
    for (i, bytes) in pieces.iter().enumerate() {
        let pts = 90_000 + i as u64 * 9000;
        ts.pes(0xc0, &part(bytes, Some(pts), decode.then_some(pts - 1200)))?;
    }
    Ok(Input {
        name,
        m2ts,
        data: ts.finish(m2ts),
    })
}

fn silence(header: [u8; 4], length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    bytes[..4].copy_from_slice(&header);
    bytes
}

fn manifest(root: &Path, feature: &str, cases: Vec<Value>) -> R<()> {
    write_json(
        &root.join(format!("harness/features/{feature}/corpus.json")),
        &Value::object([
            ("schema", Value::int(1)),
            ("directory", Value::str(".")),
            ("cases", Value::Array(cases)),
        ]),
    )
}

/// Markers, clocks and packet envelopes are generated from checked coded inputs.
/// Pins are recorded by the public API references, not the candidate.
pub fn author(root: &Path, args: &[String]) -> R<()> {
    let [record, seek, audio_argv] = args else {
        return Err(
            "usage: cargo xtask fixture transport-matched --record SEEK_REFERENCE AUDIO_REFERENCE"
                .into(),
        );
    };
    if record != "--record" {
        return Err("expected --record".into());
    }
    let seek = Path::new(seek);
    let mpeg = Path::new(audio_argv);
    let pid = PID.to_string();
    let feature = "mpegts-previous-picture";
    let avc = video(root, false, 1)?;
    let hevc = video(root, true, 1)?;
    let many = video(root, false, 12)?;
    let mut cases = [
        (
            &avc,
            "avc-earliest-picture",
            [pid.as_str(), "90000", "previous"],
        ),
        (
            &avc,
            "avc-between-keyframes",
            [pid.as_str(), "112000", "previous"],
        ),
        (
            &hevc,
            "hevc-m2ts-preroll",
            [pid.as_str(), "121000", "preroll"],
        ),
        (
            &many,
            "avc-late-preroll",
            [pid.as_str(), "425500", "preroll"],
        ),
    ]
    .into_iter()
    .map(|(input, case, options)| input.record(root, feature, case, &options, seek, "ts-seek-fold"))
    .collect::<R<Vec<_>>>()?;
    for (input, name, first, second) in [
        (&many, "avc-repeated-preroll", "115000", "425500"),
        (&hevc, "hevc-m2ts-repeated-preroll", "100000", "121000"),
    ] {
        cases.push(input.record(
            root,
            feature,
            name,
            &[pid.as_str(), first, "preroll", second],
            seek,
            "ts-seek-fold",
        )?);
    }
    for (input, name, target, split) in [
        (&avc, "avc-split-pes", "90000", "split=564"),
        (&hevc, "hevc-m2ts-split-pes", "121000", "split=576"),
    ] {
        cases.push(input.record(
            root,
            feature,
            name,
            &[pid.as_str(), target, "preroll", split],
            seek,
            "ts-seek-fold",
        )?);
    }
    cases.push(avc.record(
        root,
        feature,
        "avc-split-repeated-preroll",
        &[pid.as_str(), "90000", "preroll", "112000", "split=564"],
        seek,
        "ts-seek-fold",
    )?);
    epoch_cases(root, seek, &mut cases)?;
    physical_video_seek_cases(root, seek, &mut cases)?;
    manifest(root, feature, cases)?;

    let raw = fs::read(root.join(AUDIO)).map_err(|e| format!("{AUDIO}: {e}"))?;
    if sha256::hex(&raw) != AUDIO_SHA256 {
        return Err(format!("{AUDIO}: pinned coded bytes changed"));
    }
    let l1 = silence([0xff, 0xff, 0x04, 0xc0], 96);
    let l1_pad = silence([0xff, 0xff, 0x06, 0xc0], 100);
    let l2 = silence([0xff, 0xf5, 0x04, 0xc0], 192);
    let v1_l2 = silence([0xff, 0xfd, 0x04, 0xc0], 192).repeat(4);
    let v2_l1 = silence([0xff, 0xf7, 0x04, 0xc0], 96).repeat(4);
    let v2_l3 = silence([0xff, 0xf3, 0x04, 0xc0], 96).repeat(4);
    let layer1 = [&l1[..], &l1_pad, &l1, &l1_pad].concat();
    let mut layer1_ancillary = layer1.clone();
    layer1_ancillary[193..196].copy_from_slice(&[0x17, 0xab, 0x35]);
    let layer2 = l2.repeat(4);
    let feature = "mpegts-free-audio";
    let inputs = [
        (
            audio(
                "mpeg1-layer3",
                &raw.chunks(4500).collect::<Vec<_>>(),
                false,
                1,
            )?,
            false,
        ),
        (
            audio(
                "mpeg1-layer3-split-header",
                &[&raw[..2], &raw[2..]],
                true,
                1,
            )?,
            false,
        ),
        (
            audio(
                "mpeg1-layer1-padding",
                &[&layer1[..170], &layer1[170..]],
                false,
                1,
            )?,
            true,
        ),
        (
            audio(
                "mpeg1-layer1-padding-across-pes",
                &[&layer1_ancillary[..194], &layer1_ancillary[194..]],
                false,
                1,
            )?,
            true,
        ),
        (audio("mpeg2-layer2", &[&layer2], false, 2)?, false),
        (
            audio_with_clock(
                "mpeg1-layer2-pts-dts",
                &[&v1_l2[..1], &v1_l2[1..]],
                false,
                1,
                true,
            )?,
            false,
        ),
        (audio("mpeg2-layer1", &[&v2_l1], false, 2)?, false),
        (audio("mpeg2-layer3", &[&v2_l3], false, 2)?, false),
    ];
    let cases = inputs
        .into_iter()
        .map(|(input, layer_one_slots)| {
            let options: &[&str] = if layer_one_slots {
                &[pid.as_str(), "layer1-padding"]
            } else {
                &[pid.as_str()]
            };
            input.record(
                root,
                feature,
                input.name,
                options,
                mpeg,
                "ts-free-audio-fold",
            )
        })
        .collect::<R<Vec<_>>>()?;
    manifest(root, feature, cases)
}

/// Rewrap the checked DTS core frames with and without a DVB descriptor.
pub fn author_dts(root: &Path, args: &[String]) -> R<()> {
    let [record, reference] = args else {
        return Err("usage: cargo xtask fixture transport-dts --record REFERENCE".into());
    };
    if record != "--record" {
        return Err("expected --record".into());
    }
    let reference = Path::new(reference);
    const FEATURE: &str = "mpegts-dts-core";
    const SOURCE: &str = "harness/fixtures/mpegts/dts-core-mono.dts";
    const SHA256: &str = "a1db20abc4bce08a504108de2998337f4d906eda3373acdc3abab39fcaaf1330";
    let bytes = fs::read(root.join(SOURCE)).map_err(|e| format!("{SOURCE}: {e}"))?;
    if sha256::hex(&bytes) != SHA256 {
        return Err(format!("{SOURCE}: pinned coded bytes changed"));
    }
    let codec = Codec {
        name: "dvb-dts",
        stream_type: 0x06,
        stream_id: 0xbd,
        dvb: true,
        bytes: Vec::new(),
        first: 0,
        header: 15,
    };
    let inputs = [
        (
            "dts-core-single-frame",
            false,
            vec![part(&bytes[..512], Some(90_000), None)],
        ),
        (
            "dts-core-multiple-per-pes",
            false,
            vec![part(&bytes, Some(90_000), Some(89_760))],
        ),
        (
            "dts-core-header-across-pes",
            true,
            vec![
                part(&bytes[..7], Some(90_000), None),
                part(&bytes[7..], None, None),
            ],
        ),
        (
            "dts-core-body-across-pes",
            false,
            vec![
                part(&bytes[..1221], Some(90_000), None),
                part(&bytes[1221..], Some(92_880), Some(92_640)),
            ],
        ),
    ];
    let mut cases = Vec::new();
    for (name, m2ts, parts) in inputs {
        let mut ts = Transport::new();
        // EN 300 468 Table G.1. The flags describe the checked mono source.
        ts.tables(&codec, &[0x7b, 5, 0xd2, 0x87, 0x83, 0xfe, 0x00])?;
        for p in &parts {
            ts.pes(codec.stream_id, p)?;
        }
        let input = Input {
            name,
            m2ts,
            data: ts.finish(m2ts),
        };
        cases.push(input.record(root, FEATURE, name, &["--dvb"], reference, "ts-au-fold")?);
    }
    let selection = format!("--dts-core={PID}");
    for (name, reserved_bits_valid) in [
        ("dts-core-unlabelled-private-data", true),
        ("dts-core-pcr-reserved-bits", false),
    ] {
        let mut ts = Transport::new();
        ts.tables(&codec, &[])?;
        let mut options = vec![selection.as_str()];
        if !reserved_bits_valid {
            // A PCR-only carrier with a base of 80,000, extension zero and
            // the six reserved bits cleared. Its counter precedes the first PES.
            let mut carrier = [0xff; 188];
            carrier[..12].copy_from_slice(&[
                0x47, 0x01, 0x00, 0x2f, 183, 0x10, 0x00, 0x00, 0x9c, 0x40, 0x00, 0x00,
            ]);
            ts.bytes.extend_from_slice(&carrier);
            options.push("--ignore-pcr-reserved-bits");
        }
        for index in 0..4 {
            ts.pes(
                codec.stream_id,
                &part(&bytes, Some(90_000 + 11_520 * index), None),
            )?;
        }
        let input = Input {
            name,
            m2ts: false,
            data: ts.finish(false),
        };
        cases.push(input.record(root, FEATURE, name, &options, reference, "ts-au-fold")?);
    }
    manifest(root, FEATURE, cases)
}

fn loas_aac(payload: &[u8], configured: bool) -> R<Vec<u8>> {
    let mut frame = vec![0x56, 0xe0, 0];
    let mut position = 24;
    let mut put = |value: u32, width: usize| {
        for shift in (0..width).rev() {
            if position % 8 == 0 {
                frame.push(0);
            }
            let index = position / 8;
            frame[index] |= ((value >> shift) as u8 & 1) << (7 - position % 8);
            position += 1;
        }
    };
    put(u32::from(!configured), 1);
    if configured {
        // audioMuxVersion 0, shared time framing, one frame, program and layer.
        put(1 << 13, 15);
        put(0x1188, 16); // AAC-LC mono, 48 kHz, 1,024 samples.
        put(0, 3); // Variable payload length, one AAC layer.
        put(255, 8); // Variable-rate buffer fullness.
        put(0, 2); // No other data or header CRC.
    }
    for _ in 0..payload.len() / 255 {
        put(255, 8);
    }
    put((payload.len() % 255) as u32, 8);
    for &byte in payload {
        put(u32::from(byte), 8);
    }
    let size = frame.len() - 3;
    if size > 8191 {
        return Err("fixture LOAS payload length".into());
    }
    frame[1] |= (size >> 8) as u8;
    frame[2] = size as u8;
    Ok(frame)
}

fn latm_seek_cases(root: &Path, feature: &str, reference: &Path, cases: &mut Vec<Value>) -> R<()> {
    let aac = super::audio(root, false)?;
    let raw = &aac.bytes[aac.header..aac.first];
    let configured = loas_aac(raw, true)?;
    let dependent = loas_aac(raw, false)?;
    let latm = Codec {
        name: "latm-aac-lc",
        stream_type: 0x11,
        ..aac
    };
    let mut ts = Transport::new();
    ts.tables(&latm, &[])?;
    for (frame, pts) in [
        (&configured, 90_000),
        (&dependent, 91_920),
        (&dependent, 93_840),
        (&configured, 95_760),
    ] {
        ts.pes(latm.stream_id, &part(frame, Some(pts), None))?;
    }
    let input = Input {
        name: latm.name,
        m2ts: false,
        data: ts.finish(false),
    };
    let pid = PID.to_string();
    for (id, target, policy) in [
        ("latm-in-band-preroll", "94000", "preroll"),
        ("latm-next-configuration", "96000", "previous"),
    ] {
        cases.push(input.record(
            root,
            feature,
            id,
            &[pid.as_str(), target, policy],
            reference,
            "ts-audio-seek-fold",
        )?);
    }
    Ok(())
}

fn physical_audio_seek_cases(
    root: &Path,
    feature: &str,
    reference: &Path,
    cases: &mut Vec<Value>,
) -> R<()> {
    let pid = PID.to_string();
    for (name, header, length, step, first, repeated) in [
        (
            "mpeg1-layer1-physical-repeated-preroll",
            [0xff, 0xff, 0x14, 0xc0],
            32,
            720,
            7,
            1,
        ),
        (
            "mpeg1-layer2-physical-repeated-preroll",
            [0xff, 0xfd, 0x14, 0xc0],
            96,
            2160,
            1,
            7,
        ),
    ] {
        let codec = Codec {
            name,
            stream_type: 0x03,
            stream_id: 0xc0,
            dvb: false,
            bytes: silence(header, length),
            first: length,
            header: 4,
        };
        let mut ts = Transport::new();
        // A physical prefix has no elementary coordinates. Each later landing
        // starts at its local PAT/PMT, independently of this discarded prefix.
        for _ in 0..17 {
            ts.payload(0x1fff, &[0xff; 184]);
        }
        for index in 0..10 {
            // Identical CRC-checked sections bootstrap the program at every
            // checkpoint. The complete timestamped PES fits in one carrier.
            ts.tables(&codec, &[])?;
            let pts = 90_000 + index * step;
            ts.pes(
                codec.stream_id,
                &part(&codec.bytes, Some(pts), (index % 2 != 0).then_some(pts)),
            )?;
        }
        let input = Input {
            name,
            m2ts: false,
            data: ts.finish(false),
        };
        let first = (90_001 + first * step).to_string();
        let repeated = (90_001 + repeated * step).to_string();
        cases.push(input.record(
            root,
            feature,
            name,
            &[&pid, &first, "preroll", &repeated],
            reference,
            "ts-audio-seek-fold",
        )?);
    }
    Ok(())
}

/// Rewrap pinned audio frames with independently authored PES timing.
pub fn author_audio_seek(root: &Path, args: &[String]) -> R<()> {
    let [record, reference] = args else {
        return Err("usage: cargo xtask fixture transport-audio-seek --record REFERENCE".into());
    };
    if record != "--record" {
        return Err("expected --record".into());
    }

    const FEATURE: &str = "mpegts-previous-audio";
    let pid = PID.to_string();
    let mut cases = Vec::new();
    let mut profiles = vec![
        (
            super::audio(root, false)?,
            false,
            &[][..],
            &[][..],
            1920,
            true,
        ),
        (
            super::audio(root, true)?,
            true,
            &[0x6a, 1, 0][..],
            &["--dvb"][..],
            2880,
            true,
        ),
        (
            super::direct_eac3(root)?,
            true,
            &[][..],
            &["--atsc"][..],
            2880,
            false,
        ),
    ];
    for (name, stream_type, header, frame_len, step) in [
        ("mpeg1-layer1", 0x03, [0xff, 0xff, 0x14, 0xc0], 32, 720),
        ("mpeg1-layer2", 0x03, [0xff, 0xfd, 0x44, 0xc0], 192, 2160),
        ("mpeg2-layer2", 0x04, [0xff, 0xf5, 0x44, 0xc0], 192, 4320),
    ] {
        profiles.push((
            Codec {
                name,
                stream_type,
                stream_id: 0xc0,
                dvb: false,
                bytes: silence(header, frame_len).repeat(2),
                first: frame_len,
                header: 4,
            },
            false,
            &[],
            &[],
            step,
            false,
        ));
    }
    for (codec, m2ts, descriptors, namespace, step, repeated) in profiles {
        let frame = &codec.bytes[..codec.first];
        let name = codec.name;
        let mut ts = Transport::new();
        ts.tables(&codec, descriptors)?;
        ts.pes(codec.stream_id, &part(&codec.bytes, Some(90_000), None))?;
        ts.pes(codec.stream_id, &part(frame, None, None))?;
        ts.pes(
            codec.stream_id,
            &part(
                &codec.bytes,
                Some(90_000 + 3 * step),
                Some(90_000 + 3 * step - 90),
            ),
        )?;
        ts.pes(codec.stream_id, &part(frame, Some(90_000 + 5 * step), None))?;
        let input = Input {
            name,
            m2ts,
            data: ts.finish(m2ts),
        };
        for (policy, target) in [("previous", "90000"), ("preroll", "100000")] {
            let id = format!("{name}-{policy}");
            let mut options = namespace.to_vec();
            options.extend([pid.as_str(), target, policy]);
            cases.push(input.record(
                root,
                FEATURE,
                &id,
                &options,
                Path::new(reference),
                "ts-audio-seek-fold",
            )?);
        }
        if repeated {
            let mut options = namespace.to_vec();
            options.extend([pid.as_str(), "90000", "preroll", "100000"]);
            cases.push(input.record(
                root,
                FEATURE,
                &format!("{name}-repeated-preroll"),
                &options,
                Path::new(reference),
                "ts-audio-seek-fold",
            )?);
            let mut options = namespace.to_vec();
            let split = if m2ts { "split=576" } else { "split=564" };
            options.extend([pid.as_str(), "90000", "preroll", split]);
            cases.push(input.record(
                root,
                FEATURE,
                &format!("{name}-split-pes"),
                &options,
                Path::new(reference),
                "ts-audio-seek-fold",
            )?);
            if !m2ts {
                options.push("100000");
                cases.push(input.record(
                    root,
                    FEATURE,
                    &format!("{name}-split-repeated-preroll"),
                    &options,
                    Path::new(reference),
                    "ts-audio-seek-fold",
                )?);
            }
        }
    }
    for (name, stream_type, header, side) in [
        ("mpeg1-layer3-reservoir", 0x03, [0xff, 0xfb, 0x14, 0xc0], 5),
        ("mpeg2-layer3-reservoir", 0x04, [0xff, 0xf3, 0x44, 0xc0], 4),
    ] {
        let codec = Codec {
            name,
            stream_type,
            stream_id: 0xc0,
            dvb: false,
            bytes: Vec::new(),
            first: 96,
            header: 4,
        };
        let entry = silence(header, codec.first);
        let mut dependent = entry.clone();
        dependent[side] = if stream_type == 0x03 { 0x80 } else { 1 };
        let mut ts = Transport::new();
        ts.tables(&codec, &[])?;
        ts.pes(codec.stream_id, &part(&entry, Some(90_000), None))?;
        ts.pes(codec.stream_id, &part(&dependent, Some(93_000), None))?;
        ts.pes(codec.stream_id, &part(&entry, Some(96_000), None))?;
        let input = Input {
            name,
            m2ts: false,
            data: ts.finish(false),
        };
        cases.push(input.record(
            root,
            FEATURE,
            name,
            &[pid.as_str(), "95000", "preroll"],
            Path::new(reference),
            "ts-audio-seek-fold",
        )?);
    }
    latm_seek_cases(root, FEATURE, Path::new(reference), &mut cases)?;
    physical_audio_seek_cases(root, FEATURE, Path::new(reference), &mut cases)?;
    manifest(root, FEATURE, cases)
}

/// Exercise DTS core history and self-contained XLL seek entries.
pub fn author_dts_seek(root: &Path, args: &[String]) -> R<()> {
    let [record, reference] = args else {
        return Err("usage: cargo xtask fixture transport-dts-seek --record REFERENCE".into());
    };
    if record != "--record" {
        return Err("expected --record".into());
    }
    const FEATURE: &str = "mpegts-dts-seek";
    const SOURCE: &str = "harness/fixtures/mpegts/dts-core-mono.dts";
    const SHA256: &str = "a1db20abc4bce08a504108de2998337f4d906eda3373acdc3abab39fcaaf1330";
    let bytes = fs::read(root.join(SOURCE)).map_err(|e| format!("{SOURCE}: {e}"))?;
    if sha256::hex(&bytes) != SHA256 || bytes[4] & 2 != 0 {
        return Err(format!(
            "{SOURCE}: expected the checked core without optional header CRC"
        ));
    }
    let name = "dts-core-history";
    let codec = Codec {
        name,
        stream_type: 6,
        stream_id: 0xbd,
        dvb: true,
        bytes: Vec::new(),
        first: 512,
        header: 15,
    };
    let mut entry = Vec::from(&bytes[..512]);
    entry[10] &= !1;
    let mut dependent = entry.clone();
    dependent[10] |= 1;
    let mut ts = Transport::new();
    ts.tables(&codec, &[0x7b, 5, 0xd2, 0x87, 0x83, 0xfe, 0])?;
    for (i, frame) in [&entry, &dependent, &entry].into_iter().enumerate() {
        ts.pes(
            codec.stream_id,
            &part(frame, Some(90_000 + i as u64 * 960), None),
        )?;
    }
    let input = Input {
        name,
        m2ts: true,
        data: ts.finish(true),
    };
    let pid = PID.to_string();
    let mut cases = Vec::new();
    for (case, target, policy) in [
        ("dts-core-history-preroll", "91500", "preroll"),
        ("dts-core-history-next-entry", "91920", "previous"),
    ] {
        cases.push(input.record(
            root,
            FEATURE,
            case,
            &["--dvb", &pid, target, policy],
            Path::new(reference),
            "ts-audio-seek-fold",
        )?);
    }
    xll::seek_cases(root, FEATURE, Path::new(reference), &mut cases)?;
    manifest(root, FEATURE, cases)
}

fn extension_substream(index: u32, long: bool, asset_bytes: usize) -> Vec<u8> {
    let header_bytes = if long { 16 } else { 12 };
    let frame_bytes = header_bytes + asset_bytes;
    let mut bytes = vec![0; frame_bytes];
    bytes[..5].copy_from_slice(&[0x64, 0x58, 0x20, 0x25, 0xa5]);
    let mut position = 40;
    for (width, value) in [
        (2, index),
        (1, u32::from(long)),
        (if long { 12 } else { 8 }, header_bytes as u32 - 1),
        (if long { 20 } else { 16 }, frame_bytes as u32 - 1),
    ] {
        for bit in (0..width).rev() {
            bytes[position / 8] |= ((value >> bit) as u8 & 1) << (7 - position % 8);
            position += 1;
        }
    }
    let mut crc: u16 = 0xffff;
    for &byte in &bytes[5..header_bytes - 2] {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    bytes[header_bytes - 2..header_bytes].copy_from_slice(&crc.to_be_bytes());
    bytes[header_bytes..header_bytes + 8]
        .copy_from_slice(&[0x11, 0x8a, 0x95, 0xe1, 0x35, 0x55, 0x8c, 0x01]);
    bytes
}

/// Author aligned PES carrying a pinned core and opaque DTS-HD extension bodies.
pub fn author_dts_hd(root: &Path, args: &[String]) -> R<()> {
    let [record, reference] = args else {
        return Err("usage: cargo xtask fixture transport-dts-hd --record REFERENCE".into());
    };
    if record != "--record" {
        return Err("expected --record".into());
    }
    const FEATURE: &str = "mpegts-dts-hd";
    const SEED: &str = "harness/fixtures/mpegts/dts-core-mono.dts";
    const SHA256: &str = "a1db20abc4bce08a504108de2998337f4d906eda3373acdc3abab39fcaaf1330";
    let core = fs::read(root.join(SEED)).map_err(|e| format!("{SEED}: {e}"))?;
    if sha256::hex(&core) != SHA256 {
        return Err(format!("{SEED}: pinned coded bytes changed"));
    }
    let codec = Codec {
        name: "dvb-dts-hd",
        stream_type: 0x06,
        stream_id: 0xbd,
        dvb: true,
        bytes: Vec::new(),
        first: 0,
        header: 0,
    };
    let mut cases = Vec::new();
    for (name, m2ts, long, extra) in [
        ("dts-hd-core-extension-short", false, false, false),
        ("dts-hd-core-extension-long", true, true, false),
        ("dts-hd-core-two-extensions", false, false, true),
    ] {
        let mut ts = Transport::new();
        let mut descriptor = vec![
            0x7f, 14, 0x0e, 0xc0, 5, 1, 0x60, 0x08, 0, 0, 5, 1, 0x60, 0x48, 0, 0,
        ];
        if extra {
            descriptor[1] += 6;
            descriptor[3] |= 0x20;
            descriptor.extend([5, 1, 0x60, 0x48, 0, 0]);
        }
        ts.tables(&codec, &descriptor)?;
        let extension = extension_substream(0, long, 8);
        let second = extension_substream(1, long, 8);
        for (i, frame) in core.chunks_exact(512).take(3).enumerate() {
            let mut payload = Vec::from(frame);
            payload.extend_from_slice(&extension);
            if extra {
                payload.extend_from_slice(&second);
            }
            let pts = 90_000 + i as u64 * 960;
            let packet = Part {
                aligned: true,
                ..part(&payload, Some(pts), (i == 1).then_some(pts - 90))
            };
            ts.pes(codec.stream_id, &packet)?;
        }
        let input = Input {
            name,
            m2ts,
            data: ts.finish(m2ts),
        };
        cases.push(input.record(
            root,
            FEATURE,
            name,
            &["--dvb"],
            Path::new(reference),
            "ts-au-fold",
        )?);
    }
    extension_only_cases(root, FEATURE, &codec, Path::new(reference), &mut cases)?;
    manifest(root, FEATURE, cases)
}

fn extension_only_cases(
    root: &Path,
    feature: &str,
    codec: &Codec,
    reference: &Path,
    cases: &mut Vec<Value>,
) -> R<()> {
    let descriptor = &[0x7f, 8, 0x0e, 0x40, 5, 1, 0x60, 0x48, 0, 0];
    for (name, m2ts, long, asset_bytes) in [
        ("dts-hd-extension-only-short", false, false, 8),
        ("dts-hd-extension-only-long", true, true, 12 * 1024),
    ] {
        let mut ts = Transport::new();
        ts.tables(codec, descriptor)?;
        let payload = extension_substream(0, long, asset_bytes);
        for (index, pts) in [90_000, 91_920, 93_840].into_iter().enumerate() {
            let packet = Part {
                aligned: true,
                ..part(&payload, Some(pts), (index == 1).then_some(pts - 90))
            };
            ts.pes(codec.stream_id, &packet)?;
        }
        let input = Input {
            name,
            m2ts,
            data: ts.finish(m2ts),
        };
        cases.push(input.record(root, feature, name, &["--dvb"], reference, "ts-au-fold")?);
    }
    Ok(())
}
