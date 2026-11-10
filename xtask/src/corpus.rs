//! Authored media corpora and their manifests.
//!
//! Every fixture is built from authored structures, our own synthetic codec
//! packets (one recorded reference trace) and zlib streams; nothing is
//! downloaded. Manifests record each file's SHA-256, so regenerating proves the
//! inputs unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ebml::{cat, element as e, float64, header, int8, uint as u, vint};
use crate::json::{self, Value};
use crate::pyrandom::PyRandom;
use crate::sha256;
use crate::zlib::{self, Strategy};

/// The recorded reference trace whose packets the fixtures reuse.
const SOURCE_TRACE: &str = "harness/fixtures/avc-aac-sub-reference.json";

const SEGMENT: u32 = 0x18538067;
const INFO: u32 = 0x1549A966;
const TRACKS: u32 = 0x1654AE6B;
const CLUSTER: u32 = 0x1F43B675;

/// Where generators read inputs (`root`) and write fixtures and manifests (`out`).
pub struct Paths {
    pub root: PathBuf,
    pub out: PathBuf,
}

impl Paths {
    fn fixtures(&self, dir: &str) -> Result<PathBuf, String> {
        let path = self.out.join(".research").join(dir);
        fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    fn manifest(&self, name: &str, value: &Value) -> Result<PathBuf, String> {
        let path = self.out.join("harness").join(name);
        write_json(&path, value)?;
        Ok(path)
    }
}

pub fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, json::to_python(value, Some(2)) + "\n")
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

/// Generate every corpus family; returns one summary line per family.
pub fn generate_all(paths: &Paths) -> Result<Vec<String>, String> {
    let source = Source::load(&paths.root)?;
    Ok(vec![
        base(paths)?,
        header_removal(paths, &source)?,
        zlib_matrix(paths, &source)?,
        seek(paths, &source)?,
        colour(paths, &source)?,
        catalogue(paths, &source)?,
        playback_capsules(paths)?,
        mp4_range(paths)?,
    ])
}

fn mp4_range(paths: &Paths) -> Result<String, String> {
    const CONTROL: &str = "regular-avc-aac.mp4";
    const VARIABLE: &str = "variable-aac-duration.mp4";
    const FRAGMENTED: &str = "fragmented-avc-aac.mp4";
    const RESCALED: &str = "fragmented-aac-media-clock.mp4";
    let output = paths.fixtures("mp4-range-fixtures")?;
    let seed = paths
        .root
        .join("crates/isobmff/tests/fixtures/encoded-edit-v2/original.mp4");
    let control = fs::read(&seed).map_err(|e| format!("{}: {e}", seed.display()))?;
    if sha256::hex(&control) != "7f2f92cb0e7a97920828fc9f79daeec2471a58d990572305d7aa08c30fb512fd" {
        return Err("MP4 range seed changed".into());
    }
    let mut variable = control.clone();
    // The AAC stts has separate runs for the second and last samples.
    // Move 256 ticks between them, preserving total duration and media bytes.
    variable[83_729..83_733].copy_from_slice(&1272_u32.to_be_bytes());
    variable[83_745..83_749].copy_from_slice(&272_u32.to_be_bytes());
    write(&output.join(CONTROL), &control)?;
    write(&output.join(VARIABLE), &variable)?;
    let seed = paths
        .root
        .join("crates/playback/tests/fixtures/mp4")
        .join(FRAGMENTED);
    let fragmented = fs::read(&seed).map_err(|e| format!("{}: {e}", seed.display()))?;
    if sha256::hex(&fragmented)
        != "a258b6b9f59997eff8fb806503d1ca42c70f9be2d3cb81aa60c80fc319be5fe7"
    {
        return Err("fragmented MP4 range seed changed".into());
    }
    let mut rescaled = fragmented.clone();
    // Use a 44.1 kHz mdhd clock with the original 48 kHz AAC configuration.
    rescaled[833..837].copy_from_slice(&44_100_u32.to_be_bytes());
    write(&output.join(FRAGMENTED), &fragmented)?;
    write(&output.join(RESCALED), &rescaled)?;
    let mut cases: Vec<_> = [
        (
            "regular-open",
            CONTROL,
            &control,
            None,
            237,
            81_478,
            994_214_068_017_485_484_u64,
        ),
        (
            "regular-seek",
            CONTROL,
            &control,
            Some("28000"),
            185,
            62_763,
            15_577_132_028_095_841_482,
        ),
        (
            "variable-open",
            VARIABLE,
            &variable,
            None,
            237,
            81_478,
            1_650_792_575_122_043_820,
        ),
        (
            "variable-seek",
            VARIABLE,
            &variable,
            Some("28000"),
            186,
            62_932,
            1_562_922_133_773_717_379,
        ),
        (
            "fragmented-open",
            FRAGMENTED,
            &fragmented,
            None,
            93,
            39_981,
            7_927_680_340_045_287_421,
        ),
        (
            "fragmented-before-sync",
            FRAGMENTED,
            &fragmented,
            Some("4095"),
            93,
            39_981,
            7_927_680_340_045_287_421,
        ),
        (
            "fragmented-seek",
            FRAGMENTED,
            &fragmented,
            Some("4096"),
            74,
            30_999,
            2_297_253_848_353_618_229,
        ),
        (
            "fragmented-rescaled-open",
            RESCALED,
            &rescaled,
            None,
            93,
            39_981,
            16_257_532_825_513_011_650,
        ),
        (
            "fragmented-rescaled-seek",
            RESCALED,
            &rescaled,
            Some("4096"),
            75,
            31_255,
            16_684_294_720_659_653_080,
        ),
    ]
    .into_iter()
    .map(|(id, file, data, seek, packets, bytes, digest)| {
        mp4_range_case(id, file, data, seek, (packets, bytes, digest))
    })
    .collect();
    cases.extend(mp4_range_entries(paths, &output)?);
    let opus_file = "avc-opus-edit.mp4";
    let boundary_file = "avc-opus-seek-boundary.mp4";
    let tail_file = "avc-opus-trimmed-end.mp4";
    let rescaled_file = "avc-opus-rescaled-edit.mp4";
    let crop_file = "avc-opus-cropped-edit.mp4";
    let seed = paths
        .root
        .join("crates/isobmff/tests/fixtures/range-encoded-edits")
        .join(opus_file);
    let opus = fs::read(&seed).map_err(|e| format!("{}: {e}", seed.display()))?;
    if sha256::hex(&opus) != "109d8d19991c550fc5381c26c8a7fe9f1f006fa400a80d84ea31675cc0e0c18a" {
        return Err("MP4 Opus seed changed".into());
    }
    let mut boundary = opus.clone();
    // At a two-second video landing this puts the second audio candidate
    // exactly on the receiver's earlier search timestamp.
    boundary[1512..1516].copy_from_slice(&528_i32.to_be_bytes());
    let mut tail = opus.clone();
    // The audio edit ends inside a coded packet, while the video continues.
    for offset in [1420, 1508] {
        tail[offset..offset + 4].copy_from_slice(&1003_u32.to_be_bytes());
    }
    let mut rescaled = tail.clone();
    // Double the Opus media clock and all media ticks, leaving dOps unchanged.
    for (offset, value) in [
        (1512, 672_u32),
        (1548, 96_000),
        (1552, 307_968),
        (1784, 2016),
        (1792, 1920),
        (1800, 672),
    ] {
        rescaled[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    let mut crop = opus.clone();
    // Crop 1.1 seconds from the front. Make the first duration run span 20
    // packets so the retained and original run boundaries differ.
    for (offset, value) in [
        (1420, 2050_u32),
        (1508, 2050),
        (1512, 52_800),
        (1552, 154_896),
        (1780, 20),
        (1788, 140),
    ] {
        crop[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    write(&output.join(opus_file), &opus)?;
    write(&output.join(boundary_file), &boundary)?;
    write(&output.join(tail_file), &tail)?;
    write(&output.join(rescaled_file), &rescaled)?;
    write(&output.join(crop_file), &crop)?;
    cases.extend([
        mp4_range_case(
            "opus-open",
            opus_file,
            &opus,
            None,
            (241, 65_677, 15_624_301_091_816_995_394),
        ),
        mp4_range_case(
            "opus-seek",
            opus_file,
            &opus,
            Some("32000"),
            (92, 25_673, 5_125_035_269_830_725_090),
        ),
        mp4_range_case(
            "opus-seek-edit-boundary",
            boundary_file,
            &boundary,
            Some("32000"),
            (91, 25_535, 8_414_310_448_937_091_467),
        ),
        mp4_range_case(
            "opus-trim-end",
            tail_file,
            &tail,
            None,
            (131, 50_269, 5_934_296_594_475_595_056),
        ),
        mp4_range_case(
            "opus-trim-rescaled",
            rescaled_file,
            &rescaled,
            None,
            (131, 50_269, 7_154_259_262_571_099_376),
        ),
        mp4_range_case(
            "opus-crop-open",
            crop_file,
            &crop,
            None,
            (233, 64_822, 8_468_210_823_864_242_940),
        ),
        mp4_range_case(
            "opus-crop-seek",
            crop_file,
            &crop,
            Some("32000"),
            (33, 17_176, 15_900_364_185_324_428_767),
        ),
    ]);
    let count = cases.len();
    paths.manifest(
        "features/mp4-range/corpus.json",
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(cases))]),
    )?;
    Ok(format!("MP4 range corpus: {count} playback comparisons"))
}

fn mp4_range_case(
    id: &str,
    file: &str,
    data: &[u8],
    seek: Option<&str>,
    (packets, bytes, digest): (u32, u32, u64),
) -> Value {
    authored(
        id,
        file,
        data,
        vec![
            ("arguments", Value::strings(seek)),
            (
                "expected",
                Value::object([
                    ("packets", Value::int(packets)),
                    ("bytes", Value::int(bytes)),
                    ("digest", Value::int(digest)),
                ]),
            ),
        ],
    )
}

fn mp4_range_entries(paths: &Paths, output: &Path) -> Result<Vec<Value>, String> {
    let seed = |name: &str, expected: &str| {
        let file = paths.root.join("crates/playback/tests/fixtures").join(name);
        let bytes = fs::read(&file).map_err(|error| format!("{}: {error}", file.display()))?;
        if sha256::hex(&bytes) != expected {
            return Err(format!("MP4 range seed changed: {name}"));
        }
        Ok(bytes)
    };
    let hevc = seed(
        "mp4/encoded-hevc-aac-front.mp4",
        "94113774ce6260e885c685713a1e4a35c76d997ba49c0d62a669396649d3d6f9",
    )?;
    let av1 = seed(
        "av1/six-frames.mp4",
        "76383e7d9d646a2873ed6e8af7e52b48903b7a9a1fc95f1d807575b90c66b9c6",
    )?;
    // Retag the sole video sample description in each pinned seed. The
    // decoder configuration and samples are the original authored bytes.
    let mut cases = Vec::new();
    for (tag, source, offset, original, seek, open_fold, seek_fold) in [
        (
            "dvh1",
            &hevc,
            421,
            b"hvc1",
            "40",
            (4, 3438, 265_368_919_262_385_015),
            (4, 3438, 265_368_919_262_385_015),
        ),
        (
            "dvhe",
            &hevc,
            421,
            b"hvc1",
            "40",
            (4, 3438, 6_289_861_727_786_914_679),
            (4, 3438, 6_289_861_727_786_914_679),
        ),
        (
            "dav1",
            &av1,
            461,
            b"av01",
            "16384",
            (6, 4097, 1_285_709_537_707_954_380),
            (3, 2192, 4_884_662_521_198_875_690),
        ),
    ] {
        let mut data = source.clone();
        let entry = data
            .get_mut(offset..offset + 4)
            .ok_or("missing MP4 range sample entry")?;
        if entry != original {
            return Err("unexpected MP4 range sample entry".into());
        }
        entry.copy_from_slice(tag.as_bytes());
        let file = format!("{tag}.mp4");
        write(&output.join(&file), &data)?;
        cases.push(mp4_range_case(
            &format!("{tag}-open"),
            &file,
            &data,
            None,
            open_fold,
        ));
        cases.push(mp4_range_case(
            &format!("{tag}-seek"),
            &file,
            &data,
            Some(seek),
            seek_fold,
        ));
    }
    Ok(cases)
}

/// One manifest row for an authored fixture.
fn authored(name: &str, file: &str, data: &[u8], extra: Vec<(&str, Value)>) -> Value {
    let mut row = vec![
        ("id", Value::str(name)),
        ("profile", Value::str(name)),
        ("file", Value::str(file)),
        ("sha256", Value::str(sha256::hex(data))),
        ("rights", Value::str("authored-synthetic")),
    ];
    row.extend(extra);
    Value::object(row)
}

fn hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd-length hex".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// Packets and codec configuration of the recorded reference trace.
pub struct Source {
    data: Value,
}

impl Source {
    pub fn load(root: &Path) -> Result<Self, String> {
        let trace = crate::checks::read_json(&root.join(SOURCE_TRACE))
            .map_err(|e| format!("{SOURCE_TRACE}: {e}"))?;
        let data = trace
            .get("data")
            .cloned()
            .ok_or("source trace has no data")?;
        Ok(Source { data })
    }

    fn list(&self, key: &str) -> &[Value] {
        self.data
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_default()
    }

    fn packet(&self, keep: impl Fn(&Value) -> bool) -> Result<Vec<u8>, String> {
        let packet = self
            .list("packets")
            .iter()
            .find(|p| keep(p))
            .ok_or("no matching source packet")?;
        hex(packet
            .get("payload")
            .and_then(Value::as_str)
            .ok_or("packet without payload")?)
    }

    fn track_field(&self, keep: impl Fn(&Value) -> bool, field: &str) -> Result<Vec<u8>, String> {
        let track = self
            .list("tracks")
            .iter()
            .find(|t| keep(t))
            .ok_or("no matching source track")?;
        hex(track
            .get(field)
            .and_then(Value::as_str)
            .ok_or("track without codec private data")?)
    }

    fn on_track(index: i64) -> impl Fn(&Value) -> bool {
        move |p| p.get("track").and_then(Value::as_f64) == Some(index as f64)
    }

    /// The first AAC packet.
    fn audio_packet(&self) -> Result<Vec<u8>, String> {
        self.packet(Self::on_track(1))
    }

    /// The first video packet, and the first video key frame.
    fn video_packet(&self, key: bool) -> Result<Vec<u8>, String> {
        let on_video = Self::on_track(0);
        self.packet(move |p| on_video(p) && (!key || p.get("key").is_some_and(Value::truthy)))
    }
}

// ---------------------------------------------------------------------------
// Base corpus: canonical encoded fixtures plus structural edge cases.

const STRUCTURAL: [&str; 8] = [
    "unknown-cluster",
    "xiph-lace",
    "fixed-lace",
    "ebml-lace",
    "negative-block-time",
    "blockgroup-reference",
    "codec-delay",
    "discard-padding",
];

pub fn structural(profile: &str) -> Vec<u8> {
    let mut track = cat([
        &u(0xD7, 1),
        &u(0x73C5, 1),
        &u(0x83, 2),
        &e(0x86, b"A_PCM/INT/LIT"),
    ]);
    track.extend(u(0x23E383, 1_000_000));
    track.extend(e(
        0xE1,
        &cat([
            &u(0x9F, 1),
            &u(0x6264, 16),
            &e(0xB5, &8000f64.to_be_bytes()),
        ]),
    ));
    if profile == "codec-delay" {
        track.extend(u(0x56AA, 2_000_000));
    }
    let tracks = e(TRACKS, &e(0xAE, &track));
    let flags = match profile {
        "xiph-lace" => 0x82,
        "fixed-lace" => 0x84,
        "ebml-lace" => 0x86,
        _ => 0x80,
    };
    let relative: i16 = if profile == "negative-block-time" {
        -4
    } else {
        0
    };
    let mut block = cat([b"\x81", &relative.to_be_bytes(), &[flags]]);
    match profile {
        "xiph-lace" => block.extend(cat([b"\x02\x02\x02", &[0, 1, 2, 3, 4, 5]])),
        "fixed-lace" => block.extend(cat([b"\x02", &[0, 1, 2, 3, 4, 5]])),
        "ebml-lace" => block.extend(cat([b"\x02\x82\xbf", &[0, 1, 2, 3, 4, 5]])),
        _ => block.extend(b"\x00\x01"),
    }
    let payload = if matches!(profile, "blockgroup-reference" | "discard-padding") {
        let mut group = cat([&e(0xA1, &block), &u(0x9B, 1)]);
        if profile == "blockgroup-reference" {
            group.extend(e(0xFB, b"\xff"));
        } else {
            group.extend(e(0x75A2, &1_000_000i32.to_be_bytes()));
        }
        cat([&u(0xE7, 0), &e(0xA0, &group)])
    } else {
        cat([&u(0xE7, 0), &e(0xA3, &block)])
    };
    let cluster = if profile == "unknown-cluster" {
        cat([
            b"\x1f\x43\xb6\x75\xff",
            &payload,
            &e(CLUSTER, &cat([&u(0xE7, 10), &e(0xA3, &block)])),
        ])
    } else {
        e(CLUSTER, &payload)
    };
    cat([
        &header(),
        b"\x18\x53\x80\x67\x01\xff\xff\xff\xff\xff\xff\xff",
        &e(INFO, &u(0x2AD7B1, 1_000_000)),
        &tracks,
        &cluster,
    ])
}

fn base(paths: &Paths) -> Result<String, String> {
    let output = paths.fixtures("fixtures")?;
    let seeds = paths.root.join("harness/fixtures/encoded");
    let canonical = crate::checks::read_json(&seeds.join("manifest.json"))?;
    let mut cases = Vec::new();
    for case in canonical
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let field = |key: &str| {
            case.get(key)
                .and_then(Value::as_str)
                .ok_or(format!("canonical case without {key}"))
        };
        let (id, name, sha) = (field("id")?, field("file")?, field("sha256")?);
        if name.contains('/') || name.contains('\\') || name == ".." || name.is_empty() {
            return Err("invalid canonical fixture name".into());
        }
        let seed = fs::read(seeds.join(name)).map_err(|e| format!("{name}: {e}"))?;
        if sha256::hex(&seed) != sha {
            return Err(format!("canonical fixture changed: {name}"));
        }
        let path = output.join(name);
        if path.exists() {
            let cached = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if sha256::hex(&cached) != sha {
                return Err(format!("cached fixture changed: {name}"));
            }
        } else {
            write(&path, &seed)?;
        }
        cases.push((id.to_owned(), name.to_owned()));
    }
    for profile in STRUCTURAL {
        let name = format!("{profile}.mkv");
        write(&output.join(&name), &structural(profile))?;
        cases.push((profile.to_owned(), name));
    }
    let mut rows = Vec::new();
    for (id, name) in &cases {
        let data = fs::read(output.join(name)).map_err(|e| format!("{name}: {e}"))?;
        rows.push(authored(id, name, &data, vec![]));
    }
    // The generator label is recorded corpus data and stays as first written.
    let manifest = Value::object([
        ("schema", Value::int(1)),
        (
            "status",
            Value::str("original synthetic corpus, not upstream FATE execution"),
        ),
        ("generator", Value::str("tools/generate-corpus.py")),
        ("cases", Value::Array(rows)),
    ]);
    let path = paths.manifest("features/matroska-demux/corpus.json", &manifest)?;
    Ok(format!(
        "Materialized {} cases: {}",
        cases.len(),
        path.display()
    ))
}

// ---------------------------------------------------------------------------
// Header removal (ContentCompAlgo 3).

fn audio_track(codec: &[u8], private: &[u8], rate: f64, encodings: &[u8]) -> Vec<u8> {
    cat([
        &u(0xD7, 1),
        &u(0x73C5, 1),
        &u(0x83, 2),
        &e(0x86, codec),
        &e(0x63A2, private),
        &u(0x23E383, 1_000_000),
        &e(
            0xE1,
            &cat([&u(0x9F, 1), &u(0x6264, 16), &float64(0xB5, rate)]),
        ),
        encodings,
    ])
}

fn single_block_file(track: &[u8], block: &[u8]) -> Vec<u8> {
    let segment = cat([
        &e(INFO, &u(0x2AD7B1, 1_000_000)),
        &e(TRACKS, &e(0xAE, track)),
        &e(CLUSTER, &cat([&u(0xE7, 0), &e(0xA3, block)])),
    ]);
    cat([&header(), &e(SEGMENT, &segment)])
}

pub fn header_removal_case(
    source: &Source,
    scope: u64,
    lace: bool,
    aac: bool,
) -> Result<Vec<u8>, String> {
    let stripped: &[u8] = if aac { b"\x11\x88" } else { b"\x55\xaa" };
    let private: &[u8] = if aac {
        b"\x56\xe5\x00"
    } else if scope & 2 != 0 {
        b"cd"
    } else {
        b""
    };
    let encoding = e(
        0x6D80,
        &e(
            0x6240,
            &cat([
                &u(0x5032, scope),
                &e(0x5034, &cat([&u(0x4254, 3), &e(0x4255, stripped)])),
            ]),
        ),
    );
    let (codec, rate): (&[u8], f64) = if aac {
        (b"A_AAC", 48000.0)
    } else {
        (b"A_PCM/INT/LIT", 8000.0)
    };
    let track = audio_track(codec, private, rate, &encoding);
    let block = if aac {
        cat([b"\x81\x00\x00\x80", &source.audio_packet()?])
    } else if lace {
        b"\x81\x00\x00\x84\x01\x00\x01\x02\x03".to_vec()
    } else {
        b"\x81\x00\x00\x80".to_vec()
    };
    Ok(single_block_file(&track, &block))
}

fn header_removal(paths: &Paths, source: &Source) -> Result<String, String> {
    let output = paths.fixtures("header-fixtures")?;
    let mut rows = Vec::new();
    for (name, scope, lace, aac) in [
        ("header-empty", 1, false, false),
        ("header-lace", 1, true, false),
        ("header-private", 2, false, true),
        ("header-both", 3, false, false),
    ] {
        let data = header_removal_case(source, scope, lace, aac)?;
        let file = format!("{name}.mkv");
        write(&output.join(&file), &data)?;
        rows.push(authored(name, &file, &data, vec![]));
    }
    let count = rows.len();
    paths.manifest(
        "features/matroska-header-removal/corpus.json",
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(rows))]),
    )?;
    Ok(format!("Generated {count} header-removal cases"))
}

// ---------------------------------------------------------------------------
// zlib content compression (ContentCompAlgo 0).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Stored,
    Fixed,
    Dynamic,
}

#[derive(Clone, Copy, Debug)]
pub struct ZlibCase {
    pub kind: Kind,
    pub scope: u64,
    /// 0 none, 1 Xiph, 2 fixed-size, 3 EBML lacing.
    pub lace: u8,
    pub aac: bool,
    pub mixed: bool,
    pub empty: bool,
}

impl Default for ZlibCase {
    fn default() -> Self {
        ZlibCase {
            kind: Kind::Dynamic,
            scope: 1,
            lace: 0,
            aac: false,
            mixed: false,
            empty: false,
        }
    }
}

fn deflate(data: &[u8], kind: Kind) -> Result<Vec<u8>, String> {
    match kind {
        Kind::Stored => zlib::compress(data, 0, Strategy::Default),
        Kind::Fixed => zlib::compress(data, 6, Strategy::Fixed),
        Kind::Dynamic => zlib::compress(data, 6, Strategy::Default),
    }
}

pub fn zlib_case(source: &Source, c: ZlibCase) -> Result<Vec<u8>, String> {
    let mut payload = if c.empty {
        Vec::new()
    } else if c.kind == Kind::Stored {
        let mut rng = PyRandom::new(1951);
        (0..32_768).map(|_| rng.byte()).collect()
    } else {
        let mut unit =
            b"bounded zlib track, signed timestamps, cues and decoder preroll. ".to_vec();
        unit.extend(0..32u8);
        unit.repeat(256)
    };
    if c.aac {
        payload = source.audio_packet()?;
    }
    let mut private: Vec<u8> = if c.aac {
        b"\x11\x88\x56\xe5\x00".to_vec()
    } else {
        b"abcd".to_vec()
    };
    let stripped: Vec<u8> = if c.mixed {
        payload[..4.min(payload.len())].to_vec()
    } else {
        Vec::new()
    };
    let encoded = if c.scope & 1 != 0 {
        deflate(&payload[stripped.len()..], c.kind)?
    } else {
        payload.clone()
    };
    if c.scope & 2 != 0 {
        private = deflate(&private, c.kind)?;
    }
    let mut entries = e(
        0x6240,
        &cat([
            &u(0x5031, u64::from(c.mixed)),
            &u(0x5032, c.scope),
            &e(0x5034, &u(0x4254, 0)),
        ]),
    );
    if c.mixed {
        entries.extend(e(
            0x6240,
            &cat([
                &u(0x5031, 0),
                &u(0x5032, 1),
                &e(0x5034, &cat([&u(0x4254, 3), &e(0x4255, &stripped)])),
            ]),
        ));
    }
    let (codec, rate): (&[u8], f64) = if c.aac {
        (b"A_AAC", 48000.0)
    } else {
        (b"A_PCM/INT/LIT", 8000.0)
    };
    let track = audio_track(codec, &private, rate, &e(0x6D80, &entries));
    // Two equal-size frames, testing each legal lacing scheme independently.
    let twice = encoded.repeat(2);
    let body = match c.lace {
        1 => {
            let size = encoded.len();
            let mut sizes = vec![255u8; size / 255];
            sizes.push((size % 255) as u8);
            cat([b"\x01", &sizes, &twice])
        }
        2 => cat([b"\x01", &twice]),
        3 => cat([b"\x01", &vint(encoded.len() as u64, None), &twice]),
        _ => encoded,
    };
    let block = cat([b"\x81\x00\x00", &[0x80 | (c.lace * 2)], &body]);
    Ok(single_block_file(&track, &block))
}

fn delivery_contract(id: &str, operation: &str, contract: &[&str], profiles: Vec<Value>) -> Value {
    Value::object([
        ("schema", Value::int(1)),
        ("id", Value::str(id)),
        ("operation", Value::str(operation)),
        ("contract", Value::strings(contract.iter().copied())),
        ("required_profiles", Value::Array(profiles)),
        (
            "required_targets",
            Value::strings(["aarch64-unknown-linux-gnu"]),
        ),
        ("minimum_pairs", Value::int(30)),
        ("confidence", Value::float(0.95)),
        (
            "pending_module_requirements",
            Value::strings([
                "remaining container features",
                "complete applicable FATE coverage",
                "deferred targets",
            ]),
        ),
    ])
}

fn profiles_of(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .filter_map(|r| r.get("profile").cloned())
        .collect()
}

fn zlib_matrix(paths: &Paths, source: &Source) -> Result<String, String> {
    let output = paths.fixtures("zlib-fixtures")?;
    let d = ZlibCase::default();
    let mut matrix = vec![
        (
            "zlib-stored".to_owned(),
            ZlibCase {
                kind: Kind::Stored,
                ..d
            },
        ),
        (
            "zlib-fixed".to_owned(),
            ZlibCase {
                kind: Kind::Fixed,
                ..d
            },
        ),
        ("zlib-dynamic".to_owned(), d),
        ("zlib-empty".to_owned(), ZlibCase { empty: true, ..d }),
        (
            "zlib-private".to_owned(),
            ZlibCase {
                scope: 2,
                aac: true,
                ..d
            },
        ),
        ("zlib-both".to_owned(), ZlibCase { scope: 3, ..d }),
        ("zlib-mixed".to_owned(), ZlibCase { mixed: true, ..d }),
    ];
    for (name, lace) in [("xiph", 1), ("fixed-lace", 2), ("ebml", 3)] {
        matrix.push((format!("zlib-{name}"), ZlibCase { lace, ..d }));
    }
    let mut rows = Vec::new();
    for (name, case) in matrix {
        let data = zlib_case(source, case)?;
        let file = format!("{name}.mkv");
        write(&output.join(&file), &data)?;
        rows.push(authored(&name, &file, &data, vec![]));
    }
    let contract = delivery_contract(
        "matroska-zlib",
        "container-demux-v1",
        &[
            "RFC 1950 checksum and wrapper",
            "RFC 1951 stored/fixed/dynamic blocks",
            "ContentEncoding scopes 1/2/3",
            "all lacing schemes",
            "ordered mixed header/zlib encodings",
            "exact source and FFmpeg packet times",
            "bounded decoded output, expansion ratio, work and cancellation",
        ],
        profiles_of(&rows),
    );
    let count = rows.len();
    paths.manifest(
        "features/matroska-zlib/corpus.json",
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(rows))]),
    )?;
    paths.manifest("features/matroska-zlib/contract.json", &contract)?;
    Ok(format!("Generated {count} zlib cases"))
}

// ---------------------------------------------------------------------------
// Cues, SeekHead and preroll.

#[derive(Clone, Copy, Default)]
struct SeekCase {
    preroll: u64,
    delay: u64,
    relative: bool,
    missing: bool,
    sparse: bool,
    unknown: bool,
    front: bool,
}

fn seek_case(source: &Source, c: SeekCase) -> Result<Vec<u8>, String> {
    let packet = source.video_packet(true)?;
    let private = source.track_field(|_| true, "private")?;
    let video = cat([
        &u(0xD7, 1),
        &u(0x73C5, 1),
        &u(0x83, 1),
        &e(0x86, b"V_MPEG4/ISO/AVC"),
        &e(0x63A2, &private),
        &u(0x23E383, 500_000_000),
        &e(0xE0, &cat([&u(0xB0, 160), &u(0xBA, 90)])),
    ]);
    let audio = cat([
        &u(0xD7, 2),
        &u(0x73C5, 2),
        &u(0x83, 2),
        &e(0x86, b"A_PCM/INT/LIT"),
        &u(0x23E383, 250_000_000),
        &u(0x56BB, c.preroll),
        &u(0x56AA, c.delay),
        &e(
            0xE1,
            &cat([&u(0x9F, 1), &u(0x6264, 16), &float64(0xB5, 8000.0)]),
        ),
    ]);
    let tracks = e(TRACKS, &cat([&e(0xAE, &video), &e(0xAE, &audio)]));
    let info = e(INFO, &u(0x2AD7B1, 1_000_000));
    let block = |track: u8, offset: i16, payload: &[u8], key: bool| {
        e(
            0xA3,
            &cat([
                &[0x80 + track],
                &offset.to_be_bytes(),
                &[if key { 0x80 } else { 0 }],
                payload,
            ]),
        )
    };
    let pcm: Vec<u8> = (0..32).collect();
    let mut clusters = Vec::new();
    let mut relative_positions = Vec::new();
    for time in (0..16_000).step_by(1000) {
        let timestamp = u(0xE7, time);
        let audio_first = block(2, 0, &pcm, true);
        let body = cat([
            &timestamp,
            &audio_first,
            &block(1, 0, &packet, true),
            &block(2, 250, &pcm, true),
            &block(1, 500, &packet, false),
            &block(2, 500, &pcm, true),
            &block(2, 750, &pcm, true),
        ]);
        relative_positions.push((timestamp.len() + audio_first.len()) as u64);
        clusters.push(if c.unknown {
            cat([b"\x1f\x43\xb6\x75\xff", &body])
        } else {
            e(CLUSTER, &body)
        });
    }
    let head = |position: u64| {
        e(
            0x114D9B74,
            &e(
                0x4DBB,
                &cat([
                    &e(0x53AB, b"\x1c\x53\xbb\x6b"),
                    &e(0x53AC, &position.to_be_bytes()),
                ]),
            ),
        )
    };
    let mut metadata = cat([&info, &tracks]);
    if !(c.missing || c.front) {
        metadata.extend(head(0));
    }
    let cues = |offset: u64| {
        let mut entries = Vec::new();
        let mut position = offset;
        for (i, cluster) in clusters.iter().enumerate() {
            if !c.sparse || i % 4 == 0 {
                let mut fields = cat([&u(0xF7, 1), &e(0xF1, &position.to_be_bytes())]);
                if c.relative {
                    fields.extend(cat([&u(0xF0, relative_positions[i]), &u(0x5378, 2)]));
                }
                entries.extend(e(
                    0xBB,
                    &cat([&u(0xB3, i as u64 * 1000), &e(0xB7, &fields)]),
                ));
            }
            position += cluster.len() as u64;
        }
        e(0x1C53BB6B, &entries)
    };
    let all_clusters = clusters.concat();
    let body = if c.missing {
        cat([&metadata, &all_clusters])
    } else if c.front {
        let first = cues(metadata.len() as u64);
        let index = cues((metadata.len() + first.len()) as u64);
        if first.len() != index.len() {
            return Err("front Cues size depends on its own offset".into());
        }
        cat([&metadata, &index, &all_clusters])
    } else {
        let end = (metadata.len() + all_clusters.len()) as u64;
        cat([
            &info,
            &tracks,
            &head(end),
            &all_clusters,
            &cues(metadata.len() as u64),
        ])
    };
    Ok(cat([&header(), &e(SEGMENT, &body)]))
}

fn seek(paths: &Paths, source: &Source) -> Result<String, String> {
    let output = paths.fixtures("seek-fixtures")?;
    let d = SeekCase::default();
    let long = 3_000_000_000;
    let cases = [
        ("cues-end", d),
        (
            "cues-relative",
            SeekCase {
                relative: true,
                ..d
            },
        ),
        ("cues-front", SeekCase { front: true, ..d }),
        ("cues-preroll", SeekCase { preroll: long, ..d }),
        (
            "cues-delay-preroll",
            SeekCase {
                preroll: 80_000_000,
                delay: 50_000_000,
                ..d
            },
        ),
        (
            "cues-sparse",
            SeekCase {
                sparse: true,
                preroll: long,
                ..d
            },
        ),
        (
            "cues-unknown-cluster",
            SeekCase {
                unknown: true,
                preroll: long,
                ..d
            },
        ),
        (
            "cues-missing",
            SeekCase {
                missing: true,
                preroll: long,
                ..d
            },
        ),
    ];
    let mut rows = Vec::new();
    for (name, case) in cases {
        let data = seek_case(source, case)?;
        let file = format!("{name}.mkv");
        write(&output.join(&file), &data)?;
        rows.push(authored(
            name,
            &file,
            &data,
            vec![("seek_target_ns", Value::int(7_500_000_000i64))],
        ));
    }
    let contract = delivery_contract(
        "matroska-cues-preroll",
        "container-seek-preroll-v1",
        &[
            "bounded Cues/SeekHead lookup",
            "validated CueRelativePosition and CueBlockNumber",
            "sparse and absent index fallback",
            "all-track SeekPreRoll and codec delay",
            "all packets after seek including decode-only preroll",
            "fresh, repeated and after-EOF seeks",
            "unknown Cluster lengths",
            "seek cancellation, I/O failures and malformed indexes",
        ],
        profiles_of(&rows),
    );
    let count = rows.len();
    paths.manifest(
        "features/matroska-cues-preroll/corpus.json",
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(rows))]),
    )?;
    paths.manifest("features/matroska-cues-preroll/contract.json", &contract)?;
    Ok(format!("Generated {count} seek cases"))
}

// ---------------------------------------------------------------------------
// Colour and mastering metadata.

/// Ordered Colour fields; a later entry with the same identifier replaces the earlier value in place.
type Fields = Vec<(u32, u64)>;

fn with(base: &Fields, extra: &[(u32, u64)]) -> Fields {
    let mut out = base.clone();
    for &(id, value) in extra {
        match out.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = value,
            None => out.push((id, value)),
        }
    }
    out
}

const MASTERING: [f64; 10] = [
    0.68, 0.32, 0.265, 0.69, 0.15, 0.06, 0.3127, 0.329, 1000.0, 0.005,
];

/// MasteringMetadata with 4- or 8-byte floats; `partial` keeps only the first
/// primary and the maximum luminance.
pub fn mastering(width: usize, partial: bool, values: &[f64]) -> Vec<u8> {
    let mut body = Vec::new();
    for (i, &value) in values.iter().enumerate() {
        if partial && i != 0 && i != 8 {
            continue;
        }
        let bytes = if width == 4 {
            (value as f32).to_be_bytes().to_vec()
        } else {
            value.to_be_bytes().to_vec()
        };
        body.extend(e(0x55D1 + i as u32, &bytes));
    }
    e(0x55D0, &body)
}

type ColourProfile = (String, Option<Fields>, Vec<u8>, u64);

fn colour_profiles() -> Vec<ColourProfile> {
    let pq: Fields = vec![
        (0x55B1, 9),
        (0x55B2, 10),
        (0x55B9, 1),
        (0x55BA, 16),
        (0x55BB, 9),
    ];
    let master = mastering(8, false, &MASTERING);
    let row = |name: &str, fields: Option<Fields>, extra: Vec<u8>, tracks: u64| {
        (name.to_owned(), fields, extra, tracks)
    };
    let mut rows = vec![
        row("colour-absent", None, vec![], 1),
        row("colour-empty", Some(vec![]), vec![], 1),
        row(
            "colour-sdr",
            Some(vec![(0x55B1, 1), (0x55B9, 1), (0x55BA, 1), (0x55BB, 1)]),
            vec![],
            1,
        ),
        row(
            "colour-rgb-full",
            Some(vec![
                (0x55B1, 0),
                (0x55B2, 8),
                (0x55B9, 2),
                (0x55BA, 13),
                (0x55BB, 1),
            ]),
            vec![],
            1,
        ),
        row(
            "colour-hdr10",
            Some(with(&pq, &[(0x55BC, 1000), (0x55BD, 400)])),
            master.clone(),
            1,
        ),
        row(
            "colour-hdr32",
            Some(pq.clone()),
            mastering(4, false, &MASTERING),
            1,
        ),
        row("colour-hlg", Some(with(&pq, &[(0x55BA, 18)])), vec![], 1),
        row(
            "colour-p3",
            Some(with(&pq, &[(0x55BB, 12)])),
            master.clone(),
            1,
        ),
        row("colour-cll-only", Some(vec![(0x55BC, 1000)]), vec![], 1),
        row("colour-fall-only", Some(vec![(0x55BD, 400)]), vec![], 1),
        row(
            "colour-light-zero",
            Some(vec![(0x55BC, 0), (0x55BD, 0)]),
            vec![],
            1,
        ),
        row("colour-master-empty", Some(vec![]), e(0x55D0, b""), 1),
        row(
            "colour-master-partial",
            Some(pq.clone()),
            mastering(8, true, &MASTERING),
            1,
        ),
        row(
            "colour-master-zero",
            Some(vec![]),
            mastering(8, false, &[0.0; 10]),
            1,
        ),
        row(
            "colour-master-boundary",
            Some(pq.clone()),
            mastering(
                8,
                false,
                &[1.0, 0.0, 0.5, 0.25, 0.125, 0.0625, 0.5, 0.5, 10000.0, 0.0],
            ),
            1,
        ),
        row(
            "colour-subsampling",
            Some(vec![
                (0x55B2, 12),
                (0x55B3, 1),
                (0x55B4, 1),
                (0x55B5, 0),
                (0x55B6, 0),
            ]),
            vec![],
            1,
        ),
        row(
            "colour-many-tracks",
            Some(with(&pq, &[(0x55BC, 4000), (0x55BD, 600)])),
            master,
            64,
        ),
    ];
    for h in 0..3 {
        for v in 0..3 {
            rows.push(row(
                &format!("colour-siting-{h}-{v}"),
                Some(vec![(0x55B7, h), (0x55B8, v)]),
                vec![],
                1,
            ));
        }
    }
    let half_minimum = 0.5 / f64::from(i32::MAX);
    let repeat = |value: f64, luminance: [f64; 2]| {
        let mut values = vec![value; 8];
        values.extend(luminance);
        values
    };
    rows.extend([
        row(
            "colour-empty-elements",
            Some(vec![]),
            cat([
                &e(0x55B1, b""),
                &e(0x55BA, b""),
                &e(0x55BB, b""),
                &e(0x55D0, &e(0x55D9, b"")),
            ]),
            1,
        ),
        row(
            "colour-registry-codes",
            Some(vec![
                (0x55B1, 255),
                (0x55BA, 255),
                (0x55BB, 255),
                (0x55B9, 3),
            ]),
            vec![],
            1,
        ),
        row(
            "colour-luminance-only",
            Some(vec![]),
            e(0x55D0, &cat([&float64(0x55D9, 1000.0), &e(0x55DA, b"")])),
            1,
        ),
        row(
            "colour-primaries-only",
            Some(vec![]),
            e(
                0x55D0,
                &(0..8)
                    .flat_map(|i| float64(0x55D1 + i, 0.5))
                    .collect::<Vec<_>>(),
            ),
            1,
        ),
        row(
            "colour-master-ones",
            Some(vec![]),
            mastering(8, false, &repeat(1.0, [1000.0, 0.0])),
            1,
        ),
        row(
            "colour-master-tiny",
            Some(vec![]),
            mastering(8, false, &repeat(1e-20, [1000.0, 1e-20])),
            1,
        ),
        row(
            "colour-master-half-minimum",
            Some(vec![]),
            mastering(8, false, &repeat(half_minimum, [1000.0, half_minimum])),
            1,
        ),
    ]);
    rows
}

pub fn colour_case(
    source: &Source,
    fields: Option<&Fields>,
    extra: &[u8],
    tracks: u64,
) -> Result<Vec<u8>, String> {
    let private = source.track_field(
        |t| t.get("kind").and_then(Value::as_f64) == Some(1.0),
        "private",
    )?;
    let packet = source.video_packet(false)?;
    let colour = match fields {
        None => Vec::new(),
        Some(fields) => {
            let mut body: Vec<u8> = fields.iter().flat_map(|&(k, v)| u(k, v)).collect();
            body.extend_from_slice(extra);
            e(0x55B0, &body)
        }
    };
    let mut entries = Vec::new();
    let mut blocks = Vec::new();
    for n in 1..=tracks {
        let track = cat([
            &u(0xD7, n),
            &u(0x73C5, n),
            &u(0x83, 1),
            &e(0x86, b"V_MPEG4/ISO/AVC"),
            &e(0x63A2, &private),
            &e(0x22B59C, b"eng"),
            &u(0x23E383, 40_000_000),
            &e(0xE0, &cat([&u(0xB0, 128), &u(0xBA, 72), &colour])),
        ]);
        entries.extend(e(0xAE, &track));
        for at in 0..4i16 {
            blocks.extend(e(
                0xA3,
                &cat([&vint(n, None), &(at * 40).to_be_bytes(), b"\x80", &packet]),
            ));
        }
    }
    let segment = cat([
        &e(INFO, &u(0x2AD7B1, 1_000_000)),
        &e(TRACKS, &entries),
        &e(CLUSTER, &cat([&u(0xE7, 0), &blocks])),
    ]);
    Ok(cat([&header(), &e(SEGMENT, &segment)]))
}

fn colour(paths: &Paths, source: &Source) -> Result<String, String> {
    let output = paths.fixtures("colour-fixtures")?;
    let mut rows = Vec::new();
    for (name, fields, extra, tracks) in colour_profiles() {
        let data = colour_case(source, fields.as_ref(), &extra, tracks)?;
        let file = format!("{name}.mkv");
        write(&output.join(&file), &data)?;
        rows.push(authored(&name, &file, &data, vec![]));
    }
    let count = rows.len();
    paths.manifest(
        "features/matroska-colour/corpus.json",
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(rows))]),
    )?;
    Ok(format!("Generated {count} colour/HDR profiles"))
}

// ---------------------------------------------------------------------------
// Chapters, tags, attachments and segment information.

fn display(name: &[u8], language: &[u8], ietf: Option<&[u8]>) -> Vec<u8> {
    let mut body = cat([&e(0x85, name), &e(0x437C, language)]);
    if let Some(ietf) = ietf {
        body.extend(e(0x437D, ietf));
    }
    e(0x80, &body)
}

fn eng(name: &[u8]) -> Vec<u8> {
    display(name, b"eng", None)
}

fn chapter(uid: u64, start: u64, end: Option<u64>, extra: &[u8]) -> Vec<u8> {
    let mut body = cat([&u(0x73C4, uid), &u(0x91, start)]);
    if let Some(end) = end {
        body.extend(u(0x92, end));
    }
    body.extend_from_slice(extra);
    e(0xB6, &body)
}

const SECOND: u64 = 1_000_000_000;

fn chapters(payload: Option<&[u8]>, ordered: bool) -> Vec<u8> {
    let default = chapter(1, 0, Some(SECOND), &eng(b"Opening"));
    let edition = cat([
        &u(0x45BC, 1),
        &u(0x45DB, 1),
        &u(0x45DD, u64::from(ordered)),
        payload.unwrap_or(&default),
    ]);
    e(0x1043A770, &e(0x45B9, &edition))
}

fn attachment(uid: u64, data: &[u8], name: &[u8], mime: &[u8]) -> Vec<u8> {
    e(
        0x61A7,
        &cat([
            &u(0x46AE, uid),
            &e(0x466E, name),
            &e(0x4660, mime),
            &e(0x467E, b"Authored fixture"),
            &e(0x465C, data),
        ]),
    )
}

fn font(data: &[u8]) -> Vec<u8> {
    e(
        0x1941A469,
        &attachment(1, data, b"demo.ttf", b"application/x-truetype-font"),
    )
}

fn simple(name: &[u8], value: Option<&[u8]>, extra: &[u8], binary: bool) -> Vec<u8> {
    let mut body = e(0x45A3, name);
    if let Some(value) = value {
        body.extend(e(if binary { 0x4485 } else { 0x4487 }, value));
    }
    body.extend_from_slice(extra);
    e(0x67C8, &body)
}

fn text(name: &[u8], value: &[u8]) -> Vec<u8> {
    simple(name, Some(value), b"", false)
}

fn tags(payload: Option<&[u8]>, targets: &[u8]) -> Vec<u8> {
    let default = text(b"TITLE", b"Test");
    e(
        0x1254C367,
        &e(
            0x7373,
            &cat([&e(0x63C0, targets), payload.unwrap_or(&default)]),
        ),
    )
}

struct CatalogueProfile {
    name: String,
    metadata: Vec<Vec<u8>>,
    extra: Vec<u8>,
    late: bool,
    info: Vec<u8>,
}

fn png() -> Result<Vec<u8>, String> {
    let chunk = |kind: &[u8], data: &[u8]| {
        let body = cat([kind, data]);
        cat([
            &(data.len() as u32).to_be_bytes(),
            &body,
            &zlib::crc32(&body).to_be_bytes(),
        ])
    };
    let ihdr = cat([&1u32.to_be_bytes(), &1u32.to_be_bytes(), &[8, 6, 0, 0, 0]]);
    let idat = zlib::compress(b"\0\xff\0\0\xff", 6, Strategy::Default)?;
    Ok(cat([
        b"\x89PNG\r\n\x1a\n",
        &chunk(b"IHDR", &ihdr),
        &chunk(b"IDAT", &idat),
        &chunk(b"IEND", b""),
    ]))
}

fn catalogue_profiles() -> Result<Vec<CatalogueProfile>, String> {
    let two = cat([
        &chapter(1, 0, Some(SECOND), &eng(b"Opening")),
        &chapter(2, SECOND, Some(2 * SECOND), &eng(b"Finish")),
    ]);
    let nested = chapter(
        1,
        0,
        None,
        &cat([&eng(b"Part"), &chapter(2, 0, Some(SECOND), &eng(b"Scene"))]),
    );
    let mut chain = chapter(16, 0, Some(SECOND), &eng(b"Leaf"));
    for uid in (1..=15).rev() {
        chain = chapter(uid, 0, None, &chain);
    }
    let attach = || font(b"font-data");
    let large: Vec<u8> = (0..=255u8).collect::<Vec<_>>().repeat(4096);
    let p = |name: &str, metadata: Vec<Vec<u8>>, extra: Vec<u8>, late: bool| CatalogueProfile {
        name: name.to_owned(),
        metadata,
        extra,
        late,
        info: Vec::new(),
    };
    let none = Vec::new;
    let french = "Français".as_bytes();
    let mut rows = vec![
        p("catalogue-empty", vec![], none(), false),
        p(
            "chapters-simple",
            vec![chapters(Some(&two), false)],
            none(),
            false,
        ),
        p(
            "chapters-nested",
            vec![chapters(Some(&nested), false)],
            none(),
            false,
        ),
        p(
            "chapters-multilingual",
            vec![chapters(
                Some(&chapter(
                    1,
                    0,
                    Some(SECOND),
                    &cat([&eng(b"English"), &display(french, b"fre", Some(b"fr-CA"))]),
                )),
                false,
            )],
            none(),
            false,
        ),
        p(
            "chapters-ordered",
            vec![chapters(Some(&two), true)],
            none(),
            false,
        ),
        p(
            "chapters-ordered-parent",
            vec![chapters(Some(&nested), true)],
            none(),
            false,
        ),
        p(
            "chapters-editions",
            vec![e(
                0x1043A770,
                &cat([
                    &e(
                        0x45B9,
                        &cat([&u(0x45BC, 1), &chapter(1, 0, Some(SECOND), &eng(b"A"))]),
                    ),
                    &e(
                        0x45B9,
                        &cat([
                            &u(0x45BC, 2),
                            &u(0x45DB, 1),
                            &chapter(2, 0, Some(SECOND), &eng(b"B")),
                        ]),
                    ),
                ]),
            )],
            none(),
            false,
        ),
        p(
            "chapters-missing-end",
            vec![chapters(
                Some(&cat([
                    &chapter(1, 0, None, &eng(b"First")),
                    &chapter(2, SECOND, None, &eng(b"Last")),
                ])),
                false,
            )],
            none(),
            false,
        ),
        p(
            "chapters-empty-display",
            vec![chapters(
                Some(&chapter(1, 0, Some(SECOND), &eng(b""))),
                false,
            )],
            none(),
            false,
        ),
        p(
            "chapters-many",
            vec![chapters(
                Some(
                    &(0..512u64)
                        .flat_map(|n| {
                            chapter(
                                n + 1,
                                n * 1000,
                                Some((n + 1) * 1000),
                                &eng(n.to_string().as_bytes()),
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
                false,
            )],
            none(),
            false,
        ),
        p(
            "chapters-depth",
            vec![chapters(Some(&chain), false)],
            none(),
            false,
        ),
        p(
            "tags-global",
            vec![tags(
                Some(&cat([
                    &text(b"TITLE", b"Example"),
                    &text(b"ARTIST", b"Creator"),
                ])),
                b"",
            )],
            none(),
            false,
        ),
        p("tags-track", vec![tags(None, &u(0x63C5, 1))], none(), false),
        p(
            "tags-chapter",
            vec![chapters(None, false), tags(None, &u(0x63C4, 1))],
            none(),
            false,
        ),
        p(
            "tags-attachment",
            vec![attach(), tags(None, &u(0x63C6, 1))],
            none(),
            false,
        ),
        p(
            "tags-edition",
            vec![chapters(None, false), tags(None, &u(0x63C9, 1))],
            none(),
            false,
        ),
        p(
            "tags-nested",
            vec![tags(
                Some(&simple(b"GROUP", None, &text(b"NAME", b"Nested"), false)),
                b"",
            )],
            none(),
            false,
        ),
        p(
            "tags-multilingual",
            vec![tags(
                Some(&cat([
                    &simple(b"TITLE", Some(b"English"), &e(0x447A, b"eng"), false),
                    &simple(
                        b"TITLE",
                        Some(french),
                        &cat([&e(0x447A, b"fre"), &e(0x447B, b"fr-CA"), &u(0x4484, 0)]),
                        false,
                    ),
                ])),
                b"",
            )],
            none(),
            false,
        ),
        p(
            "tags-binary",
            vec![tags(
                Some(&simple(
                    b"BINARY",
                    Some(&(0..=255u8).collect::<Vec<_>>()),
                    b"",
                    true,
                )),
                b"",
            )],
            none(),
            false,
        ),
        p(
            "tags-repeated",
            vec![tags(
                Some(&cat([&text(b"TITLE", b"First"), &text(b"TITLE", b"Last")])),
                b"",
            )],
            none(),
            false,
        ),
        p(
            "tags-future",
            vec![tags(
                Some(&cat([
                    &text(b"X_NEW_FIELD", b"opaque"),
                    &text(b"EMPTY", b""),
                ])),
                b"",
            )],
            none(),
            false,
        ),
        p("attachments-empty", vec![font(b"")], none(), false),
        p("attachments-font", vec![attach()], none(), false),
        p(
            "attachments-unknown",
            vec![e(
                0x1941A469,
                &attachment(1, b"font-data", b"note.bin", b"application/octet-stream"),
            )],
            none(),
            false,
        ),
        p("attachments-large", vec![font(&large)], none(), false),
        p(
            "attachments-many",
            vec![e(
                0x1941A469,
                &(0..32u64)
                    .flat_map(|n| {
                        attachment(
                            n + 1,
                            b"font-data",
                            format!("font{n}.ttf").as_bytes(),
                            b"application/x-truetype-font",
                        )
                    })
                    .collect::<Vec<_>>(),
            )],
            none(),
            false,
        ),
        p(
            "metadata-combined",
            vec![
                chapters(Some(&two), false),
                attach(),
                tags(Some(&text(b"TITLE", b"Movie")), b""),
                tags(Some(&text(b"TITLE", b"Track")), &u(0x63C5, 1)),
            ],
            cat([&e(0x22B59D, b"en-GB"), &u(0x55AB, 1)]),
            false,
        ),
        p(
            "metadata-late",
            vec![chapters(Some(&two), false), attach(), tags(None, b"")],
            none(),
            true,
        ),
        p(
            "track-flags",
            vec![],
            cat([
                &u(0xB9, 1),
                &u(0x88, 0),
                &u(0x55AA, 1),
                &u(0x55AB, 1),
                &u(0x55AC, 1),
                &u(0x55AD, 1),
                &u(0x55AE, 1),
                &u(0x55AF, 1),
            ]),
            false,
        ),
        p("track-disabled", vec![], u(0xB9, 0), false),
        p(
            "track-ietf-before",
            vec![],
            cat([&e(0x22B59D, b"fr-CA"), &e(0x22B59C, b"fre")]),
            false,
        ),
        p(
            "track-ietf-after",
            vec![],
            cat([&e(0x22B59C, b"fre"), &e(0x22B59D, b"fr-CA")]),
            false,
        ),
    ];
    let image = e(
        0x1941A469,
        &attachment(1, &png()?, b"cover.png", b"image/png"),
    );
    rows.push(p("attachments-image", vec![image], none(), false));
    rows.push(p(
        "attachments-large-late",
        vec![font(&large)],
        none(),
        true,
    ));
    let mut info = |name: &str, info: Vec<u8>| {
        rows.push(CatalogueProfile {
            name: name.to_owned(),
            metadata: vec![],
            extra: vec![],
            late: false,
            info,
        });
    };
    info(
        "info-title",
        cat([
            &e(0x7BA9, b"Authored title"),
            &e(0x4D80, b"Muxer"),
            &e(0x5741, b"Writer"),
        ]),
    );
    for (name, date) in [
        ("epoch", 0),
        ("negative", -1001),
        ("positive", 1001),
        ("leap", 730_944_000_123_456_789),
        ("earliest", i64::MIN),
        ("latest", i64::MAX),
    ] {
        info(&format!("info-date-{name}"), int8(0x4461, date));
    }
    rows.extend([
        p(
            "tags-multi-target",
            vec![
                chapters(Some(&two), false),
                tags(
                    Some(&text(b"DESCRIPTION", b"Selected chapters")),
                    &cat([&u(0x63C4, 1), &u(0x63C4, 2)]),
                ),
            ],
            none(),
            false,
        ),
        p(
            "tags-wildcard",
            vec![tags(None, &u(0x63C5, 0))],
            none(),
            false,
        ),
        p(
            "tags-cross-target",
            vec![
                chapters(Some(&two), false),
                attach(),
                tags(
                    Some(&text(b"DESCRIPTION", b"Multi-kind")),
                    &cat([&u(0x63C5, 1), &u(0x63C4, 2), &u(0x63C6, 1)]),
                ),
            ],
            none(),
            false,
        ),
        p("track-original-false", vec![], u(0x55AE, 0), false),
        p(
            "tags-many",
            vec![tags(
                Some(
                    &(0..512)
                        .flat_map(|i| {
                            text(
                                format!("X_FIELD_{i:04}").as_bytes(),
                                format!("Value {i}").as_bytes(),
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
                b"",
            )],
            none(),
            false,
        ),
        p(
            "chapters-unsorted",
            vec![chapters(
                Some(&cat([
                    &chapter(1, SECOND, Some(2 * SECOND), &eng(b"Later")),
                    &chapter(2, 0, Some(SECOND), &eng(b"Earlier")),
                ])),
                false,
            )],
            none(),
            false,
        ),
    ]);
    rows.push(CatalogueProfile {
        name: "info-duration".into(),
        metadata: vec![],
        extra: vec![],
        late: false,
        info: float64(0x4489, 160.0),
    });
    Ok(rows)
}

fn catalogue_case(source: &Source, profile: &CatalogueProfile) -> Result<Vec<u8>, String> {
    let fields: Fields = vec![(0x55B1, 9), (0x55BA, 16), (0x55BB, 9)];
    let data = colour_case(source, Some(&fields), &mastering(8, false, &MASTERING), 1)?;
    let top = crate::ebml::children(&data, 0, data.len())?;
    let header_end = top
        .iter()
        .find(|c| c.id == 0x1A45DFA3)
        .ok_or("no EBML header")?
        .end;
    let segment = top.iter().find(|c| c.id == SEGMENT).ok_or("no Segment")?;
    let mut pieces = Vec::new();
    for child in crate::ebml::children(&data, segment.start, segment.end)? {
        let mut payload = data[child.start..child.end].to_vec();
        if child.id == INFO {
            payload.extend_from_slice(&profile.info);
        }
        if child.id == TRACKS {
            let entry = crate::ebml::children(&data, child.start, child.end)?[0];
            let mut track = data[entry.start..entry.end].to_vec();
            if !profile.extra.is_empty() {
                track = crate::ebml::children(&data, entry.start, entry.end)?
                    .into_iter()
                    .filter(|f| f.id != 0x22B59C && f.id != 0x22B59D)
                    .flat_map(|f| e(f.id, &data[f.start..f.end]))
                    .collect();
                track.extend_from_slice(&profile.extra);
            }
            payload = e(0xAE, &track);
        }
        pieces.push((child.id, e(child.id, &payload)));
    }
    let index = if profile.late { 3 } else { 2 };
    let mut parts: Vec<(u32, Vec<u8>)> = pieces[..index].to_vec();
    for item in &profile.metadata {
        parts.push((
            u32::from_be_bytes([item[0], item[1], item[2], item[3]]),
            item.clone(),
        ));
    }
    parts.extend_from_slice(&pieces[index..]);
    // Fixed-width offsets keep construction deterministic and allow late metadata.
    let head = |positions: &[(u32, u64)]| {
        let seeks: Vec<u8> = positions
            .iter()
            .flat_map(|&(id, at)| {
                e(
                    0x4DBB,
                    &cat([&e(0x53AB, &id.to_be_bytes()), &e(0x53AC, &at.to_be_bytes())]),
                )
            })
            .collect();
        e(0x114D9B74, &seeks)
    };
    let initial = head(
        &parts
            .iter()
            .filter(|(id, _)| *id != CLUSTER)
            .map(|(id, _)| (*id, 0))
            .collect::<Vec<_>>(),
    );
    let mut positions = Vec::new();
    let mut offset = initial.len() as u64;
    for (id, raw) in &parts {
        if *id != CLUSTER {
            positions.push((*id, offset));
        }
        offset += raw.len() as u64;
    }
    let seek = head(&positions);
    if seek.len() != initial.len() {
        return Err("SeekHead size depends on its offsets".into());
    }
    let body: Vec<u8> = seek
        .into_iter()
        .chain(parts.into_iter().flat_map(|(_, raw)| raw))
        .collect();
    Ok(cat([&data[..header_end], &e(SEGMENT, &body)]))
}

fn catalogue(paths: &Paths, source: &Source) -> Result<String, String> {
    let output = paths.fixtures("catalogue-fixtures")?;
    let mut rows = Vec::new();
    for profile in catalogue_profiles()? {
        let data = catalogue_case(source, &profile)?;
        let file = format!("{}.mkv", profile.name);
        write(&output.join(&file), &data)?;
        rows.push(authored(&profile.name, &file, &data, vec![]));
    }
    let count = rows.len();
    paths.manifest(
        "features/matroska-segment-metadata/corpus.json",
        &Value::object([("schema", Value::int(1)), ("cases", Value::Array(rows))]),
    )?;
    Ok(format!("Generated {count} catalogue profiles"))
}

// ---------------------------------------------------------------------------
// Playback startup and seek capsules.

const PLAYBACK_CORPUS: &str = "harness/features/matroska-startup-seek/corpus.json";

/// `SMVCL001`, the operation (0 startup, 1 seek after discovery, 2 seek after the
/// end of file), the seek target in milliseconds as a big-endian `i64`, then the file.
pub fn playback_capsule(operation: &str, target_ms: i64, source: &[u8]) -> Result<Vec<u8>, String> {
    let kind = match operation {
        "startup" => 0,
        "seek-after-discovery" => 1,
        "seek-after-end" => 2,
        _ => return Err(format!("unknown playback operation {operation}")),
    };
    let mut capsule = b"SMVCL001".to_vec();
    capsule.push(kind);
    capsule.extend_from_slice(&target_ms.to_be_bytes());
    capsule.extend_from_slice(source);
    Ok(capsule)
}

/// Build every capsule of the committed playback manifest from its pinned source
/// and refuse any whose source or capsule hash differs from the manifest.
fn playback_capsules(paths: &Paths) -> Result<String, String> {
    let output = paths.fixtures("startup-seek-fixtures")?;
    let text = fs::read_to_string(paths.root.join(PLAYBACK_CORPUS))
        .map_err(|e| format!("{PLAYBACK_CORPUS}: {e}"))?;
    let manifest = json::parse(&text).map_err(|e| format!("{PLAYBACK_CORPUS}: {e}"))?;
    let rows = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("playback manifest has no cases")?;
    for row in rows {
        let field = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("playback case without {key}"))
        };
        let id = field("id")?;
        let source_path = field("source")?;
        let source =
            fs::read(paths.root.join(source_path)).map_err(|e| format!("{source_path}: {e}"))?;
        if sha256::hex(&source) != field("source_sha256")? {
            return Err(format!(
                "{id}: source {source_path} differs from the manifest"
            ));
        }
        let target = crate::harness::linux::py_int(row.get("target_ms"))
            .and_then(|t| i64::try_from(t).ok())
            .ok_or_else(|| format!("{id}: invalid target_ms"))?;
        let capsule = playback_capsule(field("operation")?, target, &source)?;
        if sha256::hex(&capsule) != field("sha256")? {
            return Err(format!("{id}: capsule differs from the manifest"));
        }
        let file = field("file")?;
        if Path::new(file)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{id}: file escapes the corpus"));
        }
        write(&output.join(file), &capsule)?;
    }
    Ok(format!("Built {} playback capsules", rows.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_capsules_match_the_manifest_and_refuse_changed_sources() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let out = crate::walk::scratch("capsules");
        let paths = Paths {
            root: root.clone(),
            out: out.clone(),
        };
        assert_eq!(
            playback_capsules(&paths).unwrap(),
            "Built 44 playback capsules"
        );
        let capsule =
            fs::read(out.join(".research/startup-seek-fixtures/seek-hevc-seek-700-eof.case"))
                .unwrap();
        assert_eq!(&capsule[..17], b"SMVCL001\x02\0\0\0\0\0\0\x02\xbc");
        assert!(playback_capsule("rewind", 0, b"").is_err());
        fs::remove_dir_all(out).unwrap();
    }

    /// Regenerating into a scratch tree reproduces every committed manifest byte
    /// for byte, so every fixture hash and contract is unchanged.
    #[test]
    fn regeneration_reproduces_the_committed_manifests() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let out = crate::walk::scratch("corpus");
        generate_all(&Paths {
            root: root.clone(),
            out: out.clone(),
        })
        .unwrap();
        for name in [
            "features/matroska-demux/corpus.json",
            "features/matroska-header-removal/corpus.json",
            "features/matroska-zlib/corpus.json",
            "features/matroska-zlib/contract.json",
            "features/matroska-cues-preroll/corpus.json",
            "features/matroska-cues-preroll/contract.json",
            "features/matroska-colour/corpus.json",
            "features/matroska-segment-metadata/corpus.json",
            "features/mp4-range/corpus.json",
        ] {
            let ours = fs::read_to_string(out.join("harness").join(name)).unwrap();
            let committed = fs::read_to_string(root.join("harness").join(name)).unwrap();
            assert!(
                ours == committed,
                "{name} differs from the committed manifest"
            );
        }
        fs::remove_dir_all(out).unwrap();
    }
}
