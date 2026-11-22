//! Shared authored streams and pinned FATE derivatives for the DTS comparisons.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::harness::{manifest, report, validation, worker};
use crate::json::{self, Value};
use crate::sha256;

pub(super) const IDS: [&str; 5] = [
    "dts-core-frames",
    "dts-core-header",
    "dts-packed-frames",
    "dts-unpacking",
    "dts-parser-trace",
];
pub(super) const DIRECTORY: &str = ".research/dts-corpus";
const FIXTURES: &str = "crates/framing/fixtures";
const DECODER: &str = ".research/reference-decode/install/bin/ffmpeg";
const WHOLE: &str = "268435456";
const MAXIMUM: &str = "16384";
const PACKINGS: [&str; 3] = ["be14", "le14", "le16"];
const WINDOWS: [&str; 5] = ["1", "7", "100", "4096", WHOLE];

type Result<T> = std::result::Result<T, String>;

struct Corpus {
    directory: PathBuf,
    cases: [Vec<Value>; IDS.len()],
    /// The streams [`Corpus::core`] has registered, by SHA-256 and frame limit: a
    /// stream met again under another name is compared once, under its first name.
    streams: BTreeSet<(String, String)>,
}

impl Corpus {
    fn input(&mut self, id: &str, bytes: &[u8], fetched: bool) -> Result<Input> {
        let file = format!("{id}.dts");
        let path = self.directory.join(&file);
        if fs::read(&path).as_deref().ok() != Some(bytes) {
            // Lane preparation may run beside another harness invocation.
            let pending = self
                .directory
                .join(format!("{file}.{}.partial", std::process::id()));
            fs::write(&pending, bytes)
                .map_err(|error| format!("{}: {error}", pending.display()))?;
            fs::rename(&pending, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        let sha256 = sha256::hex(bytes);
        let rights = if fetched {
            "fetched-not-redistributed"
        } else {
            "authored-synthetic"
        };
        Ok(Input {
            file,
            sha256,
            rights,
        })
    }

    fn case(&mut self, feature: usize, input: &Input, id: &str, profile: &str, args: &[&str]) {
        self.cases[feature].push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(&input.file)),
            ("sha256", Value::str(&input.sha256)),
            ("rights", Value::str(input.rights)),
            ("arguments", Value::strings(args.iter().copied())),
        ]));
    }

    fn core(&mut self, id: &str, profile: &str, bytes: &[u8], limit: &str) -> Result<()> {
        if !self.streams.insert((sha256::hex(bytes), limit.to_owned())) {
            return Ok(());
        }
        let source = self.input(id, bytes, false)?;
        self.case(0, &source, id, profile, &["4096", limit, "frames"]);
        for packing in PACKINGS {
            let id = format!("{id}-{packing}");
            let bytes = pack_frames(bytes, packing);
            let input = self.input(&id, &bytes, false)?;
            self.case(2, &input, &id, packing, &["normal", packing, "4096"]);
            self.case(3, &input, &id, packing, &[packing]);
        }
        Ok(())
    }

    fn native(&mut self, id: &str, packing: &str, bytes: &[u8], all_windows: bool) -> Result<()> {
        let source = self.input(id, bytes, false)?;
        self.case(
            2,
            &source,
            &format!("{id}-carrier"),
            packing,
            &["carrier14", packing, "4096"],
        );
        self.case(
            2,
            &source,
            &format!("{id}-sync"),
            packing,
            &["sync14", packing, "4096"],
        );
        self.case(
            2,
            &source,
            &format!("{id}-normal"),
            packing,
            &["normal", packing, "4096"],
        );
        self.case(3, &source, id, packing, &[packing]);
        if id.starts_with("exact-") || id.starts_with("unused-high-") {
            self.case(
                3,
                &source,
                &format!("{id}-canonical"),
                "sync14",
                &[packing, "frames"],
            );
        }

        for window in if all_windows {
            WINDOWS.as_slice()
        } else {
            &WINDOWS[3..4]
        } {
            let suffix = if *window == WHOLE { "whole" } else { window };
            self.case(
                4,
                &source,
                &format!("{id}-w{suffix}"),
                packing,
                &[packing, window],
            );
        }
        Ok(())
    }
}

struct Input {
    file: String,
    sha256: String,
    rights: &'static str,
}

fn core_extent(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(&[0x7f, 0xfe, 0x80, 0x01]) {
        return None;
    }
    let [a, b, c] = *bytes.get(5..8)? else {
        return None;
    };
    let code = (usize::from(a & 3) << 12) | (usize::from(b) << 4) | usize::from(c >> 4);
    Some(code + 1)
}

pub(crate) fn pack_frames(mut bytes: &[u8], packing: &str) -> Vec<u8> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let len = core_extent(bytes)
            .filter(|&size| size >= 96)
            .unwrap_or(bytes.len())
            .min(bytes.len());
        let (frame, remaining) = bytes.split_at(len);
        bytes = remaining;
        if packing == "le16" {
            for pair in frame.chunks(2) {
                result.extend(pair.iter().rev());
            }
        } else {
            for at in (0..frame.len() * 8).step_by(14) {
                let mut word = 0;
                for i in at..at + 14 {
                    let bit = frame.get(i / 8).map_or(0, |byte| (byte >> (7 - i % 8)) & 1);
                    word = (word << 1) | u16::from(bit);
                }
                // The second sync word's upper two bits are retained as marker bits.
                if at == 14 && frame.starts_with(&[0x7f, 0xfe, 0x80, 0x01]) {
                    word |= 0xc000;
                }
                result.extend(if packing == "be14" {
                    word.to_be_bytes()
                } else {
                    word.to_le_bytes()
                });
            }
        }
    }
    result
}

fn authored(root: &Path, corpus: &mut Corpus) -> Result<()> {
    let mut matrix = BTreeMap::new();
    for (file, label) in [
        ("dts-core-header-author-v1.tsv", "header"),
        ("dts-hcrc-known-v1.tsv", "crc"),
        ("dts-inspect-source-v1.tsv", "source"),
    ] {
        let text = fs::read_to_string(root.join(FIXTURES).join(file)).map_err(|e| e.to_string())?;
        for line in text
            .lines()
            .filter(|row| !row.starts_with('#') && !row.starts_with("id\t"))
        {
            let mut columns = line.split('\t');
            let id = format!(
                "{label}-{}",
                columns.next().ok_or("DTS fixture id")?.replace("::", "-")
            );
            let bytes = hex(columns.next().ok_or("DTS fixture bytes")?)?;
            let input = corpus.input(&id, &bytes, false)?;
            corpus.case(1, &input, &id, "prefix", &["prefix"]);
            if label == "header" && !bytes.is_empty() {
                matrix.insert(id, bytes);
            }
        }
    }
    // Preserve every complete authored header code in a full frame even when
    // the header is expected to be refused. The body is deliberately opaque.
    for (id, mut bytes) in matrix {
        if !id.contains("short-header-")
            && let Some(size) = core_extent(&bytes)
            && bytes.len() < size
        {
            bytes.resize(size, 0x55);
        }
        corpus.core(
            &id.replacen("header-", "matrix-", 1),
            "matrix",
            &bytes,
            MAXIMUM,
        )?;
        if id.starts_with("header-extension-") {
            let id = id.replacen("header-", "parser-", 1);
            let packed = pack_frames(&bytes, "be14");
            let source = corpus.input(&id, &packed, false)?;
            corpus.case(4, &source, &id, "be14", &["be14", "4096"]);
        }
    }

    let text = fs::read_to_string(root.join(FIXTURES).join("dts-core-frame-author-v1.tsv"))
        .map_err(|error| error.to_string())?;
    let mut minimum = Vec::new();
    for row in text.lines().skip(1) {
        let mut columns = row.split('\t');
        let id = columns.next().ok_or("DTS frame id")?;
        let bytes = hex(columns.next().ok_or("DTS frame bytes")?)?;
        if id == "minimum" {
            minimum.clone_from(&bytes);
        }
        corpus.core(id, "authored", &bytes, MAXIMUM)?;
        let source = corpus.input(id, &bytes, false)?;
        corpus.case(1, &source, id, "stream", &["stream"]);
        if matches!(id, "minimum" | "maximum") {
            corpus.case(
                2,
                &source,
                &format!("{id}-be16"),
                "be16",
                &["normal", "be16", "4096"],
            );
        }
    }
    if minimum.len() != 96 {
        return Err("missing authored minimum DTS frame".into());
    }
    for len in 0..15 {
        corpus.core(
            &format!("header-prefix-{len:02}"),
            "boundaries",
            &minimum[..len],
            MAXIMUM,
        )?;
    }
    let mut arrangements = Vec::new();
    for mode in 17_u8..63 {
        let mut frame = minimum.clone();
        frame[7] = (frame[7] & 0xf0) | (mode >> 2);
        frame[8] = (frame[8] & 0x3f) | ((mode & 3) << 6);
        frame[13..].fill(0x55);
        arrangements.extend(frame);
    }
    corpus.core("user-defined-amode", "matrix", &arrangements, MAXIMUM)?;
    let source = corpus.input("user-defined-amode", &arrangements, false)?;
    corpus.case(1, &source, "user-defined-amode", "stream", &["stream"]);
    corpus.core("over-maximum-frame-bytes", "boundaries", &minimum, "95")?;
    let mut extension_sync = pack_frames(&minimum, "be14");
    extension_sync[96..100].copy_from_slice(&[0x64, 0x58, 0x20, 0x25]);
    let source = corpus.input("parser-extension-substream-sync", &extension_sync, false)?;
    corpus.case(
        4,
        &source,
        "parser-extension-substream-sync",
        "be14",
        &["be14", "4096"],
    );

    let mut undersize = minimum.clone();
    undersize[7] &= 0xef; // FSIZE 94
    undersize.truncate(95);
    corpus.core("below-minimum-fsize", "boundaries", &undersize, MAXIMUM)?;
    let mut changed_rate = minimum.clone();
    changed_rate[8] = (changed_rate[8] & 0xc3) | (8 << 2);
    corpus.core(
        "rate-change",
        "boundaries",
        &[minimum.as_slice(), &changed_rate, &minimum].concat(),
        MAXIMUM,
    )?;
    corpus.core(
        "interframe-garbage",
        "boundaries",
        &[minimum.as_slice(), &[0x37, 0x50, 0x12, 0x3e], &minimum].concat(),
        MAXIMUM,
    )?;

    for dir in ["dts-native-v1", "dts-operation-v1"] {
        let mut paths = fs::read_dir(root.join(FIXTURES).join(dir))
            .map_err(|e| e.to_string())?
            .map(|result| result.map(|entry| entry.path()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            if path.extension().is_none_or(|ext| ext != "bin") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("invalid DTS fixture name")?;
            let packing = if id.ends_with("little") || id.ends_with("-le") {
                "le14"
            } else {
                "be14"
            };
            corpus.native(
                id,
                packing,
                &fs::read(path.clone()).map_err(|e| e.to_string())?,
                true,
            )?;
        }
    }
    let text = fs::read_to_string(root.join(FIXTURES).join("dts14-sync-v1.tsv"))
        .map_err(|e| e.to_string())?;
    for row in text.lines().filter(|row| !row.starts_with('#')) {
        let [id, order, source, ..] = row.split('\t').collect::<Vec<_>>()[..] else {
            return Err("invalid DTS sync fixture".into());
        };
        let packing = if order == "big" { "be14" } else { "le14" };
        corpus.native(id, packing, &hex(source)?, false)?;
    }
    // Table 7-2 short and long size fields: both extensions declare a
    // 16-byte header and 32-byte frame. Asset contents are opaque to framing.
    let mut with_extensions = minimum.clone();
    for sizes in [
        &[0x01, 0xe0, 0x03, 0xe0][..],
        &[0x20, 0x1e, 0x00, 0x03, 0xe0],
    ] {
        let start = with_extensions.len();
        with_extensions.extend([0x64, 0x58, 0x20, 0x25, 0]);
        with_extensions.extend(sizes);
        with_extensions.resize(start + 32, 0);
        with_extensions.extend(&minimum);
    }
    let source = corpus.input("core-plus-exss", &with_extensions, false)?;
    corpus.case(
        0,
        &source,
        "core-plus-exss",
        "authored",
        &["1", MAXIMUM, "core"],
    );
    Ok(())
}

fn hex(text: &str) -> Result<Vec<u8>> {
    if text == "~" {
        return Ok(Vec::new());
    }
    crate::worker_tests::unhex(text)
}

fn fetched(root: &Path, corpus: &mut Corpus) -> Result<()> {
    let recipes = crate::codecs::derived::streams_matching(root, ".core.dts")?;
    if recipes.len() != 23 {
        return Err(format!(
            "DTS corpus requires 23 pinned streams, found {}",
            recipes.len()
        ));
    }
    for recipe in &recipes {
        let path = root.join(&recipe.path);
        if !path.is_file() && !crate::codecs::derived::make(root, &root.join(DECODER), recipe)? {
            return Err(format!(
                "{} does not match its derivation pin",
                path.display()
            ));
        }
        let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if sha256::hex(&bytes) != recipe.sha256 {
            return Err(format!("{} does not match its pin", path.display()));
        }
        let id = recipe.name.trim_end_matches(".core.dts");
        let source = corpus.input(&format!("{id}.core"), &bytes, true)?;
        corpus.case(0, &source, id, "fate", &["4096", MAXIMUM, "frames"]);
        corpus.case(
            0,
            &source,
            &format!("{id}-decoded"),
            "decoded",
            &["4096", MAXIMUM, "decoded"],
        );
        corpus.case(1, &source, id, "stream", &["stream"]);
        if id == "core_51_24_48_768_0" {
            corpus.case(
                2,
                &source,
                &format!("{id}-be16"),
                "be16",
                &["normal", "be16", "4096"],
            );
        }

        // A coreless stream stays in the core comparison as a refusal control.
        let Some(first) = core_extent(&bytes).and_then(|size| bytes.get(..size)) else {
            continue;
        };
        for packing in PACKINGS {
            let packed_id = format!("{id}-first-{packing}");
            let input = corpus.input(&packed_id, &pack_frames(first, packing), true)?;
            corpus.case(2, &input, &packed_id, packing, &["normal", packing, "4096"]);
            corpus.case(3, &input, &packed_id, "decode", &[packing]);
        }
    }
    Ok(())
}

fn authored_at(root: &Path, output: &Path) -> Result<Corpus> {
    let directory = output.join(DIRECTORY);
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let mut corpus = Corpus {
        directory,
        cases: std::array::from_fn(|_| Vec::new()),
        streams: BTreeSet::new(),
    };
    authored(root, &mut corpus)?;
    Ok(corpus)
}

/// Generate the authored subset for static gate preflight without fetching media.
pub(crate) fn gate_fixtures(root: &Path, output: &Path) -> Result<()> {
    authored_at(root, output).map(|_| ())
}

pub(super) fn fixtures(root: &Path) -> Result<()> {
    let mut corpus = authored_at(root, root)?;
    fetched(root, &mut corpus)?;
    for (id, generated) in IDS.into_iter().zip(corpus.cases) {
        let path = root.join(format!("harness/features/{id}/corpus.json"));
        let (listed, _) = manifest::read(root, &path)?;
        let expected = listed
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("DTS manifest needs cases")?;
        if expected.len() != generated.len()
            || expected.iter().zip(generated).any(|(case, fixture)| {
                ["id", "profile", "file", "sha256", "rights", "arguments"]
                    .into_iter()
                    .any(|field| case.get(field) != fixture.get(field))
            })
        {
            return Err(format!(
                "{} differs from the fixture recipe",
                path.display()
            ));
        }
    }
    println!("prepared DTS fixtures in {DIRECTORY}");
    Ok(())
}

/// Recheck the reference PCM pin for each real packed frame. Record mode updates
/// the pin only after both representations have decoded to the same nonempty PCM.
pub(super) fn decode_pins(root: &Path, listing: &mut Value, record: bool) -> Result<()> {
    let identity = json::read(&root.join("target/harness/identity.json"))?;
    let candidate =
        crate::harness::build::candidate_worker(root, &identity, "framing-views-worker")?;
    let scratch = crate::worker_tests::Scratch::new()?;
    let Value::Object(fields) = listing else {
        return Err("expected DTS manifest".into());
    };
    let Some((_, Value::Array(cases))) = fields.iter_mut().find(|(key, _)| key == "cases") else {
        return Err("DTS manifest has no cases".into());
    };
    let mut checked = 0;
    for case in cases
        .iter_mut()
        .filter(|case| case.get("profile").and_then(Value::as_str) == Some("decode"))
    {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .ok_or("DTS case needs an id")?;
        let file = case
            .get("file")
            .and_then(Value::as_str)
            .ok_or("DTS case needs a file")?;
        let directory = root.join(DIRECTORY);
        if let Some(problem) = report::input_problem(&directory, case, Some("dts-unpack-"))? {
            return Err(format!("{id}: {problem}"));
        }
        let input = directory.join(file);
        let mut command = vec![
            candidate.as_os_str().to_owned(),
            "dts-unpack-trace".into(),
            input.clone().into(),
        ];
        command.extend(report::case_arguments(case));
        let result = worker::run_in(
            &command,
            &[],
            Some(root),
            std::time::Duration::from_secs(30),
        )?
        .filter(|result| result.code == 0)
        .ok_or_else(|| format!("{id}: DTS trace failed"))?;
        let text = std::str::from_utf8(&result.stdout).map_err(|error| error.to_string())?;
        let hex = text
            .lines()
            .next()
            .and_then(|row| row.strip_prefix("1 "))
            .and_then(|row| row.split_once(' '))
            .map(|(_, bytes)| bytes)
            .ok_or_else(|| format!("{id}: no canonical bytes"))?;
        let bytes = crate::worker_tests::unhex(hex)?;
        let size =
            core_extent(&bytes).ok_or_else(|| format!("{id}: missing canonical core header"))?;
        let frame = bytes
            .get(..size)
            .ok_or_else(|| format!("{id}: incomplete canonical frame"))?;
        let canonical = scratch.file("canonical.dts", frame)?;
        let packed = pcm_md5(root, &input)?;
        let unpacked = pcm_md5(root, &canonical)?;
        if packed != unpacked {
            return Err(format!(
                "{id}: packed PCM {packed} differs from canonical PCM {unpacked}"
            ));
        }
        if record {
            validation::put(case, "decode_md5", Value::str(packed))?;
        } else if case.get("decode_md5").and_then(Value::as_str) != Some(packed.as_str()) {
            return Err(format!("{id}: decoded PCM {packed} differs from pin"));
        }
        checked += 1;
    }
    if checked == 0 {
        return Err("DTS manifest has no decoded controls".into());
    }
    println!("verified {checked} packed DTS PCM hashes");
    Ok(())
}

fn pcm_md5(root: &Path, input: &Path) -> Result<String> {
    let result = Command::new(root.join(DECODER))
        .args(["-nostdin", "-v", "error", "-xerror", "-f", "dts", "-i"])
        .arg(input)
        .args(["-map", "0:a:0", "-c:a", "pcm_s16le", "-f", "md5", "-"])
        .output()
        .map_err(|error| error.to_string())?;
    if !result.status.success() {
        return Err(format!(
            "{}: {}",
            input.display(),
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let text = std::str::from_utf8(&result.stdout).map_err(|error| error.to_string())?;
    text.trim()
        .strip_prefix("MD5=")
        .filter(|hash| {
            hash.len() == 32
                && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                && *hash != "d41d8cd98f00b204e9800998ecf8427e"
        })
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{}: empty or invalid PCM hash", input.display()))
}

pub(crate) fn verify_decode_pins(root: &Path) -> Result<()> {
    let (mut listing, _) = manifest::read(
        root,
        &root.join("harness/features/dts-unpacking/corpus.json"),
    )?;
    decode_pins(root, &mut listing, false)
}
