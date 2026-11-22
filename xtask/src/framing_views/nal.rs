//! Authored inputs for the NAL reader and configuration comparisons.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::corpus::Paths;
use crate::json::{self, Value};
use crate::sha256;

pub(super) const IDS: [&str; 3] = [
    "nal-length-prefixed",
    "nal-config-inputs",
    "hevc-record-annexb",
];
const DIRECTORY: &str = ".research/nal-corpus";
const RAW: &str = "crates/framing/fixtures/raw-annex-b";
const SCRIPTS: &str = "harness/features/packet-framing/corpus.json";
const AVC_CONTROL: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0, 10, 0x80, 0, 0, 1, 0x68, 0xce, 6, 0xe2,
];
const HVC_PREFIX: [u8; 23] = [
    1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 30, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f, 0,
];
// The three synthetic HEVC elementary streams produce the same record when remuxed.
const REMUXED_HEVC: &str = concat!(
    "0101600000009000000000001ef000fcfdf8f800000f03200001001840010c01",
    "ffff01600000030090000003000003001e959009210001002942010101600000",
    "030090000003000003001ea01420616595924932bc05a0200000030020000003",
    "018122000100074401c172b42240",
);

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    Avc,
    Hevc,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::Avc => "avc",
            Self::Hevc => "hevc",
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Avc => 1,
            Self::Hevc => 2,
        }
    }

    fn width(self, record: &[u8]) -> usize {
        // The original no-extradata and Annex B cases use four-byte sample fields.
        let index = if self == Self::Avc { 4 } else { 21 };
        record
            .get(index)
            .filter(|_| !record.starts_with(&[0, 0, 0, 1]))
            .map_or(4, |byte| usize::from(byte & 3) + 1)
    }

    fn empty(self, width: usize) -> Vec<u8> {
        let bits = u8::try_from(width - 1).unwrap();
        match self {
            Self::Avc => vec![1, 66, 0, 10, 0xfc | bits, 0xe0, 0],
            Self::Hevc => {
                let mut record = HVC_PREFIX;
                record[21] = 0x0c | bits;
                record.to_vec()
            }
        }
    }

    fn nal_type(self, data: &[u8]) -> u8 {
        if self == Self::Avc {
            data[0] & 31
        } else {
            (data[0] >> 1) & 63
        }
    }

    fn parameter(self, data: &[u8]) -> bool {
        !data.is_empty()
            && match self {
                Self::Avc => matches!(self.nal_type(data), 7 | 8),
                Self::Hevc => matches!(self.nal_type(data), 32..=34),
            }
    }

    fn header_bytes(self) -> usize {
        usize::from(self.byte())
    }
}

#[derive(Default)]
struct Corpus {
    cases: Vec<Value>,
}

impl Corpus {
    fn add(&mut self, file: &str, id: &str, profile: &str, hash: &str, args: &[&str]) {
        self.cases.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(file)),
            ("sha256", Value::str(hash)),
            ("rights", Value::str("authored-synthetic")),
            ("required", Value::Bool(true)),
            ("arguments", Value::strings(args.iter().copied())),
        ]));
    }
}

struct Corpora<'a> {
    output: &'a Path,
    units: Corpus,
    config: Corpus,
    hvcc: Corpus,
}

impl Corpora<'_> {
    fn units(&mut self, id: &str, profile: &str, codec: Codec, bytes: &[u8]) -> Result<()> {
        let file = format!("{id}.bsf");
        store(&self.output.join(&file), bytes)?;
        let hash = sha256::hex(bytes);
        for window in ["1", "7", "4096"] {
            self.units.add(
                &file,
                &format!("{id}-w{window}"),
                profile,
                &hash,
                &[codec.name(), window],
            );
        }
        Ok(())
    }

    fn annexb(&mut self, id: &str, codec: Codec, bytes: &[u8]) -> Result<()> {
        let file = format!("{id}.annexb");
        store(&self.output.join(&file), bytes)?;
        self.config.add(
            &file,
            id,
            "annexb",
            &sha256::hex(bytes),
            &["annexb", codec.name()],
        );
        let mut sample = Vec::new();
        for unit in annexb_units(bytes) {
            append_length(&mut sample, 4, unit)?;
        }
        self.units(
            &format!("lp-{id}"),
            "configuration-blob",
            codec,
            &script(codec, 4, &[sample])?,
        )
    }

    fn declared(
        &mut self,
        id: &str,
        codec: Codec,
        bytes: &[u8],
        custom: Option<&str>,
    ) -> Result<()> {
        let file = format!("{id}.record");
        let width = codec.width(bytes).to_string();
        let layout = custom.map_or_else(|| declarations(codec, bytes), str::to_owned);
        store(&self.output.join(&file), bytes)?;
        self.config.add(
            &file,
            id,
            "declared",
            &sha256::hex(bytes),
            &["declared", codec.name(), &width, &layout],
        );
        Ok(())
    }

    fn hvcc(&mut self, id: &str, profile: &str, bytes: &[u8]) -> Result<()> {
        let file = format!("{id}.hvcc");
        store(&self.output.join(&file), bytes)?;
        let hash = sha256::hex(bytes);
        for mode in ["all", "decoder"] {
            self.hvcc
                .add(&file, &format!("{id}-{mode}"), profile, &hash, &[mode]);
        }
        Ok(())
    }
}

fn store(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn load(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return Err("odd fixture hex length".into());
    }
    pairs
        .iter()
        .map(|pair| {
            let hex = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
            u8::from_str_radix(hex, 16).map_err(|e| e.to_string())
        })
        .collect()
}

fn append_length(sample: &mut Vec<u8>, width: usize, unit: &[u8]) -> Result<()> {
    let size = u32::try_from(unit.len())
        .map_err(|e| e.to_string())?
        .to_be_bytes();
    if size[..4 - width].iter().any(|&byte| byte != 0) {
        return Err(format!(
            "{} bytes do not fit in a {width}-byte field",
            unit.len()
        ));
    }
    sample.extend(&size[4 - width..]);
    sample.extend(unit);
    Ok(())
}

fn be32(value: usize) -> Result<[u8; 4]> {
    u32::try_from(value)
        .map(u32::to_be_bytes)
        .map_err(|e| e.to_string())
}

fn script(codec: Codec, width: usize, samples: &[Vec<u8>]) -> Result<Vec<u8>> {
    let config = codec.empty(width);
    let mut bytes = b"SMBSF001".to_vec();
    bytes.push(codec.byte());
    bytes.extend(be32(config.len())?);
    bytes.extend(config);
    bytes.extend(be32(samples.len() + 1)?);
    for sample in samples {
        bytes.extend([0; 41]); // packet event and unused packet metadata
        bytes.extend(be32(sample.len())?);
        bytes.extend(u32::MAX.to_be_bytes()); // no side data
        bytes.extend(sample);
    }
    bytes.push(2); // drain
    Ok(bytes)
}

// Annex B start codes, with the optional zero_byte and inter-unit padding.
// Zeros at physical EOF remain on the last unit, as in the wire fixture corpus.
fn annexb_units(bytes: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    for (offset, code) in bytes.windows(3).enumerate() {
        if code != [0, 0, 1] {
            continue;
        }
        if let Some(begin) = start {
            let mut end = offset;
            while end > begin && bytes[end - 1] == 0 {
                end -= 1;
            }
            units.push(&bytes[begin..end]);
        }
        start = Some(offset + 3);
    }
    if let Some(begin) = start {
        units.push(&bytes[begin..]);
    }
    units
}

fn join_annexb(units: &[&[u8]], width: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for unit in units {
        bytes.extend(&[0, 0, 0, 1][4 - width..]);
        bytes.extend_from_slice(unit);
    }
    bytes
}

fn raw_samples(codec: Codec, width: usize, packets: &[&[u8]]) -> Result<Vec<Vec<u8>>> {
    packets
        .iter()
        .map(|packet| {
            let mut sample = Vec::new();
            for unit in annexb_units(packet) {
                if width == 1 && unit.len() > 255 {
                    let (header, body) = unit.split_at(codec.header_bytes());
                    for fragment in body.chunks(255 - header.len()) {
                        let mut part = header.to_vec();
                        part.extend(fragment);
                        append_length(&mut sample, width, &part)?;
                    }
                } else {
                    append_length(&mut sample, width, unit)?;
                }
            }
            Ok(sample)
        })
        .collect()
}

fn scripts(paths: &Paths, corpora: &mut Corpora<'_>) -> Result<()> {
    let original = paths.out.join(".research/nal-script-source");
    crate::framing_bench::generate(&original)?;
    let manifest = json::read(&paths.root.join(SCRIPTS))?;
    let distinct = manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("packet framing cases missing")?;
    let mut hashes = BTreeSet::new();
    for case in distinct {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .ok_or("packet framing id missing")?;
        let file = case
            .get("file")
            .and_then(Value::as_str)
            .ok_or("packet framing filename missing")?;
        let input = load(&original.join(file))?;
        if Some(sha256::hex(&input).as_str()) != case.get("sha256").and_then(Value::as_str) {
            return Err(format!("packet framing script changed: {id}"));
        }
        let codec = match input[8] {
            1 => Codec::Avc,
            2 => Codec::Hevc,
            _ => return Err(format!("unexpected codec: {id}")),
        };
        let end = 13 + u32::from_be_bytes(input[9..13].try_into().unwrap()) as usize;
        let raw = &input[13..end];
        let empty = codec.empty(codec.width(raw));
        let mut rewritten = input[..9].to_vec();
        rewritten.extend(be32(empty.len())?);
        rewritten.extend(empty);
        rewritten.extend(&input[end..]);
        corpora.units(&format!("lp-{id}"), "script", codec, &rewritten)?;
        if hashes.insert((codec.byte(), sha256::hex(raw))) {
            corpora.declared(&format!("declared-{id}"), codec, raw, None)?;
            if codec == Codec::Hevc {
                corpora.hvcc(&format!("hvcc-{id}"), "script", raw)?;
            }
        }
    }
    Ok(())
}

fn raw_fixture(paths: &Paths, id: &str, codec: Codec, corpora: &mut Corpora<'_>) -> Result<()> {
    let ext = if codec == Codec::Avc { "h264" } else { "hevc" };
    let root = paths.root.join(RAW);
    let raw = load(&root.join(format!("{id}.{ext}")))?;
    let geometry =
        fs::read_to_string(root.join(format!("{id}.packets"))).map_err(|e| e.to_string())?;
    let mut packets = Vec::new();
    let mut end = 0;
    for line in geometry.lines() {
        let mut fields = line.split_ascii_whitespace();
        let mut field = || {
            fields
                .next()
                .ok_or("missing packet extent")?
                .parse::<usize>()
                .map_err(|e| e.to_string())
        };
        let (offset, length) = (field()?, field()?);
        if offset != end || length == 0 {
            return Err(format!("noncontiguous raw extents: {id}"));
        }
        end = offset + length;
        packets.push(raw.get(offset..end).ok_or("raw extent beyond input")?);
    }
    if end != raw.len() {
        return Err(format!("raw extents omit bytes: {id}"));
    }
    raw_controls(id, codec, &packets, corpora)?;
    if codec == Codec::Hevc {
        corpora.hvcc(
            &format!("hvcc-remux-{id}"),
            "reference-written",
            &unhex(REMUXED_HEVC)?,
        )?;
    }
    Ok(())
}

fn raw_controls(
    id: &str,
    codec: Codec,
    packets: &[&[u8]],
    corpora: &mut Corpora<'_>,
) -> Result<()> {
    for width in 1..=4 {
        let samples = raw_samples(codec, width, packets)?;
        let suffix = if width == 1 && id != "avc-authored" {
            "-fragments"
        } else {
            ""
        };
        corpora.units(
            &format!("lp-{id}-width{width}{suffix}"),
            "raw",
            codec,
            &script(codec, width, &samples)?,
        )?;
    }
    let sets: Vec<_> = packets
        .iter()
        .flat_map(|packet| annexb_units(packet))
        .filter(|unit| codec.parameter(unit))
        .collect();
    let plain = join_annexb(&sets, 4);
    let first = sets.first().ok_or("raw fixture lacks parameter sets")?;
    corpora.annexb(&format!("{id}-three-byte"), codec, &join_annexb(&sets, 3))?;
    corpora.annexb(&format!("{id}-four-byte"), codec, &plain)?;
    let mut leading = vec![0; 5];
    leading.extend(&plain);
    corpora.annexb(&format!("{id}-leading-zeroes"), codec, &leading)?;
    let mut trailing = plain.clone();
    trailing.extend([0; 4]);
    corpora.annexb(&format!("{id}-trailing-zeroes"), codec, &trailing)?;
    let (aud, sei): (&[u8], &[u8]) = if codec == Codec::Avc {
        (&[0x09, 0xf0], &[0x06, 5, 1, 0x7f, 0x80])
    } else {
        (&[0x46, 1, 0x50], &[0x4e, 1, 5, 1, 0x7f, 0x80])
    };
    let mut mixed = vec![*first, aud, sei];
    mixed.extend_from_slice(&sets[1..]);
    corpora.annexb(&format!("{id}-aud-sei"), codec, &join_annexb(&mixed, 4))?;
    let mut duplicates = vec![*first];
    duplicates.extend(&sets);
    corpora.annexb(
        &format!("{id}-duplicate"),
        codec,
        &join_annexb(&duplicates, 4),
    )?;
    corpora.annexb(
        &format!("{id}-no-sets"),
        codec,
        &join_annexb(&[aud, sei], 4),
    )?;
    let mut dangling = plain;
    dangling.extend([0, 0, 1]);
    corpora.annexb(&format!("{id}-dangling-prefix"), codec, &dangling)?;
    let one = join_annexb(&[*first], 4);
    for length in 0..one.len() {
        corpora.annexb(
            &format!("{id}-first-set-cut-{length:02}"),
            codec,
            &one[..length],
        )?;
    }
    Ok(())
}

// The caller supplies these ranges. The declaration describes only the wire
// layout, including an out-of-range final field on a truncated record.
fn declarations(codec: Codec, bytes: &[u8]) -> String {
    let mut layout = String::new();
    if codec == Codec::Hevc {
        let mut cursor = 23;
        for _ in 0..bytes.get(22).copied().unwrap_or(0) {
            let start = cursor;
            let Some(header) = bytes.get(cursor..cursor + 3) else {
                return invalid_layout(layout, bytes.len());
            };
            cursor += 3;
            let count = u16::from_be_bytes([header[1], header[2]]);
            if append_array(
                &mut layout,
                bytes,
                &mut cursor,
                start,
                header[0] & 63,
                if header[0] & 128 == 0 { "0" } else { "1" },
                count,
            ) {
                break;
            }
        }
        if bytes.len() < 23 && !bytes.starts_with(&[0, 0, 0, 1]) {
            return invalid_layout(layout, bytes.len());
        }
    } else {
        let Some(&count) = bytes.get(5) else {
            return invalid_layout(layout, bytes.len());
        };
        let mut cursor = 6;
        if append_array(
            &mut layout,
            bytes,
            &mut cursor,
            5,
            7,
            "-",
            u16::from(count & 31),
        ) {
            return layout;
        }
        let Some(&count) = bytes.get(cursor) else {
            return invalid_layout(layout, bytes.len());
        };
        let start = cursor;
        cursor += 1;
        if append_array(
            &mut layout,
            bytes,
            &mut cursor,
            start,
            8,
            "-",
            u16::from(count),
        ) {
            return layout;
        }
        if matches!(bytes[1], 100 | 110 | 122 | 144)
            && let Some(&count) = bytes.get(cursor + 3)
        {
            let start = cursor;
            cursor += 4;
            append_array(
                &mut layout,
                bytes,
                &mut cursor,
                start,
                13,
                "-",
                u16::from(count),
            );
        }
    }
    layout
}

fn invalid_layout(mut layout: String, size: usize) -> String {
    write!(layout, "0,-,{size},{};", size + 1).unwrap();
    layout
}

fn append_array(
    layout: &mut String,
    bytes: &[u8],
    cursor: &mut usize,
    start: usize,
    nal_type: u8,
    complete: &str,
    count: u16,
) -> bool {
    let mut fields = String::new();
    for _ in 0..count {
        let at = *cursor;
        let payload = at + 2;
        let Some(field) = bytes.get(at..payload) else {
            *cursor = payload;
            write!(fields, ",{at},{payload},{payload}").unwrap();
            break;
        };
        let end = payload + usize::from(u16::from_be_bytes([field[0], field[1]]));
        write!(fields, ",{at},{payload},{end}").unwrap();
        *cursor = end;
        if end > bytes.len() {
            break;
        }
    }
    write!(layout, "{nal_type},{complete},{start},{}{fields};", *cursor).unwrap();
    *cursor > bytes.len()
}

fn samples(paths: &Paths, corpora: &mut Corpora<'_>) -> Result<()> {
    let vectors = fs::read_to_string(paths.root.join("crates/framing/fixtures/framing-v1.tsv"))
        .map_err(|e| e.to_string())?;
    for line in vectors.lines().skip(1) {
        let fields: Vec<_> = line.split('\t').collect();
        let [id, codec, mode, width, hex, ..] = fields.as_slice() else {
            return Err("invalid framing TSV row".into());
        };
        let codec = if *codec == "avc" {
            Codec::Avc
        } else {
            Codec::Hevc
        };
        let bytes = unhex(hex)?;
        if *mode == "annexb" {
            corpora.annexb(id, codec, &bytes)?;
        } else {
            let width = width.parse::<usize>().map_err(|e| e.to_string())?;
            corpora.units(
                &format!("lp-{id}"),
                "boundary",
                codec,
                &script(codec, width, &[bytes])?,
            )?;
        }
    }
    for codec in [Codec::Avc, Codec::Hevc] {
        let header: &[u8] = if codec == Codec::Avc {
            &[0x41]
        } else {
            &[0x02, 1]
        };
        let base = codec.name();
        let mut boundary = vec![0x55; 1 << 20];
        boundary[..header.len()].copy_from_slice(header);
        for (tag, width, bytes) in [
            ("maximum", 4, boundary.as_slice()),
            ("width2-65535", 2, &boundary[..65_535]),
            ("width3-65536", 3, &boundary[..65_536]),
        ] {
            let mut packet = Vec::new();
            append_length(&mut packet, width, bytes)?;
            corpora.units(
                &format!("lp-{base}-{tag}"),
                "boundary",
                codec,
                &script(codec, width, &[packet])?,
            )?;
        }
        let mut over_width = vec![0, 0];
        over_width.extend(&boundary[..65_536]);
        for (tag, width, packet) in [
            ("width2-overflow-65536", 2, over_width),
            ("zero-length", 2, vec![0, 0]),
            ("one-past-end", 1, vec![2, header[0]]),
            ("inside-length", 4, vec![0, 0, 0]),
        ] {
            corpora.units(
                &format!("lp-{base}-{tag}"),
                "boundary",
                codec,
                &script(codec, width, &[packet])?,
            )?;
        }
    }
    corpora.units(
        "lp-hevc-short-header",
        "boundary",
        Codec::Hevc,
        &script(Codec::Hevc, 1, &[vec![1, 2]])?,
    )?;
    corpora.declared(
        "declared-opaque-test-layout",
        Codec::Avc,
        &[0xaa, 0xf1, 0, 2, 0x67, 0xe3, 0xbe, 0, 1, 0x1f, 0xcc],
        Some("7,0,1,6,2,4,6;31,-,6,10,7,9,10;"),
    )?;
    corpora.declared(
        "declared-invalid-range",
        Codec::Avc,
        &Codec::Avc.empty(4),
        Some("7,-,5,8,6,7,8;"),
    )?;
    Ok(())
}

fn array(declared: u8, nals: &[&[u8]]) -> Result<Vec<u8>> {
    let mut bytes = vec![declared];
    bytes.extend(
        u16::try_from(nals.len())
            .map_err(|e| e.to_string())?
            .to_be_bytes(),
    );
    for nal in nals {
        append_length(&mut bytes, 2, nal)?;
    }
    Ok(bytes)
}

fn hvcc_record(arrays: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut bytes = HVC_PREFIX.to_vec();
    bytes[22] = u8::try_from(arrays.len()).map_err(|e| e.to_string())?;
    for array in arrays {
        bytes.extend(array);
    }
    Ok(bytes)
}

fn hevc_controls(corpora: &mut Corpora<'_>) -> Result<()> {
    let vps: &[u8] = &[0x40, 1, 0x80];
    let sps: &[u8] = &[0x42, 1, 0x81];
    let pps: &[u8] = &[0x44, 1, 0x82];
    let v = array(0xa0, &[vps])?;
    let s = array(0x21, &[sps])?;
    let p = array(0xa2, &[pps])?;
    let known = hvcc_record(&[v.clone(), s.clone(), p.clone()])?;
    let large = |size: usize| {
        let mut nal = vec![0x55; size];
        nal[..vps.len()].copy_from_slice(vps);
        nal
    };
    for (id, arrays) in [
        ("out-of-order", vec![p.clone(), s.clone(), v.clone()]),
        (
            "aud-array",
            vec![v.clone(), array(35, &[&[0x46, 1, 0x50]])?],
        ),
        (
            "unknown-array",
            vec![array(63, &[&[0x7e, 1, 0x80]])?, v.clone()],
        ),
        ("unknown-empty", vec![v.clone(), array(63, &[])?]),
        ("empty-array", vec![array(32, &[])?]),
        ("zero-arrays", vec![]),
        ("zero-length-nal", vec![array(32, &[&[]])?]),
        ("duplicate-array", vec![v.clone(), s.clone(), v.clone()]),
        (
            "declaration-mismatch",
            vec![array(32, &[sps])?, array(34, &[&[0x46, 1, 0x50]])?],
        ),
        ("complete", vec![array(0xa1, &[sps])?]),
        ("incomplete", vec![array(0x21, &[sps])?]),
        ("array-limit", vec![v.clone(); 64]),
        ("array-limit-plus-one", vec![v.clone(); 65]),
        ("nal-count-limit", vec![array(32, &[vps; 256])?]),
        ("nal-count-limit-plus-one", vec![array(32, &[vps; 257])?]),
        ("nal-size-limit", vec![array(32, &[&large(65_535)])?]),
        // 23-byte prefix + 3-byte array header + two 2-byte lengths.
        (
            "record-size-limit",
            vec![array(32, &[&large(65_535), &large(65_507)])?],
        ),
        (
            "record-size-limit-plus-one",
            vec![array(32, &[&large(65_535), &large(65_508)])?],
        ),
    ] {
        corpora.hvcc(&format!("hvcc-{id}"), "authored", &hvcc_record(&arrays)?)?;
    }
    for (id, cut) in [("array-header", 24), ("length", 27), ("payload", 29)] {
        corpora.hvcc(&format!("hvcc-truncated-{id}"), "authored", &known[..cut])?;
    }
    let arbitrary_arrays = unhex(
        "01000000000000000000000000f000fcfcf8f800000303a000020003400181000340018137000100046e0900ff61000100024401a5ff",
    )?;
    corpora.hvcc("hvcc-opaque-arrays", "authored", &arbitrary_arrays)?;
    corpora.declared(
        "declared-opaque-arrays",
        Codec::Hevc,
        &arbitrary_arrays,
        None,
    )?;
    Ok(())
}

pub(crate) fn fixtures(paths: &Paths, manifests: bool) -> Result<String> {
    let output = paths.out.join(DIRECTORY);
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut corpora = Corpora {
        output: &output,
        units: Corpus::default(),
        config: Corpus::default(),
        hvcc: Corpus::default(),
    };
    scripts(paths, &mut corpora)?;
    for (id, codec) in [
        ("avc-aud0", Codec::Avc),
        ("avc-aud1", Codec::Avc),
        ("hevc-aud0", Codec::Hevc),
        ("hevc-aud1", Codec::Hevc),
        ("hevc-eos", Codec::Hevc),
    ] {
        raw_fixture(paths, id, codec, &mut corpora)?;
    }
    raw_controls("avc-authored", Codec::Avc, &[AVC_CONTROL], &mut corpora)?;
    samples(paths, &mut corpora)?;
    hevc_controls(&mut corpora)?;
    let mut total = 0;
    for (id, corpus) in IDS
        .into_iter()
        .zip([corpora.units, corpora.config, corpora.hvcc])
    {
        total += corpus.cases.len();
        let expected = Value::object([
            ("schema", Value::int(1)),
            ("directory", Value::str(DIRECTORY)),
            ("cases", Value::Array(corpus.cases)),
        ]);
        if manifests {
            json::write(&output.join(format!("{id}-draft.json")), &expected)?;
        } else {
            let path = paths
                .root
                .join(format!("harness/features/{id}/corpus.json"));
            verify_membership(&path, &expected)?;
        }
    }
    Ok(format!(
        "generated {total} NAL framing and configuration cases"
    ))
}

fn verify_membership(path: &Path, expected: &Value) -> Result<()> {
    let mut pinned = json::read(path)?;
    if let Value::Object(fields) = &mut pinned
        && let Some((_, Value::Array(cases))) = fields.iter_mut().find(|(key, _)| key == "cases")
    {
        for case in cases {
            if let Value::Object(fields) = case {
                fields.retain(|(key, _)| key != "expected");
            }
        }
    }
    if !crate::harness::diff::equal(&pinned, expected) {
        return Err(format!(
            "{}: generated NAL corpus membership changed",
            path.display()
        ));
    }
    Ok(())
}
