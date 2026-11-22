//! Reference comparison inputs for checksums.

use std::fs;
use std::path::Path;

use crate::json::{self, Value};
use crate::{pyrandom::PyRandom, sha256};

pub(super) struct Cases<'a> {
    root: &'a Path,
    id: &'a str,
    rows: Vec<Value>,
}

impl<'a> Cases<'a> {
    pub(super) fn new(root: &'a Path, id: &'a str) -> Self {
        Self {
            root,
            id,
            rows: Vec::new(),
        }
    }

    pub(super) fn pinned(
        &mut self,
        id: &str,
        profile: &str,
        path: &str,
        pin: &str,
        rights: &str,
        args: &[&str],
    ) {
        self.rows.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(path)),
            ("sha256", Value::str(pin)),
            ("rights", Value::str(rights)),
            ("arguments", Value::strings(args.iter().copied())),
        ]));
    }

    pub(super) fn authored(
        &mut self,
        name: &str,
        data: &[u8],
        write: bool,
    ) -> Result<(String, String), String> {
        let file = format!("harness/features/{}/inputs/{name}", self.id);
        let path = self.root.join(&file);
        if fs::read(&path).as_deref().ok() != Some(data) {
            if !write {
                return Err(format!("{file}: fixture differs from its recipe"));
            }
            let directory = path.parent().ok_or("fixture needs a parent")?;
            fs::create_dir_all(directory).map_err(|error| error.to_string())?;
            fs::write(path, data).map_err(|error| error.to_string())?;
        }
        Ok((file, sha256::hex(data)))
    }

    pub(super) fn finish(self, write: bool) -> Result<(), String> {
        let path = self
            .root
            .join(format!("harness/features/{}/corpus.json", self.id));
        if write {
            return json::write(
                &path,
                &Value::object([
                    ("schema", Value::int(1)),
                    ("directory", Value::str(".")),
                    ("cases", Value::Array(self.rows)),
                ]),
            );
        }

        let actual = json::read(&path)?;
        let rows = actual
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("missing cases")?;
        if rows.len() != self.rows.len()
            || rows.iter().zip(&self.rows).any(|(actual, expected)| {
                ["id", "profile", "file", "sha256", "rights", "arguments"]
                    .into_iter()
                    .any(|key| actual.get(key) != expected.get(key))
            })
        {
            return Err(format!(
                "{}: fixture cases differ from their recipe",
                self.id
            ));
        }
        Ok(())
    }
}

pub(super) fn string<'a>(row: &'a Value, field: &str) -> Result<&'a str, String> {
    row.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {field}"))
}

const FUNCTIONS: &[&str] = &[
    "mpeg",
    "ogg",
    "ieee",
    "flac-header",
    "flac-ac3",
    "mpeg-audio",
];

fn registers(
    cases: &mut Cases<'_>,
    id: &str,
    profile: &str,
    file: &str,
    hash: &str,
    rights: &str,
    split: u32,
) {
    let offset = split.to_string();
    for function in FUNCTIONS {
        for (schedule, arg) in [("whole", "whole"), ("split", offset.as_str())] {
            cases.pinned(
                &format!("{id}-{function}-{schedule}"),
                profile,
                file,
                hash,
                rights,
                &[function, arg],
            );
        }
    }
}

pub(super) fn fixtures(root: &Path, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, "crc-values");
    let mut random = PyRandom::new(19);
    let bytes: Vec<_> = (0..64).map(|_| random.below(256) as u8).collect();
    for size in 0..=bytes.len() {
        let name = format!("seed-{size:02}");
        let (path, hash) = cases.authored(&format!("{name}.bin"), &bytes[..size], write)?;
        registers(
            &mut cases,
            &name,
            "seed",
            &path,
            &hash,
            "authored-synthetic",
            random.below(u32::try_from(size + 1).map_err(|error| error.to_string())?),
        );
    }
    let (path, hash) = cases.authored(
        "pat.bin",
        &[
            0x00, 0xb0, 0x0d, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0, 0xe8, 0xf9, 0x5e, 0x7d,
        ],
        write,
    )?;
    registers(
        &mut cases,
        "pat",
        "seed",
        &path,
        &hash,
        "authored-synthetic",
        12,
    );
    let samples = json::read(&root.join("harness/fate/samples.json"))?;
    let sample_rows = samples
        .get("samples")
        .and_then(Value::as_array)
        .ok_or("missing FATE samples")?;
    for (set, profile) in [
        ("codec-derived-streams", "codec"),
        ("ogg-reader-inputs", "ogg"),
    ] {
        let index = json::read(&root.join(format!("harness/evidence/sets/{set}.json")))?;
        let files = index
            .get("files")
            .and_then(Value::as_array)
            .ok_or("missing evidence files")?;
        for (index, row) in files.iter().enumerate() {
            let path = string(row, "path")?;
            let hash = string(row, "sha256")?;
            let rights = sample_rows
                .iter()
                .find(|row| {
                    string(row, "path").ok()
                        == Some(path.strip_prefix(".research/").unwrap_or(path))
                        && string(row, "sha256").ok() == Some(hash)
                })
                .and_then(|row| string(row, "rights").ok())
                .unwrap_or_else(|| {
                    if path.starts_with(".research/ogg-generated/")
                        || path.starts_with(".research/speex-generated/")
                    {
                        "authored-synthetic"
                    } else {
                        "fetched-not-redistributed"
                    }
                });
            let input = root.join(path);
            let size = fs::metadata(&input)
                .map_err(|error| format!("{path}: {error}"))?
                .len();
            let size = u32::try_from(size).map_err(|error| error.to_string())?;
            let name = input
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("missing file name")?;
            registers(
                &mut cases,
                &format!("{profile}-{index:02}-{name}"),
                profile,
                path,
                hash,
                rights,
                random.below(size + 1),
            );
        }
    }
    cases.finish(write)
}
