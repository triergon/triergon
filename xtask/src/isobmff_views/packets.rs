//! Small authored variants of the ISO BMFF packet fixtures.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::Path;

use crate::json::{self, Value};

const FIXTURES: &str = "crates/isobmff/tests/fixtures";
const PCM: &str = "harness/features/quicktime-pcm-samples/variants";
const DTS: &str = "harness/features/isobmff-dts-signalling/variants";
const RECOVERY: &str = "harness/features/mov-run-recovery/variants";
const ENCODED: &str = "harness/features/mov-encoded-views/variants";
const STSD: &[[u8; 4]] = &[*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd"];
type Result<T> = std::result::Result<T, String>;

pub(super) fn author(root: &Path, out: &Path) -> Result<()> {
    let read = |name: &str| fs::read(root.join(FIXTURES).join(name)).map_err(|e| e.to_string());
    let pcm = read("qt-pcm/pcm-s16be-mono-end.mov")?;
    let dts = read("range-encoded-edits/leading-eac3.mp4")?;
    let mut variants = Vec::new();
    pcm_variants(&pcm, &mut variants)?;
    dts_variants(&dts, &mut variants)?;
    let base = read("r-front-stco.mov")?;
    let geometry = read("transport/geometry-20.mov")?;
    for (name, original, relative) in [
        ("physical-base", base.clone(), None),
        ("physical-short", base.clone(), Some(1)),
        ("physical-large-short", geometry, Some(1)),
        (
            "physical-after-mdat",
            [base.as_slice(), b"\0\0\0\x08free"].concat(),
            Some(1),
        ),
    ] {
        let mut data = original;
        if let Some(relative) = relative {
            let offset = data.len() - relative;
            let mut path = STSD.to_vec();
            *path.last_mut().ok_or("empty sample-table path")? = *b"stco";
            let field = locate(&data, &path)?.last().ok_or("missing stco")?.start + 16;
            data[field..field + 4].copy_from_slice(&(offset as u32).to_be_bytes());
        }
        variants.push((RECOVERY, name.into(), data));
    }
    for (version, seconds) in [(0, (1_u64 << 31) - 1), (0, 1 << 31), (1, 1 << 32)] {
        let mut data = read(&format!("encoded-metadata/creation-v{version}-0.mov"))?;
        let path = [*b"moov", *b"trak", *b"mdia", *b"mdhd"];
        let field = locate(&data, &path)?.pop().ok_or("missing mdhd")?.start + 12;
        if version == 0 {
            data[field..field + 4].copy_from_slice(&(seconds as u32).to_be_bytes());
        } else {
            data[field..field + 8].copy_from_slice(&seconds.to_be_bytes());
        }
        variants.push((ENCODED, format!("creation-v{version}-{seconds}"), data));
    }
    for (directory, name, bytes) in variants {
        let directory = out.join(directory);
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        fs::write(directory.join(format!("{name}.mov")), bytes).map_err(|e| e.to_string())?;
    }
    recovery_corpus(root, out)
}

fn recovery_corpus(root: &Path, out: &Path) -> Result<()> {
    let mut cases = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |prefix: &str, name: &str, mode: &str, base: &Path| -> Result<()> {
        let file = format!("{prefix}/{name}");
        let hash = crate::harness::linux::file_sha(&base.join(&file))?;
        let stem = name.trim_end_matches(".mov");
        for (schedule, window, quantum) in [("ordinary", "65536", "4096"), ("tiny", "1", "3")] {
            if seen.insert((hash.clone(), mode.to_owned(), schedule)) {
                cases.push(Value::object([
                    ("id", Value::str(format!("{stem}-{schedule}"))),
                    ("profile", Value::str(format!("{mode}-{schedule}"))),
                    ("file", Value::str(&file)),
                    ("sha256", Value::str(&hash)),
                    ("rights", Value::str("authored-synthetic")),
                    ("arguments", Value::strings([window, quantum, mode])),
                ]));
            }
        }
        Ok(())
    };
    for (directory, mode) in [
        ("run-recovery", "run"),
        ("field-recovery", "run"),
        ("direct-recovery", "direct"),
        ("field-recovery", "physical"),
    ] {
        let prefix = format!("{FIXTURES}/{directory}");
        let mut names = fs::read_dir(root.join(&prefix))
            .map_err(|error| error.to_string())?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name())
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>>>()?;
        names.sort_unstable();
        for name in &names {
            let name = name.to_str().ok_or("non-UTF-8 recovery fixture name")?;
            let is_movie = Path::new(name).extension().is_some_and(|ext| ext == "mov");
            if is_movie && name.starts_with("tail-") == (mode == "physical") {
                add(&prefix, name, mode, root)?;
            }
        }
    }
    for name in [
        "physical-after-mdat",
        "physical-base",
        "physical-large-short",
        "physical-short",
    ] {
        add(RECOVERY, &format!("{name}.mov"), "physical", out)?;
    }
    json::write(
        &out.join(RECOVERY).with_file_name("corpus.json"),
        &Value::object([
            ("schema", Value::int(1)),
            ("directory", Value::str(".")),
            ("cases", Value::Array(cases)),
        ]),
    )
}

type Variants = Vec<(&'static str, String, Vec<u8>)>;

fn pcm_variants(template: &[u8], variants: &mut Variants) -> Result<()> {
    for (tag, bits, channels) in [
        (*b"raw ", 8, 0),
        (*b"raw ", 8, 3),
        (*b"raw ", 16, 4),
        (*b"twos", 8, 1),
        (*b"twos", 24, 5),
        (*b"twos", 32, 6),
        (*b"sowt", 16, 2),
        (*b"sowt", 24, 7),
        (*b"sowt", 32, 8),
    ] {
        let name = format!(
            "{}-{bits}bit-{channels}ch",
            String::from_utf8_lossy(&tag).trim()
        );
        let sound = sound(0, channels, bits, 48_000 << 16);
        let width = u32::from(channels).max(1) * u32::from(bits) / 8;
        variants.push((PCM, name, pcm_movie(template, tag, &sound, width, false)?));
    }
    let mut fractional = sound(0, 1, 16, (48_000 << 16) + 32_768);
    fractional[10..12].copy_from_slice(&37_u16.to_be_bytes());
    fractional[12..16].copy_from_slice(b"test");
    variants.push((
        PCM,
        "fractional-rate".into(),
        pcm_movie(template, *b"twos", &fractional, 2, false)?,
    ));
    let mut auxiliary = sound(0, 1, 16, 48_000 << 16);
    auxiliary[20..24].copy_from_slice(&[0xff, 0xfe, 0, 7]);
    variants.push((
        PCM,
        "compression-and-packet-size".into(),
        pcm_movie(template, *b"twos", &auxiliary, 2, false)?,
    ));
    variants.push((
        PCM,
        "partial-frame".into(),
        pcm_movie(template, *b"twos", &sound(0, 1, 16, 48_000 << 16), 2, true)?,
    ));
    let mut v1 = sound(1, 1, 16, 48_000 << 16);
    for value in [1_u32, 2, 2, 2] {
        v1.extend(value.to_be_bytes());
    }
    variants.push((
        PCM,
        "sound-v1".into(),
        pcm_movie(template, *b"twos", &v1, 2, false)?,
    ));
    let mut v2 = sound(2, 3, 16, 1 << 16);
    v2[20..22].copy_from_slice(&(-2_i16).to_be_bytes());
    v2.extend(72_u32.to_be_bytes());
    v2.extend(48_000_f64.to_be_bytes());
    for value in [1_u32, 0x7f00_0000, 16, 14, 2, 1] {
        v2.extend(value.to_be_bytes());
    }
    variants.push((
        PCM,
        "sound-v2".into(),
        pcm_movie(template, *b"lpcm", &v2, 2, false)?,
    ));
    Ok(())
}

fn pcm_movie(
    template: &[u8],
    tag: [u8; 4],
    sound: &[u8],
    width: u32,
    partial: bool,
) -> Result<Vec<u8>> {
    let mut movie = template.to_vec();
    sample_entry(&mut movie, tag, sound)?;
    let mut stsz = STSD.to_vec();
    *stsz.last_mut().ok_or("empty stsz path")? = *b"stsz";
    let range = locate(&movie, &stsz)?.pop().ok_or("missing stsz")?;
    let count = word(&movie, range.start + 16);
    let mut sizes = vec![0; 4];
    sizes.extend(if partial { 0 } else { width }.to_be_bytes());
    sizes.extend(count.to_be_bytes());
    let bytes = count * width - u32::from(partial);
    if partial {
        for index in 0..count {
            sizes.extend((width - u32::from(index == 0)).to_be_bytes());
        }
    }
    replace(&mut movie, &stsz, &sizes)?;
    let payload: Vec<u8> = (0..bytes).map(|b| (b % 251) as u8).collect();
    replace(&mut movie, &[*b"mdat"], &payload)?;
    Ok(movie)
}

fn dts_variants(template: &[u8], variants: &mut Variants) -> Result<()> {
    let mut add = |name: String, tag, sound: &[u8]| {
        let mut file = template.to_vec();
        sample_entry(&mut file, tag, sound)?;
        variants.push((DTS, name, file));
        Ok::<(), String>(())
    };
    let sound = sound(0, 2, 16, 48_000 << 16);
    for (tag, frequency, construction, duration) in [
        (*b"dtsc", 48_000, 1, 0),
        (*b"dtsh", 96_000, 14, 1),
        (*b"dtsl", 96_000, 14, 2),
        (*b"dtse", 48_000, 18, 3),
        (*b"dtsx", 48_000, 0, 0),
    ] {
        let mut entry = sound.clone();
        entry.extend(atom(*b"ddts", &ddts(frequency, construction, duration)));
        add(String::from_utf8_lossy(&tag).into_owned(), tag, &entry)?;
    }
    let record = ddts(48_000, 1, 0);
    for end in 0..record.len() {
        let mut entry = sound.clone();
        entry.extend(atom(*b"ddts", &record[..end]));
        add(format!("ddts-truncated-{end:02}"), *b"dtsc", &entry)?;
    }
    for (name, record) in [
        ("ddts-zero-frequency", ddts(0, 1, 0)),
        ("ddts-largest-duration", ddts(192_000, 4, 3)),
    ] {
        let mut entry = sound.clone();
        entry.extend(atom(*b"ddts", &record));
        add(name.into(), *b"dtsc", &entry)?;
    }
    for oti in [0x40, 0xa9, 0xaa, 0xab, 0xac] {
        let mut entry = sound.clone();
        let descriptors = [
            0, 0, 0, 0, 3, 0x80, 0x80, 0x80, 27, 0, 1, 0, 4, 0x80, 0x80, 0x80, 13, oti, 0x15, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 0x80, 0x80, 0x80, 1, 2,
        ];
        entry.extend(atom(*b"esds", &descriptors));
        add(format!("mp4a-object-{oti:02x}"), *b"mp4a", &entry)?;
    }
    let mut quicktime = sound;
    quicktime[8..10].copy_from_slice(&1_u16.to_be_bytes());
    quicktime[20..22].copy_from_slice(&(-2_i16).to_be_bytes());
    for value in [512_u32, 0, 0, 2] {
        quicktime.extend(value.to_be_bytes());
    }
    add("dtsc-quicktime-v1".into(), *b"dtsc", &quicktime)
}

fn ddts(frequency: u32, construction: u8, duration: u8) -> [u8; 20] {
    let mut config = [0; 20];
    config[..4].copy_from_slice(&frequency.to_be_bytes());
    config[4..8].copy_from_slice(&900_000_u32.to_be_bytes());
    config[8..12].copy_from_slice(&750_000_u32.to_be_bytes());
    config[12] = 24;
    config[13] = (duration << 6) | (construction << 1);
    config[14..17].copy_from_slice(&((2_u32 << 18) | (384 << 4)).to_be_bytes()[1..]);
    config[18] = 2;
    config
}

fn sound(version: u16, channels: u16, bits: u16, rate: u32) -> Vec<u8> {
    let mut entry = vec![0; 28];
    entry[6..8].copy_from_slice(&1_u16.to_be_bytes());
    entry[8..10].copy_from_slice(&version.to_be_bytes());
    entry[16..18].copy_from_slice(&channels.to_be_bytes());
    entry[18..20].copy_from_slice(&bits.to_be_bytes());
    entry[24..28].copy_from_slice(&rate.to_be_bytes());
    entry
}

fn sample_entry(movie: &mut Vec<u8>, tag: [u8; 4], sound: &[u8]) -> Result<()> {
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(atom(tag, sound));
    replace(movie, STSD, &stsd)
}

fn atom(tag: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut data = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    data.extend(tag);
    data.extend(payload);
    data
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(
        bytes[at..at + 4]
            .try_into()
            .expect("four-byte fixture field"),
    )
}

fn locate(bytes: &[u8], path: &[[u8; 4]]) -> Result<Vec<Range<usize>>> {
    let mut ranges = Vec::new();
    let mut parent = 0..bytes.len();
    for tag in path {
        let mut cursor = parent.start;
        let range = loop {
            if cursor + 8 > parent.end {
                return Err(format!(
                    "missing fixture box {}",
                    String::from_utf8_lossy(tag)
                ));
            }
            let size = word(bytes, cursor) as usize;
            if size < 8 || size > parent.end - cursor {
                return Err("invalid fixture box size".into());
            }
            if &bytes[cursor + 4..cursor + 8] == tag {
                break cursor..cursor + size;
            }
            cursor += size;
        };
        parent = range.start + 8..range.end;
        ranges.push(range);
    }
    Ok(ranges)
}

fn replace(movie: &mut Vec<u8>, path: &[[u8; 4]], payload: &[u8]) -> Result<()> {
    let ranges = locate(movie, path)?;
    let leaf = ranges.last().ok_or("empty fixture path")?;
    for range in &ranges {
        let size = range.len() + payload.len() + 8 - leaf.len();
        movie[range.start..range.start + 4].copy_from_slice(&(size as u32).to_be_bytes());
    }
    movie.splice(leaf.start + 8..leaf.end, payload.iter().copied());
    Ok(())
}
