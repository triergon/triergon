//! Raw evidence kept outside the repository: deterministic packing, and fetching
//! a set named in `harness/evidence/index.json` from the archive that
//! `TRIERGON_EVIDENCE_ARCHIVE` names (an `s3://` location read with the `aws` CLI).
//!
//! A pack is a `.tar.gz` whose bytes depend only on the files: entries sorted by
//! path, zero timestamps and owners, and gzip stored blocks with no name or time.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::Value;
use crate::sha256;

pub const INDEX: &str = "harness/evidence/index.json";

/// Every regular file under `dir`, hidden ones included, sorted by relative path.
pub fn files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    fn visit(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let kind = fs::symlink_metadata(&path)
                .map_err(|e| e.to_string())?
                .file_type();
            if kind.is_dir() {
                visit(&path, base, out)?;
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(base)
                    .expect("under the base")
                    .to_str()
                    .ok_or_else(|| format!("{}: not UTF-8", path.display()))?
                    .replace('\\', "/");
                out.push((relative, path));
            } else {
                return Err(format!(
                    "{}: not a regular file or directory",
                    path.display()
                ));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn octal(field: &mut [u8], value: u64) {
    let text = format!("{value:0width$o}", width = field.len() - 1);
    field[..text.len()].copy_from_slice(text.as_bytes());
}

fn header(name: &[u8], prefix: &[u8], size: u64, executable: bool, kind: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], if executable { 0o755 } else { 0o644 });
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix);
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    octal(&mut h[148..155], u64::from(sum));
    h[155] = b' ';
    h
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().div_ceil(512) * 512, 0);
}

/// A ustar archive of `files`; paths that do not fit a ustar name and prefix use
/// a pax `path` record.
pub fn tar(files: &[(String, Vec<u8>, bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, data, executable) in files {
        let bytes = path.as_bytes();
        let split = (bytes.len() > 100)
            .then(|| {
                bytes
                    .iter()
                    .enumerate()
                    .filter(|&(i, &b)| b == b'/' && i <= 155 && bytes.len() - i - 1 <= 100)
                    .map(|(i, _)| i)
                    .next_back()
            })
            .flatten();
        let (name, prefix): (&[u8], &[u8]) = match split {
            _ if bytes.len() <= 100 => (bytes, b""),
            Some(i) => (&bytes[i + 1..], &bytes[..i]),
            None => {
                let mut record = format!(" path={path}\n");
                let mut length = record.len();
                while format!("{length}{record}").len() != length {
                    length = format!("{length}{record}").len();
                }
                record = format!("{length}{record}");
                out.extend(header(b"pax", b"", record.len() as u64, false, b'x'));
                out.extend(record.as_bytes());
                pad(&mut out);
                (&bytes[bytes.len().saturating_sub(100)..], b"")
            }
        };
        out.extend(header(name, prefix, data.len() as u64, *executable, b'0'));
        out.extend(data);
        pad(&mut out);
    }
    out.extend([0u8; 1024]);
    out
}

/// gzip of `data` in stored deflate blocks, with no name and a zero timestamp.
pub fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    let mut chunks = data.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend([1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        let len = chunk.len() as u16;
        out.push(u8::from(chunks.peek().is_none()));
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend(chunk);
    }
    out.extend(crate::zlib::crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

/// The deterministic pack of every file under `dir`.
pub fn pack(dir: &Path) -> Result<Vec<u8>, String> {
    let mut entries = Vec::new();
    for (relative, path) in files(dir)? {
        let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&path)
                .map_err(|e| e.to_string())?
                .permissions()
                .mode()
                & 0o111
                != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        entries.push((relative, data, executable));
    }
    Ok(gzip_stored(&tar(&entries)))
}

/// `cargo xtask evidence pack DIR OUT`: write the pack and print its SHA-256.
pub fn pack_command(dir: &Path, out: &Path) -> Result<String, String> {
    let data = pack(dir)?;
    fs::write(out, &data).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(sha256::hex(&data))
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `cargo xtask evidence fetch SET [DIR]`: download the named set, verify its
/// SHA-256 and every file's SHA-256 against the index, and unpack it into DIR
/// (default `.research/evidence/SET`).
pub fn fetch(root: &Path, name: &str, into: Option<&Path>) -> Result<String, String> {
    let index = crate::checks::read_json(&root.join(INDEX))?;
    let set = index
        .get("sets")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .find(|set| s(set, "name") == Some(name))
        .ok_or_else(|| format!("{name} is not in {INDEX}"))?;
    let (key, expected) = (
        s(set, "key").ok_or("set without key")?,
        s(set, "sha256").ok_or("set without sha256")?,
    );
    let archive = std::env::var("TRIERGON_EVIDENCE_ARCHIVE")
        .map_err(|_| "set TRIERGON_EVIDENCE_ARCHIVE to the evidence archive location")?;
    let cache = root.join(".research/evidence-cache");
    fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let download = cache.join(format!("{expected}.tar.gz"));
    if !download.is_file() {
        let partial = cache.join(format!("{expected}.part"));
        let status = Command::new("aws")
            .args(["s3", "cp", "--only-show-errors"])
            .arg(format!("{}/{key}", archive.trim_end_matches('/')))
            .arg(&partial)
            .status()
            .map_err(|e| format!("aws: {e}"))?;
        if !status.success() {
            return Err(format!("aws s3 cp failed: {status}"));
        }
        fs::rename(&partial, &download).map_err(|e| e.to_string())?;
    }
    let data = fs::read(&download).map_err(|e| e.to_string())?;
    if sha256::hex(&data) != expected {
        let _ = fs::remove_file(&download);
        return Err(format!("{key}: SHA-256 differs from the index"));
    }
    let tar = crate::archive::gunzip(&data, 1 << 32)?;
    let entries = crate::archive::tar_files(&tar)?;
    // A set packed from repository files lists each file; an archived run copied
    // in whole is verified by its archive hash alone.
    let listed = set.get("files").and_then(Value::as_array);
    if listed.is_some_and(|l| l.len() != entries.len()) {
        return Err(format!("{name}: archive and index list different files"));
    }
    let target = into.map_or_else(
        || root.join(".research/evidence").join(name),
        Path::to_path_buf,
    );
    for (at, entry) in entries.iter().enumerate() {
        let relative = entry.path.as_str();
        if let Some(file) = listed.map(|l| &l[at]) {
            let path = s(file, "path").unwrap_or_default();
            let indexed = path
                .strip_prefix(s(set, "path").unwrap_or_default())
                .and_then(|p| p.strip_prefix('/'))
                .unwrap_or(path);
            if relative != indexed || Some(sha256::hex(entry.data).as_str()) != s(file, "sha256") {
                return Err(format!("{name}: {relative} differs from the index"));
            }
        }
        if Path::new(relative)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{name}: {relative} escapes the set"));
        }
        let out = target.join(relative);
        fs::create_dir_all(out.parent().expect("file has a parent")).map_err(|e| e.to_string())?;
        fs::write(&out, entry.data).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    Ok(format!(
        "{name}: {} files verified into {}",
        entries.len(),
        target.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_are_deterministic_and_round_trip() {
        let dir = crate::walk::scratch("evidence");
        let long = "a".repeat(120);
        fs::create_dir_all(dir.join("x/.hidden")).unwrap();
        fs::create_dir_all(dir.join(&long)).unwrap();
        fs::write(dir.join("x/.hidden/f.json"), b"{}\n").unwrap();
        fs::write(dir.join("b.txt"), vec![7u8; 200_000]).unwrap();
        fs::write(dir.join(&long).join(format!("{long}.json")), b"long").unwrap();
        fs::write(dir.join("empty"), b"").unwrap();
        let first = pack(&dir).unwrap();
        assert_eq!(first, pack(&dir).unwrap());
        let tar = crate::archive::gunzip(&first, 1 << 30).unwrap();
        let entries = crate::archive::tar_files(&tar).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            names,
            [
                format!("{long}/{long}.json"),
                "b.txt".to_owned(),
                "empty".to_owned(),
                "x/.hidden/f.json".to_owned()
            ]
        );
        assert_eq!(entries[1].data.len(), 200_000);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_index_lists_every_archived_file_once() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let Ok(index) = crate::checks::read_json(&root.join(INDEX)) else {
            return;
        };
        let mut paths = std::collections::BTreeSet::new();
        for set in index.get("sets").and_then(Value::as_array).unwrap() {
            let sha = s(set, "sha256").unwrap();
            assert!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(s(set, "key").unwrap().ends_with(&format!("{sha}.tar.gz")));
            for file in set
                .get("files")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                let path = s(file, "path").unwrap();
                assert!(paths.insert(path.to_owned()), "{path} listed twice");
                if path.starts_with("research/") {
                    assert!(!root.join(path).exists(), "{path} is still tracked");
                }
            }
        }
    }
}
