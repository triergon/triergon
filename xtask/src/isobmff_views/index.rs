//! Authored index boxes and fragmented MOV probes.

use std::{fs, path::Path};

use crate::{
    corpus::Paths,
    harness::linux::file_sha,
    json::{self, Value},
};

type R<T> = Result<T, String>;

const WIRE: &str = "isobmff-segment-indexes";
const LANDINGS: &str = "isobmff-index-landings";
const TRACE: &str = "mov-reference-trace";
pub(super) const IDS: [&str; 3] = [WIRE, LANDINGS, TRACE];
const DIRECTORY: &str = "isobmff-index-fixtures";
const TESTS: &str = "crates/isobmff/tests/fixtures";
const FRAGMENTED: &str = "crates/isobmff/tests/fixtures/f-tfra-mfra.mp4";

struct Corpus<'a> {
    paths: &'a Paths,
    directory: &'a Path,
    cases: Vec<Value>,
}

impl Corpus<'_> {
    fn add(&mut self, id: &str, profile: &str, file: &str, arguments: Vec<String>) -> R<()> {
        let path = if file.starts_with(".research/") {
            self.paths.out.join(file)
        } else {
            self.paths.root.join(file)
        };
        self.cases.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(file)),
            ("sha256", Value::str(file_sha(&path)?)),
            ("rights", Value::str("authored-synthetic")),
            ("arguments", Value::strings(arguments)),
        ]));
        Ok(())
    }

    fn write(&mut self, id: &str, bytes: &[u8]) -> R<String> {
        let name = format!("{id}.bin");
        fs::write(self.directory.join(&name), bytes).map_err(|err| err.to_string())?;
        Ok(format!(".research/{DIRECTORY}/{name}"))
    }

    fn wire(&mut self, id: &str, kind: &str, bytes: &[u8], offset: usize, len: usize) -> R<()> {
        let file = self.write(id, bytes)?;
        self.wire_at(id, kind, &file, offset, len)
    }

    fn wire_at(&mut self, id: &str, kind: &str, file: &str, offset: usize, len: usize) -> R<()> {
        self.add(
            id,
            kind,
            file,
            vec![kind.into(), offset.to_string(), len.to_string(), "1".into()],
        )
    }

    fn finish(self, id: &str, manifest: bool) -> R<()> {
        if manifest {
            json::write(
                &self.directory.join(format!("{id}-draft.json")),
                &Value::object([
                    ("schema", Value::int(1)),
                    ("directory", Value::str(".")),
                    ("cases", Value::Array(self.cases)),
                ]),
            )?;
        }
        Ok(())
    }
}

pub(crate) fn fixtures(paths: &Paths, manifest: bool) -> R<String> {
    let dir = paths.fixtures(DIRECTORY)?;
    for (id, author) in [
        (WIRE, wire as fn(&mut Corpus<'_>) -> R<()>),
        (LANDINGS, landings),
        (TRACE, trace),
    ] {
        let mut corpus = Corpus {
            paths,
            directory: &dir,
            cases: Vec::new(),
        };
        author(&mut corpus)?;
        corpus.finish(id, manifest)?;
    }
    Ok(format!(
        "ISO BMFF index and model fixtures at {}",
        dir.display()
    ))
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(
        u32::try_from(8 + body.len())
            .expect("fixture box length")
            .to_be_bytes(),
    );
    bytes.extend(kind);
    bytes.extend(body);
    bytes
}

fn sidx(
    version: u8,
    track: u32,
    scale: u32,
    tick: u64,
    first: u64,
    entries: &[[u32; 3]],
) -> Vec<u8> {
    let mut body = vec![version, 0, 0, 0];
    body.extend(track.to_be_bytes());
    body.extend(scale.to_be_bytes());
    for value in [tick, first] {
        let wire = value.to_be_bytes();
        body.extend(if version == 0 { &wire[4..] } else { &wire[..] });
    }
    body.extend([0; 2]);
    body.extend(
        u16::try_from(entries.len())
            .expect("fixture entry count")
            .to_be_bytes(),
    );
    for words in entries {
        for word in words {
            body.extend(word.to_be_bytes());
        }
    }
    atom(b"sidx", &body)
}

fn tfra(version: u8, widths: [usize; 3], entries: &[(u64, u64, [u32; 3])]) -> Vec<u8> {
    let [a, b, c] = widths.map(|width| (width - 1) as u32);
    let mut body = vec![version, 0, 0, 0];
    body.extend(7_u32.to_be_bytes());
    body.extend(((a << 4) | (b << 2) | c).to_be_bytes());
    body.extend((entries.len() as u32).to_be_bytes());
    for &(tick, offset, coordinates) in entries {
        for value in [tick, offset] {
            let wire = value.to_be_bytes();
            body.extend(if version == 0 { &wire[4..] } else { &wire[..] });
        }
        for (number, width) in coordinates.into_iter().zip(widths) {
            body.extend(&number.to_be_bytes()[4 - width..]);
        }
    }
    atom(b"tfra", &body)
}

fn truncated(corpus: &mut Corpus<'_>, kind: &str, bytes: &[u8], offset: usize) -> R<()> {
    let length = bytes.len() - offset;
    for n in 0..length {
        corpus.wire(
            &format!("{kind}-truncated-{n}"),
            kind,
            &bytes[..offset + n],
            offset,
            n,
        )?;
    }
    Ok(())
}

fn wire(corpus: &mut Corpus<'_>) -> R<()> {
    corpus.wire_at("tfra-movie", "tfra", FRAGMENTED, 999, 62)?;
    corpus.wire_at("mfro-movie", "mfro", FRAGMENTED, 1061, 16)?;
    let records = [[40, 45000, 0x9000_0000], [0x8000_0018, 90000, 0x2000_0007]];
    for version in 0..=1 {
        let single = sidx(version, 7, 90000, 90000, 32, &records[..1]);
        for (variant, entries) in [
            ("empty", &records[..0]),
            ("one", &records[..1]),
            ("nested", &records[..]),
        ] {
            let mut bytes = atom(b"free", &[0; 92]);
            bytes.extend(sidx(version, 7, 90000, 90000, 32, entries));
            corpus.wire(
                &format!("sidx-v{version}-{variant}"),
                "sidx",
                &bytes,
                100,
                bytes.len() - 100,
            )?;
        }
        if version == 0 {
            truncated(corpus, "sidx", &single, 0)?;
        }
        let empty = tfra(version, [1; 3], &[]);
        corpus.wire(
            &format!("tfra-v{version}-empty"),
            "tfra",
            &empty,
            0,
            empty.len(),
        )?;
        for a in 1..=4 {
            for b in 1..=4 {
                for c in 1..=4 {
                    let widths = [a, b, c];
                    let (time, offset) = if version == 0 {
                        (u64::from(u32::MAX), 4000)
                    } else {
                        ((1u64 << 63) + 7, (1u64 << 32) + 42)
                    };
                    let largest = widths.map(|w| u32::MAX >> ((4 - w) * 8));
                    let bytes = tfra(
                        version,
                        widths,
                        &[(time, offset, largest), (0, 128, [4, 3, 2])],
                    );
                    corpus.wire(
                        &format!("tfra-v{version}-{a}{b}{c}"),
                        "tfra",
                        &bytes,
                        0,
                        bytes.len(),
                    )?;
                }
            }
        }
    }
    let entries: Vec<_> = (0..u16::MAX)
        .map(|i| {
            [
                0x7fff_ffff,
                if i % 2 == 0 { 0x7fff_ffff } else { u32::MAX },
                (u32::from(i % 8) << 28) | 0x8000_0005,
            ]
        })
        .collect();
    let large = sidx(1, 7, 90000, u64::MAX - 4, (1 << 32) + 9, &entries);
    corpus.wire("sidx-max-rows-all-sap", "sidx", &large, 0, large.len())?;
    let overflow = sidx(1, 7, 90000, 0, u64::MAX - 64, &[[100, 1, 0]]);
    corpus.wire("sidx-range-overflow", "sidx", &overflow, 0, overflow.len())?;
    let random = tfra(1, [1, 2, 4], &[(5, 4096, [1, 256, 65536])]);
    truncated(corpus, "tfra", &random, 0)?;
    for (id, at, byte) in [
        ("tfra-count-too-large", 23, 2),
        ("tfra-reserved", 16, 1),
        ("tfra-zero-coordinate", 40, 0),
    ] {
        let mut bytes = random.clone();
        bytes[at] = byte;
        corpus.wire(id, "tfra", &bytes, 0, bytes.len())?;
    }
    for (id, kind, size) in [
        ("mfro-parent", b"mfra", 24_u32),
        ("mfro-before-start", b"mfra", 25),
        ("mfro-wrong-kind", b"free", 24),
        ("mfro-short-parent", b"mfra", 1),
    ] {
        let mut body = vec![0; 4];
        body.extend(size.to_be_bytes());
        let bytes = atom(kind, &atom(b"mfro", &body));
        corpus.wire(id, "mfro", &bytes, 8, 16)?;
        if id == "mfro-parent" {
            truncated(corpus, "mfro", &bytes, 8)?;
        }
    }
    Ok(())
}

fn landings(corpus: &mut Corpus<'_>) -> R<()> {
    let original = fs::read(corpus.paths.root.join(FRAGMENTED)).map_err(|e| e.to_string())?;
    let args = |kind: &str, offset: usize, length: usize, ranges: &[u64]| {
        let mut args = vec![
            kind.into(),
            offset.to_string(),
            length.to_string(),
            "1".into(),
            "1".into(),
        ];
        args.extend(ranges.iter().map(u64::to_string));
        args
    };
    corpus.add(
        "tfra-landings",
        "tfra",
        FRAGMENTED,
        args("tfra", 999, 62, &[607, 799, 0, 799, 991, 3000]),
    )?;
    let mut shifted = original.clone();
    shifted[1042..1050].copy_from_slice(&9000_u64.to_be_bytes());
    let file = corpus.write("tfra-shifted", &shifted)?;
    corpus.add(
        "tfra-shifted-landings",
        "tfra",
        &file,
        args("tfra", 999, 62, &[607, 799, 0, 799, 991, 9000]),
    )?;
    for version in 0..=1 {
        let padding = if version == 0 {
            Vec::new()
        } else {
            atom(b"free", &[0; 8])
        };
        let index = sidx(
            version,
            1,
            1000,
            0,
            padding.len() as u64,
            &[[192, 3000, 0x9000_0000]; 2],
        );
        let mut file = original[..607].to_vec();
        file.extend(&index);
        file.extend(&padding);
        let first = file.len() as u64;
        file.extend(&original[607..991]);
        let id = format!("sidx-v{version}-landings");
        let path = corpus.write(&id, &file)?;
        corpus.add(
            &id,
            "sidx",
            &path,
            args(
                "sidx",
                607,
                index.len(),
                &[first, first + 192, 0, first + 192, first + 384, 3000],
            ),
        )?;
    }
    Ok(())
}

fn trace(corpus: &mut Corpus<'_>) -> R<()> {
    for (model, group) in [
        ("receiver", "clock-boundary"),
        ("configuration", "private-catalogue"),
        ("header", "private-catalogue"),
        ("diagnostics", "private-catalogue"),
    ] {
        let mut files = fs::read_dir(corpus.paths.root.join(TESTS).join(group))
            .map_err(|e| e.to_string())?
            .map(|e| e.map(|item| item.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        files.sort_unstable();
        for file in files {
            let file = file.to_str().ok_or("non-UTF-8 model fixture")?;
            if let Some((stem, "mov" | "mp4")) = file.rsplit_once('.') {
                corpus.add(
                    &format!("{model}-{stem}"),
                    model,
                    &format!("{TESTS}/{group}/{file}"),
                    vec![model.into(), "65536".into(), "1".into()],
                )?;
            }
        }
    }
    let clock = fs::read(
        corpus
            .paths
            .root
            .join(TESTS)
            .join("clock-boundary/f-tfdt-above-i64.mp4"),
    )
    .map_err(|e| e.to_string())?;
    let max = i64::MAX as u64;
    let origin = max - (1 << 48);
    let threshold = max - (2 << 48);
    for (profile, ticks) in [
        ("below-threshold", threshold - 1),
        ("threshold", threshold),
        ("above-threshold", threshold + 1),
        ("before-origin", origin - 1),
        ("origin", origin),
        ("after-origin", origin + 1),
        ("near-max-19", max - 19),
        ("near-max-9", max - 9),
        ("decoded-to-max", max - 2),
        ("at-max", max),
    ] {
        // Keep the first fragment and give each of its three samples one tick.
        let mut bytes = clock[..835].to_vec();
        bytes[707..715].copy_from_slice(&ticks.to_be_bytes());
        for duration in [735, 767, 779] {
            bytes[duration..duration + 4].copy_from_slice(&1_u32.to_be_bytes());
        }
        let id = format!("receiver-{profile}");
        let file = corpus.write(&id, &bytes)?;
        corpus.add(
            &id,
            "receiver",
            &file,
            vec!["receiver".into(), "65536".into(), "1".into()],
        )?;
    }
    for (name, fixture, offset, bytes) in [
        ("opus-version", "dOps-only-size-11.mov", 503, [1_u8; 4]),
        ("dts-zero-rate", "ddts-only-size-20.mov", 503, [0; 4]),
    ] {
        let mut file = fs::read(
            corpus
                .paths
                .root
                .join(TESTS)
                .join("private-catalogue")
                .join(fixture),
        )
        .map_err(|e| e.to_string())?;
        file[offset..offset + 4].copy_from_slice(&bytes);
        let file = corpus.write(name, &file)?;
        for model in ["configuration", "header", "diagnostics"] {
            corpus.add(
                &format!("{model}-{name}"),
                model,
                &file,
                vec![model.into(), "65536".into(), "1".into()],
            )?;
        }
    }
    let transport = json::read(
        &corpus
            .paths
            .root
            .join(TESTS)
            .join("transport/PROVENANCE.json"),
    )?;
    for row in transport
        .get("rows")
        .and_then(Value::as_array)
        .ok_or("missing transport rows")?
    {
        let file = row
            .get("file")
            .and_then(Value::as_str)
            .ok_or("missing transport file")?;
        let read = row
            .get("read_maximum")
            .and_then(|value| match value {
                Value::Number(number) => number.as_i128(),
                _ => None,
            })
            .ok_or("missing transport read size")?;
        let stem = file.trim_end_matches(".mov");
        corpus.add(
            &format!("transport-{stem}-{read}"),
            "transport",
            &format!("{TESTS}/transport/{file}"),
            transport_arguments(
                read,
                if read == 64 && matches!(stem, "geometry-01" | "geometry-04") {
                    "416"
                } else {
                    "entry"
                },
            ),
        )?;
    }
    let base = fs::read(
        corpus
            .paths
            .root
            .join(TESTS)
            .join("transport/geometry-00.mov"),
    )
    .map_err(|e| e.to_string())?;
    let stco = base
        .windows(4)
        .position(|v| v == b"stco")
        .ok_or("missing fixture stco")?
        + 12;
    corpus.add(
        "initial-available",
        "initial",
        &format!("{TESTS}/transport/geometry-00.mov"),
        vec!["initial".into(), "1".into(), "1".into()],
    )?;
    for (read, ticks) in [
        (1, [33443, 33444]),
        (2, [33443, 33444]),
        (3, [33444, 33445]),
        (4, [33443, 33444]),
        (7, [33443, 33444]),
        (64, [33497, 33498]),
        (65536, [33515, 33516]),
    ] {
        for at in [747_u32, ticks[0], ticks[1], u32::MAX] {
            let id = format!("initial-{read}-{at}");
            let mut bytes = base.clone();
            bytes[stco..stco + 4].copy_from_slice(&at.to_be_bytes());
            let file = corpus.write(&id, &bytes)?;
            let mut arguments = vec!["initial".into(), read.to_string(), "1".into()];
            if (read, at) == (64, 33497) {
                arguments.push("409".into());
            }
            corpus.add(&id, "initial", &file, arguments)?;
        }
    }
    // The root-walk tests construct trailers around the same front movie.
    let root = fs::read(corpus.paths.root.join(TESTS).join("r-front-stco.mov"))
        .map_err(|e| e.to_string())?;
    let offset = root
        .windows(4)
        .position(|word| word == b"stco")
        .ok_or("missing root stco")?
        + 12;
    for (payload, count, read) in [
        (0, 1, 1),
        (0, 2, 64),
        (0, 3, 64),
        (1, 1, 1),
        (8, 1, 1),
        (9, 1, 1),
        (16, 1, 1),
        (64, 1, 1),
        (32768, 1, 1),
        (32783, 2, 1),
        (32784, 2, 1),
        (32785, 2, 1),
        (0, 14, 1),
    ] {
        let mut bytes = root.clone();
        let footer = atom(b"free", &vec![0; payload]);
        for _ in 0..count {
            bytes.extend_from_slice(&footer);
        }
        let len = u32::try_from(bytes.len()).expect("footer fixture length");
        bytes[offset..offset + 4].copy_from_slice(&len.to_be_bytes());
        let id = format!("transport-footer-{payload}-{count}");
        let file = corpus.write(&id, &bytes)?;
        corpus.add(&id, "transport", &file, transport_arguments(read, "entry"))?;
    }
    Ok(())
}

fn transport_arguments(read: i128, rewind: &str) -> Vec<String> {
    vec![
        "transport".into(),
        read.to_string(),
        "1".into(),
        rewind.into(),
    ]
}
