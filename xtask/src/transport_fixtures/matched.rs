//! Independently authored TS controls for selected-track seek and audio framing.
use super::{Codec, PID, PROGRAM, R, Transport, part, source};
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

fn video(root: &Path, hevc: bool, repeat: usize) -> R<Input> {
    let codec = super::video(root, hevc)?;
    let stem = format!("crates/framing/fixtures/raw-annex-b/{}-aud0", codec.name);
    let raw = source(
        root,
        &format!("{stem}.{}", if hevc { "hevc" } else { "h264" }),
    )?;
    let index =
        String::from_utf8(source(root, &format!("{stem}.packets"))?).map_err(|e| e.to_string())?;
    let mut ts = Transport::new();
    ts.tables(&codec, &[])?;
    let mut i = 0_u64;
    for _ in 0..repeat {
        for line in index.lines() {
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

/// Rewrap the checked DTS core frames with literal DVB descriptors and PES clocks.
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
    manifest(root, FEATURE, cases)
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
        }
    }
    manifest(root, FEATURE, cases)
}
