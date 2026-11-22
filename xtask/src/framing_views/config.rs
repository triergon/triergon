//! Authored codec configuration controls and reused pinned decoder vectors.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    codecs::derived,
    corpus::Paths,
    json::{self, Value},
    sha256,
    worker_tests::unhex,
};

type Result<T> = std::result::Result<T, String>;

pub(super) const IDS: [&str; 4] = [
    "codec-config-views",
    "hevc-hvcc-prefix",
    "vp9-config",
    "eac3-bsi",
];
const DIRECTORY: &str = "config-view-fixtures";
const AAC_ACCESS_UNITS: &[(&str, &str, &str)] = &[
    ("asc-frame-lc", "al18_44.adts", "1208"),
    ("asc-frame-explicit-sbr", "al_sbr_cm_48_2.adts", "2b118800"),
    ("asc-frame-implicit-sbr", "al_sbr_cm_48_2.adts", "1310"),
];

#[derive(Clone, Copy)]
enum View {
    Reference,
    Hvcc,
    Vpcc,
    Eac3,
}

struct Corpus {
    root: PathBuf,
    cases: [Vec<Value>; 4],
}

impl Corpus {
    fn case(&mut self, view: View, id: &str, profile: &str, input: [&str; 3], args: &[&str]) {
        let [file, hash, rights] = input;
        self.cases[view as usize].push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(file)),
            ("sha256", Value::str(hash)),
            ("rights", Value::str(rights)),
            ("arguments", Value::strings(args.iter().copied())),
        ]));
    }

    fn authored(
        &mut self,
        view: View,
        id: &str,
        profile: &str,
        bytes: &[u8],
        args: &[&str],
    ) -> Result<()> {
        let file = format!("{DIRECTORY}/{id}.bin");
        let path = self.root.join(&file);
        if fs::read(&path).as_deref().ok() != Some(bytes) {
            let pending = path.with_extension(format!("{}.partial", std::process::id()));
            fs::write(&pending, bytes)
                .map_err(|error| format!("{}: {error}", pending.display()))?;
            fs::rename(&pending, &path).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        self.case(
            view,
            id,
            profile,
            [&file, &sha256::hex(bytes), "authored-synthetic"],
            args,
        );
        Ok(())
    }

    fn fetched(
        &mut self,
        view: View,
        id: &str,
        profile: &str,
        input: &derived::Stream,
        args: &[&str],
    ) -> Result<()> {
        let file = input
            .path
            .strip_prefix(".research/")
            .ok_or("vector outside corpus directory")?;
        self.case(
            view,
            id,
            profile,
            [file, &input.sha256, "fetched-not-redistributed"],
            args,
        );
        Ok(())
    }
}

fn text(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn fixtures(paths: &Paths, drafts: bool) -> Result<String> {
    let directory = paths.fixtures(DIRECTORY)?;
    let mut corpus = Corpus {
        root: directory
            .parent()
            .ok_or("missing corpus parent")?
            .to_path_buf(),
        cases: std::array::from_fn(|_| Vec::new()),
    };
    hvcc(&paths.root, &mut corpus)?;
    vpcc(&paths.root, &mut corpus)?;
    asc(&paths.root, &mut corpus)?;
    eac3(&paths.root, &mut corpus)?;
    for (id, cases) in IDS.into_iter().zip(corpus.cases) {
        if drafts {
            json::write(
                &directory.join(format!("{id}-draft.json")),
                &Value::object([
                    ("schema", Value::int(1)),
                    ("directory", Value::str(".research")),
                    ("cases", Value::Array(cases)),
                ]),
            )?;
        } else {
            let path = paths
                .root
                .join(format!("harness/features/{id}/corpus.json"));
            let pinned = json::read(&path)?;
            let Some(current) = pinned.get("cases").and_then(Value::as_array) else {
                return Err(format!("{}: missing cases", path.display()));
            };
            if cases.len() != current.len()
                || cases.iter().zip(current).any(|(made, pinned)| {
                    ["id", "profile", "file", "sha256", "rights", "arguments"]
                        .into_iter()
                        .any(|key| made.get(key) != pinned.get(key))
                })
            {
                return Err(format!(
                    "{}: configuration corpus membership or input changed",
                    path.display()
                ));
            }
        }
    }
    Ok("Prepared the authored codec configuration views".into())
}

fn hvcc(root: &Path, corpus: &mut Corpus) -> Result<()> {
    let named = [
        0x01, 0xa5, 0x81, 0x23, 0x45, 0x67, 0x92, 0x34, 0x56, 0x78, 0x9a, 0xbc, 123, 0xfa, 0xbc,
        0xfd, 0xfe, 0xfa, 0xfb, 0x12, 0x34, 0xb7, 0,
    ];
    for size in 0..=named.len() {
        corpus.authored(
            View::Hvcc,
            &format!("hvcc-prefix-{size}"),
            "authored",
            &named[..size],
            &[],
        )?;
    }
    let mut tail = named.to_vec();
    tail.extend([0xfa, 7, 0, 6]);
    corpus.authored(View::Hvcc, "hvcc-opaque-tail", "authored", &tail, &[])?;
    let fields = [
        ("version", 0, 8),
        ("space", 8, 2),
        ("tier", 10, 1),
        ("profile", 11, 5),
        ("compatibility", 16, 32),
        ("constraints", 48, 48),
        ("level", 96, 8),
        ("segmentation", 108, 12),
        ("parallelism", 126, 2),
        ("chroma", 134, 2),
        ("luma-depth", 141, 3),
        ("chroma-depth", 149, 3),
        ("average-rate", 152, 16),
        ("constant-rate", 168, 2),
        ("layers", 170, 3),
        ("nested", 173, 1),
        ("length-size", 174, 2),
        ("arrays", 176, 8),
    ];
    for (field, offset, width) in fields {
        let maximum = (1_u64 << width) - 1;
        for (name, value) in [
            ("zero", 0),
            ("one", 1),
            ("mid", maximum / 2),
            ("max", maximum),
        ] {
            let mut record = named;
            for i in 0..width {
                let at = offset + i;
                let mask = 0x80 >> (at % 8);
                record[at / 8] = (record[at / 8] & !mask)
                    | if value & (1 << (width - i - 1)) != 0 {
                        mask
                    } else {
                        0
                    };
            }
            corpus.authored(
                View::Hvcc,
                &format!("hvcc-{field}-{name}"),
                "authored",
                &record,
                &[],
            )?;
        }
    }
    for (label, offset, mask) in [
        ("segmentation", 13, 0x80),
        ("parallelism", 15, 0x80),
        ("chroma", 16, 0x80),
        ("luma-depth", 17, 0x80),
        ("chroma-depth", 18, 0x80),
        ("three-byte-length", 21, 1),
    ] {
        let mut record = named;
        record[offset] &= !mask;
        corpus.authored(
            View::Hvcc,
            &format!("hvcc-issue-{label}"),
            "authored",
            &record,
            &[],
        )?;
    }

    let scripts = json::read(&root.join("harness/features/packet-framing/inputs.json"))?;
    for case in scripts
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing framing cases")?
    {
        let data = crate::worker_tests::hex_bytes(case.get("hex"))?;
        if data.get(8) != Some(&2) {
            continue;
        }
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .ok_or("missing framing case name")?;
        let count = u32::from_be_bytes(data[9..13].try_into().unwrap()) as usize;
        let config = data
            .get(13..13 + count)
            .ok_or("short framing script config")?;
        corpus.authored(
            View::Hvcc,
            &format!("hvcc-script-{id}"),
            "packet-framing",
            config,
            &[],
        )?;
    }
    for stem in ["hevc-aud0", "hevc-aud1", "hevc-eos"] {
        let path = root.join(format!(
            "harness/features/codec-config-views/reference-written/{stem}.mp4"
        ));
        corpus.authored(
            View::Reference,
            stem,
            "reference-written",
            &bytes(&path)?,
            &["hvcc-written"],
        )?;
    }
    Ok(())
}

fn vpcc(root: &Path, corpus: &mut Corpus) -> Result<()> {
    let table = text(&root.join("crates/framing/fixtures/vp9-config-v1.tsv"))?;
    for line in table.lines().skip(1) {
        let columns: Vec<_> = line.split('\t').collect();
        let mut flags = columns[3]
            .parse::<u32>()
            .map_err(|error| error.to_string())?
            .to_be_bytes();
        let oversized = flags[0] != 0;
        flags[0] = columns[2]
            .parse::<u8>()
            .map_err(|error| error.to_string())?;
        let mut record = flags.to_vec();
        if columns[1] != "~" {
            record.extend(unhex(columns[1])?);
        }
        if oversized {
            let id = format!("vpcc-{}", columns[0]);
            corpus.authored(View::Vpcc, &id, "authored", &record, &[columns[3]])?;
        } else {
            let id = format!("vpcc-{}", columns[0]);
            corpus.authored(View::Vpcc, &id, "authored", &record, &[])?;
            corpus.authored(View::Reference, &id, "vpcc", &record, &["vpcc"])?;
        }
    }
    let mut sample = |name: &str, raw: &[u8]| {
        let id = format!("vpcc-{name}");
        corpus.authored(View::Vpcc, &id, "authored", raw, &[])?;
        corpus.authored(View::Reference, &id, "vpcc", raw, &["vpcc"])
    };
    let mut record = [1, 0, 0, 0, 0, 10, 0x81, 1, 1, 1, 0, 0];
    for size in 0..4 {
        sample(&format!("short-fullbox-{size}"), &record[..size])?;
    }
    for level in [0, 10, 11, 20, 21, 30, 31, 40, 41, 50, 51, 52, 60, 61, 62] {
        record[5] = level;
        sample(&format!("level-{level}"), &record)?;
    }
    record[5] = 10;
    for (label, packed) in [
        ("depth-nine", 0x91),
        ("chroma-reserved", 0x89),
        ("limited-range", 0x80),
    ] {
        record[6] = packed;
        sample(label, &record)?;
    }
    for value in [0, 1, 2, 3, 4, 9, 16, 18, 22, 255] {
        record[7..10].fill(value);
        sample(&format!("cicp-{value}"), &record)?;
    }
    Ok(())
}

#[derive(Default)]
struct Bits {
    data: Vec<u8>,
    count: usize,
}

impl Bits {
    fn put(&mut self, value: u32, width: usize) {
        for i in (0..width).rev() {
            if self.count.is_multiple_of(8) {
                self.data.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.data.last_mut().unwrap() |= bit << (7 - self.count % 8);
            self.count += 1;
        }
    }

    fn fields(&mut self, list: &[(u32, usize)]) {
        for &(value, width) in list {
            self.put(value, width);
        }
    }

    fn align(&mut self) {
        self.put(0, (8 - self.count % 8) % 8);
    }
}

fn ga(object: u32, index: u32, channel: u32) -> Bits {
    let mut bits = Bits::default();
    bits.fields(&[(object, 5), (index, 4)]);
    if index == 15 {
        bits.put(48_000, 24);
    }
    bits.fields(&[(channel, 4), (0, 3)]);
    bits
}

fn pce(counts: [u32; 6], tagged_mix: bool) -> Bits {
    let mut bits = ga(2, 3, 0);
    bits.fields(&[(9, 4), (1, 2), (3, 4)]);
    for (count, width) in counts.into_iter().zip([4, 4, 4, 2, 3, 4]) {
        bits.put(count, width);
    }
    if tagged_mix {
        bits.fields(&[(1, 1), (7, 4), (1, 1), (2, 4), (1, 1), (3, 2), (1, 1)]);
    } else {
        bits.put(0, 3);
    }
    for (kind, count) in counts.into_iter().enumerate() {
        for tag in 0..count {
            if kind < 3 || kind == 5 {
                bits.put(1, 1);
            }
            bits.put(tag % 16, 4);
        }
    }
    bits.align();
    bits.put(0, 8);
    bits
}

fn asc(root: &Path, corpus: &mut Corpus) -> Result<()> {
    let mut sample = |name: &str, raw: &[u8], bits: usize| {
        corpus.authored(
            View::Reference,
            &format!("asc-{name}"),
            "asc",
            raw,
            &["asc", &bits.to_string()],
        )
    };
    for line in text(&root.join("crates/framing/tests/aac-fixtures/original15.tsv"))?
        .lines()
        .skip(1)
    {
        let columns: Vec<_> = line.split('\t').collect();
        let record = unhex(columns[1])?;
        sample(&columns[0].to_ascii_lowercase(), &record, record.len() * 8)?;
    }
    for index in 0..=15 {
        let bits = ga(2, index, 2);
        sample(&format!("sampling-index-{index}"), &bits.data, bits.count)?;
    }
    for channel in 0..=15 {
        let bits = ga(2, 3, channel);
        sample(&format!("channel-{channel}"), &bits.data, bits.count)?;
    }
    for object in [0, 1, 3, 4, 6, 17, 23, 30] {
        let bits = ga(object, 3, 2);
        sample(&format!("object-{object}"), &bits.data, bits.count)?;
    }
    for extension in [0, 10, 63] {
        let mut bits = Bits::default();
        bits.fields(&[(31, 5), (extension, 6), (3, 4), (2, 4), (0, 3)]);
        sample(
            &format!("escaped-object-{extension}"),
            &bits.data,
            bits.count,
        )?;
    }
    for (index, label) in ["front", "side", "back", "lfe", "data", "coupling"]
        .into_iter()
        .enumerate()
    {
        let mut counts = [1, 0, 0, 0, 0, 0];
        counts[index] = 1;
        let record = pce(counts, false);
        sample(&format!("pce-{label}"), &record.data, record.count)?;
    }
    for (label, counts) in [
        ("empty", [0; 6]),
        ("every-kind", [1; 6]),
        ("max-elements", [15, 15, 15, 3, 7, 15]),
    ] {
        let record = pce(counts, true);
        sample(&format!("pce-{label}"), &record.data, record.count)?;
    }
    let record = bytes(&root.join("crates/framing/tests/aac-fixtures/compat-pce-93.bin"))?;
    sample("pce-declared-93", &record, 93)?;
    sample("pce-physical", &record, record.len() * 8)?;

    let lc = ga(2, 3, 2);
    for end in 0..=lc.count {
        sample(&format!("fixed-bits-{end}"), &lc.data, end)?;
    }
    for size in 0..lc.data.len() {
        sample(
            &format!("physical-bytes-{size}"),
            &lc.data[..size],
            size * 8,
        )?;
    }
    let mut sync = ga(2, 6, 1);
    sync.fields(&[(0x2b7, 11), (5, 5), (1, 1), (3, 4), (0x548, 11), (1, 1)]);
    for end in [16, 24, 27, 31, 32, 36, 37, 38, 47, 48, 49, 56, 64] {
        sample(&format!("sync-extent-{end}"), &sync.data, end)?;
    }
    for (label, size) in [("shorter", 16), ("equal", 56), ("longer", 64)] {
        corpus.authored(
            View::Reference,
            &format!("asc-mov-{label}"),
            "asc-mov",
            &sync.data,
            &["asc-mov", &size.to_string()],
        )?;
    }
    let adts = derived::streams_matching(root, ".adts")?;
    for &(id, name, config) in AAC_ACCESS_UNITS {
        let input = adts
            .iter()
            .find(|stream| stream.name == name)
            .ok_or("missing AAC access-unit vector")?;
        corpus.fetched(
            View::Reference,
            id,
            "asc-frame",
            input,
            &["asc-frame", config],
        )?;
    }
    Ok(())
}

fn eac3(root: &Path, corpus: &mut Corpus) -> Result<()> {
    let mut baseline = None;
    for fixture in ["eac3-bsi-v1", "eac3-bsi-boundary-v1", "eac3-diagnosis-v2"] {
        let list = text(&root.join(format!("crates/framing/fixtures/{fixture}.tsv")))?;
        for line in list.lines().skip(1) {
            let columns: Vec<_> = line.split('\t').collect();
            let bytes = if columns[1] == "~" {
                Vec::new()
            } else {
                unhex(columns[1])?
            };
            if baseline.is_none() && fixture == "eac3-bsi-v1" {
                let end = columns[2]
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?;
                baseline = Some((bytes.clone(), end - 16));
            }
            corpus.authored(
                View::Eac3,
                &format!("{fixture}-{}", columns[0]),
                "authored",
                &bytes,
                &["32768", "4096"],
            )?;
        }
    }
    let (frame, bits) = baseline.ok_or("no E-AC-3 BSI boundary fixture")?;
    for (name, end, bytes) in [
        ("exact-bsi", bits, frame.len()),
        ("short-bsi", bits - 1, frame.len()),
        ("short-frame", bits, frame.len() - 1),
        ("zero-bsi", 0, frame.len()),
        ("zero-frame", bits, 0),
    ] {
        corpus.authored(
            View::Eac3,
            &format!("eac3-limit-{name}"),
            "authored",
            &frame,
            &[&end.to_string(), &bytes.to_string()],
        )?;
    }
    for size in 0..frame.len() {
        corpus.authored(
            View::Eac3,
            &format!("eac3-truncated-{size}"),
            "authored",
            &frame[..size],
            &["32768", "4096"],
        )?;
    }
    for stream in derived::streams_matching(root, ".eac3")? {
        let id = stream
            .name
            .trim_end_matches(".eac3")
            .replace(['.', '_'], "-");
        corpus.fetched(View::Eac3, &id, "fate", &stream, &["32768", "4096"])?;
        corpus.fetched(View::Reference, &id, "eac3", &stream, &["eac3"])?;
    }
    Ok(())
}

pub(super) fn ensure_fetched(root: &Path) -> Result<()> {
    let ffmpeg = root.join(".research/reference-decode/install/bin/ffmpeg");
    for stream in derived::streams_matching(root, ".eac3")?.into_iter().chain(
        derived::streams_matching(root, ".adts")?
            .into_iter()
            .filter(|stream| {
                AAC_ACCESS_UNITS
                    .iter()
                    .any(|(_, name, _)| *name == stream.name)
            }),
    ) {
        if sha256::file_hex(&root.join(&stream.path)).as_deref().ok()
            == Some(stream.sha256.as_str())
        {
            continue;
        }
        if !derived::make(root, &ffmpeg, &stream)? {
            return Err(format!("{} differs from its input pin", stream.name));
        }
    }
    Ok(())
}
