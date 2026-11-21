//! Deterministic authored fixtures for bit readers and arithmetic kernels.

use std::fs;
use std::path::Path;

use crate::json::{self, Value};
use crate::pyrandom::PyRandom;
use crate::sha256;

type R<T> = Result<T, String>;

pub(super) const IDS: &[&str] = &["bit-readers", "rbsp-unescape", "dsp-windows", "dsp-divide"];

pub(super) struct Cases {
    root: std::path::PathBuf,
    directory: String,
    rows: Vec<Value>,
}

impl Cases {
    pub(super) fn new(root: &Path, feature: &str) -> R<Self> {
        let directory = format!("harness/features/{feature}/inputs");
        let root = root.join(&directory);
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        Ok(Self {
            root,
            directory,
            rows: Vec::new(),
        })
    }

    pub(super) fn add(&mut self, id: &str, profile: &str, data: &[u8], args: &[&str]) -> R<()> {
        let file = format!("{id}.case");
        let path = self.root.join(&file);
        if fs::read(&path).is_ok_and(|current| current == data) {
            // Leave the timestamps of existing inputs alone.
        } else {
            fs::write(path, data).map_err(|error| error.to_string())?;
        }
        self.rows.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(file)),
            ("sha256", Value::str(sha256::hex(data))),
            ("rights", Value::str("authored-synthetic")),
            ("arguments", Value::strings(args.iter().copied())),
        ]));
        Ok(())
    }

    pub(super) fn manifest(self, write: bool) -> R<()> {
        let path = self.root.with_file_name("corpus.json");
        let next = Value::object([
            ("schema", Value::int(1)),
            ("directory", Value::str(self.directory)),
            ("cases", Value::Array(self.rows)),
        ]);
        if write {
            json::write(&path, &next)?;
        } else {
            let current = json::read(&path)?;
            if current.get("directory") != next.get("directory") {
                return Err(format!("{}: corpus directory changed", path.display()));
            }
            let entries = |value: &Value| {
                let Some(Value::Array(cases)) = value.get("cases") else {
                    return Vec::new();
                };
                cases
                    .iter()
                    .map(|case| {
                        Value::object(
                            ["id", "file", "profile", "sha256", "rights", "arguments"]
                                .into_iter()
                                .filter_map(|field| {
                                    case.get(field).cloned().map(|value| (field, value))
                                }),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            if entries(&current) != entries(&next) {
                return Err(format!("{}: generated cases changed", path.display()));
            }
        }
        Ok(())
    }
}

fn script(data: &[u8], instructions: &str) -> Vec<u8> {
    let mut input = format!("{}\n", data.len()).into_bytes();
    input.extend_from_slice(data);
    input.extend_from_slice(instructions.as_bytes());
    input
}

fn bit_readers(root: &Path) -> R<Cases> {
    let mut cases = Cases::new(root, "bit-readers")?;
    let mut random = PyRandom::new(434);
    for length in [
        0_u32, 1, 2, 7, 8, 9, 16, 31, 32, 33, 64, 128, 255, 256, 1024, 4096,
    ] {
        let data = random.bytes(length as usize);
        let mut instructions = String::new();
        for width in 0..=64 {
            instructions.push_str(&format!("position\npeek {width}\nread {width}\n"));
            if width % 8 == 3 {
                instructions.push_str("align\nskip 3\n");
            }
        }
        let fixture = script(&data, &instructions);
        for reader in ["direct", "cached", "checked", "lsb", "rice"] {
            cases.add(
                &format!("{reader}-{length:04}"),
                reader,
                &fixture,
                &[reader],
            )?;
        }
    }
    for reader in ["direct", "cached", "checked", "lsb", "rice"] {
        for consumed in [55, 56, 57] {
            let text = format!(
                "skip 23\nskip 32\nread {}\nposition\npeek 1\nread 1\nread 0\nalign\nposition\n",
                consumed - 55
            );
            cases.add(
                &format!("{reader}-end-{consumed}"),
                reader,
                &script(b"\xf5\x6a\x00\xff\x5d\x99\x15", &text),
                &[reader],
            )?;
        }
    }
    for k in 0..=64 {
        let mut data = random.bytes(32);
        data[0] |= 0x80;
        let fixture = script(&data, &format!("rice {k}\nposition\nrice 0\nposition\n"));
        cases.add(
            &format!("rice-parameter-{k:02}"),
            "rice",
            &fixture,
            &["rice"],
        )?;
    }
    for (id, mut data) in [
        ("long", vec![0; 20]),
        ("no-stop", vec![0; 20]),
        ("short", vec![0; 8]),
    ] {
        if id == "long" {
            data[10..13].copy_from_slice(&[0x05, 0x3d, 0x80]);
        } else if id == "short" {
            data[7] = 1;
        }
        for k in [0, 5, 31] {
            let fixture = script(&data, &format!("rice {k}\nposition\n"));
            cases.add(&format!("rice-{id}-{k}"), "rice", &fixture, &["rice"])?;
        }
    }
    Ok(cases)
}

fn nal(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

fn annex_b(data: &[u8], header: usize) -> R<Vec<u8>> {
    let mut starts = Vec::new();
    let mut zeros = 0;
    for (i, &v) in data.iter().enumerate() {
        match v {
            0 => zeros += 1,
            1 if zeros >= 2 => {
                starts.push((i - zeros, i + 1));
                zeros = 0;
            }
            _ => zeros = 0,
        }
    }
    starts.push((data.len() - zeros, data.len()));
    let mut out = Vec::new();
    for pair in starts.windows(2) {
        let start = pair[0].1 + header;
        let Some(payload) = data.get(start..pair[1].0) else {
            return Err("short Annex B NAL header".into());
        };
        nal(&mut out, payload);
    }
    Ok(out)
}

fn rbsp(root: &Path) -> R<Cases> {
    let mut cases = Cases::new(root, "rbsp-unescape")?;
    let mut placements = Vec::new();
    for byte in 0..=255 {
        for prefix in [&[][..], b"\xa5", b"\xa5\x00"] {
            let mut input = prefix.to_vec();
            input.extend([0, 0, 3, byte, 0x80]);
            nal(&mut placements, &input);
        }
    }
    for payload in [
        &[][..],
        &[0],
        &[0, 0, 3],
        &[0, 0, 3, 0, 0, 3],
        &[0, 0, 3, 0, 0, 3, 3, 0x80],
        &[0, 0, 2],
        &[0, 0, 1],
        &[0, 0, 0],
    ] {
        nal(&mut placements, payload);
    }
    cases.add("emulation-prevention", "placement", &placements, &[])?;
    let mut random = PyRandom::new(741);
    let mut payloads = Vec::new();
    for n in 0..=256 {
        nal(&mut payloads, &random.bytes(n));
    }
    cases.add("seeded-payloads", "seeded", &payloads, &[])?;
    for (file, header) in [
        ("avc-aud0.h264", 1),
        ("avc-aud1.h264", 1),
        ("hevc-aud0.hevc", 2),
        ("hevc-aud1.hevc", 2),
        ("hevc-eos.hevc", 2),
    ] {
        let bytes = fs::read(root.join("crates/framing/fixtures/raw-annex-b").join(file))
            .map_err(|error| format!("{file}: {error}"))?;
        cases.add(
            file.split('.').next().unwrap_or(file),
            "annex-b",
            &annex_b(&bytes, header)?,
            &[],
        )?;
    }
    cases.add(
        "mixed-prefixes",
        "annex-b",
        &annex_b(b"\0\0\0\x01\x67\x80\0\0\x01\x68\0\0\x03\x01\x80", 1)?,
        &[],
    )?;
    Ok(cases)
}

fn windows(root: &Path) -> R<Cases> {
    let mut cases = Cases::new(root, "dsp-windows")?;
    for alpha in 0..=20 {
        let profile = if alpha == 0 { "vorbis" } else { "kbd" };
        for first in (1..=4096).step_by(128) {
            let shapes: String = (first..first + 128)
                .map(|length| format!("{length} {alpha}\n"))
                .collect();
            cases.add(
                &format!("{profile}-{alpha:02}-{first:04}"),
                profile,
                shapes.as_bytes(),
                &[],
            )?;
        }
    }
    Ok(cases)
}

fn divide(root: &Path) -> R<Cases> {
    let mut cases = Cases::new(root, "dsp-divide")?;
    for first in (1..=65535).step_by(4096) {
        let text = format!("range {first} {}\n", (first + 4095).min(65535));
        cases.add(
            &format!("divisor-{first:05}"),
            "boundary",
            text.as_bytes(),
            &[],
        )?;
    }
    let mut random = PyRandom::new(918);
    let mut pairs = String::new();
    for _ in 0..4096 {
        let a = random.below(u32::MAX) as i32;
        let d = 1 + random.below(i32::MAX as u32);
        let n = random.below(u32::MAX);
        pairs.push_str(&format!("{a} {n} {d}\n"));
        let d = 1 + random.below(65535);
        let n = random.below(u32::MAX) % ((i32::MAX as u32) / d + 1);
        pairs.push_str(&format!("{a} {n} {d}\n"));
    }
    for d in [
        65_536,
        65_537,
        1 << 20,
        i32::MAX as u32 - 1,
        i32::MAX as u32,
    ] {
        for n in [0, 1, u32::MAX - 1, u32::MAX] {
            pairs.push_str(&format!("{} {n} {d}\n", i32::MAX));
        }
    }
    cases.add("seeded-pairs", "seeded", pairs.as_bytes(), &[])?;
    Ok(cases)
}

pub(super) fn fixtures(root: &Path, id: &str, write: bool) -> R<()> {
    match id {
        "bit-readers" => bit_readers(root),
        "rbsp-unescape" => rbsp(root),
        "dsp-windows" => windows(root),
        "dsp-divide" => divide(root),
        _ => Err(format!("unknown authored corpus {id}")),
    }?
    .manifest(write)
}

/// Prepare the pinned RFC bitstreams on a checkout without the gate image's inputs.
pub(crate) fn fetch_opus(root: &Path, net: &dyn crate::fetch::Transport) -> R<Vec<String>> {
    const FEATURE: &str = "harness/features/opus-final-range/corpus.json";
    const ARCHIVE_SHA: &str = "6b26a22f9ba87b2b836906a9bb7afec5f8e54d49553b1200382520ee6fedfa55";
    const SOURCES: &[crate::fetch::Source<'_>] = &[
        crate::fetch::Source::Url(
            "https://opus-codec.org/static/testvectors/opus_testvectors-rfc8251.tar.gz",
        ),
        crate::fetch::Source::Url(
            "https://www.ietf.org/proceedings/98/slides/materials-98-codec-opus-newvectors-00.tar.gz",
        ),
    ];
    let manifest = root.join(FEATURE);
    if !manifest.is_file() {
        return Ok(Vec::new());
    }
    let manifest = json::read(&manifest)?;
    let rows = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing Opus testvectors")?;
    let directory = root.join(".research/opus-testvectors");
    let mut missing = Vec::new();
    let mut verified = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let name = format!("testvector{:02}.bit", index + 1);
        if row.get("file").and_then(Value::as_str) != Some(name.as_str()) {
            return Err(format!("unexpected Opus vector: {name}"));
        }
        let pin = crate::harness::validation::hash(row, "sha256")?;
        let destination = directory.join(&name);
        if destination.exists() {
            if crate::harness::linux::file_sha(&destination)? != pin {
                return Err(format!("{} differs from its pin", destination.display()));
            }
        } else {
            missing.push((name.clone(), destination, pin));
        }
        verified.push(name);
    }
    if missing.is_empty() {
        return Ok(verified);
    }
    let data = crate::fetch::pinned(
        net,
        SOURCES,
        ARCHIVE_SHA,
        80 << 20,
        "triergon-opus-testvectors",
    )?;
    let scratch = crate::process::TempDir::new(None)?;
    let archive = scratch.0.join("vectors.tar.gz");
    fs::write(&archive, data).map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    for (name, destination, pin) in missing {
        let output = std::process::Command::new("tar")
            .arg("-xOzf")
            .arg(&archive)
            .arg(format!("opus_newvectors/{name}"))
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() || sha256::hex(&output.stdout) != pin {
            return Err(format!("{name} differs from its pinned RFC vector"));
        }
        crate::fetch::publish(&destination, &output.stdout)?;
    }
    Ok(verified)
}
