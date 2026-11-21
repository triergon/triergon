//! Raw evidence kept outside the repository: deterministic packing, uploading,
//! promoting and fetching the sets the evidence index lists, one file per set in
//! `harness/evidence/sets/`, in the archive that `TRIERGON_EVIDENCE_ARCHIVE` names
//! (an `s3://` location reached with the `aws` CLI).
//!
//! A pack is a tar with entries sorted by path and zero timestamps and owners. As
//! a `.tar.gz` it sits in gzip stored blocks with no name or time, so its bytes
//! depend only on the files; as a `.tar.zst` the system `zstd` compresses and
//! decompresses it.
//!
//! Every upload lands under `qualification/`, which the archive expires 90 days
//! after upload. A set moves under `evidence/`, which is kept, only when a result
//! cites it, so nothing is kept that nothing cites. A set's index entry lists the
//! gate results it holds, which later gate runs reuse once it is kept, each with
//! its gate target and what it measured, which the status tables are rendered from.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::archive::{TarFiles, TarReader};
use crate::harness::measured::{Measured, Source};
use crate::harness::policy::Policy;
use crate::harness::reuse;
use crate::json::{self, Value};
use crate::sha256;

/// The index's own fields: its schema, where keys lie, and how identifiers were
/// renamed. Nothing reads them but the index's own test.
#[cfg(test)]
const INDEX: &str = "harness/evidence/index.json";
/// The index's sets, one file each, `NAME.json`, in the canonical layout.
pub const SETS: &str = "harness/evidence/sets";

/// Every set of the index, by name.
pub fn sets(root: &Path) -> Result<Vec<Value>, String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root.join(SETS)).map_err(|e| format!("{SETS}: {e}"))? {
        let file = entry.map_err(|e| format!("{SETS}: {e}"))?.file_name();
        let file = file
            .to_str()
            .ok_or_else(|| format!("{SETS}/{} is not a set's file", Path::new(&file).display()))?;
        let name = file
            .strip_suffix(".json")
            .ok_or_else(|| format!("{SETS}/{file} is not a set's file"))?;
        names.push(name.to_owned());
    }
    names.sort();
    names.iter().map(|name| set(root, name)).collect()
}

/// The set `name` of the index.
pub fn set(root: &Path, name: &str) -> Result<Value, String> {
    let path = set_path(name)?;
    if !root.join(&path).is_file() {
        return Err(format!("{name} is not in the evidence index ({path})"));
    }
    let set = json::read(&root.join(&path)).map_err(|e| format!("{path}: {e}"))?;
    if s(&set, "name") != Some(name) {
        return Err(format!("{path} does not hold set {name}"));
    }
    Ok(set)
}

/// Where set `name` lies, from the repository root.
fn set_path(name: &str) -> Result<String, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
    {
        return Err(format!("{name:?} is not a set's name"));
    }
    Ok(format!("{SETS}/{name}.json"))
}

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

/// Zeros that pad `len` bytes to the next 512-byte record.
fn padding(len: u64) -> &'static [u8] {
    &[0; 512][..(len.next_multiple_of(512) - len) as usize]
}

/// Writes a ustar archive of `files` (relative path, source file) to `out`, one file
/// at a time; paths that do not fit a ustar name and prefix use a pax `path` record.
/// Errors name the source file, or `out_name` for a failed write.
fn write_tar(
    out: &mut impl Write,
    out_name: &str,
    files: &[(String, PathBuf)],
) -> Result<(), String> {
    let written = |e: io::Error| format!("{out_name}: {e}");
    for (path, source) in files {
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
                let size = record.len() as u64;
                out.write_all(&header(b"pax", b"", size, false, b'x'))
                    .map_err(written)?;
                out.write_all(record.as_bytes()).map_err(written)?;
                out.write_all(padding(size)).map_err(written)?;
                (&bytes[bytes.len().saturating_sub(100)..], b"")
            }
        };
        let read = |e: io::Error| format!("{}: {e}", source.display());
        let file = File::open(source).map_err(read)?;
        let metadata = file.metadata().map_err(read)?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        let size = metadata.len();
        out.write_all(&header(name, prefix, size, executable, b'0'))
            .map_err(written)?;
        let mut body = file.take(size);
        let mut buffer = vec![0u8; 1 << 16];
        let mut copied = 0u64;
        loop {
            let n = match body.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(read(e)),
            };
            out.write_all(&buffer[..n]).map_err(written)?;
            copied += n as u64;
        }
        if copied != size {
            return Err(format!("{}: shrank while packing", source.display()));
        }
        out.write_all(padding(size)).map_err(written)?;
    }
    out.write_all(&[0; 1024]).map_err(written)
}

const STORED_BLOCK: usize = 65_535;

/// A gzip member of stored deflate blocks, with no name and a zero timestamp, written
/// as the data arrives. A full block waits for more data so the last one can be
/// marked final.
struct GzipStored<W: Write> {
    out: W,
    block: Vec<u8>,
    crc: u32,
    len: u64,
}

impl<W: Write> GzipStored<W> {
    fn new(mut out: W) -> io::Result<Self> {
        out.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255])?;
        Ok(Self {
            out,
            block: Vec::with_capacity(STORED_BLOCK),
            crc: 0,
            len: 0,
        })
    }

    fn emit(&mut self, last: bool) -> io::Result<()> {
        let len = self.block.len() as u16;
        self.out.write_all(&[u8::from(last)])?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(&(!len).to_le_bytes())?;
        self.out.write_all(&self.block)?;
        self.block.clear();
        Ok(())
    }

    fn finish(mut self) -> io::Result<W> {
        self.emit(true)?;
        self.out.write_all(&self.crc.to_le_bytes())?;
        // ISIZE is the length modulo 2^32.
        self.out.write_all(&(self.len as u32).to_le_bytes())?;
        Ok(self.out)
    }
}

impl<W: Write> Write for GzipStored<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.block.len() == STORED_BLOCK && !data.is_empty() {
            self.emit(false)?;
        }
        let taken = &data[..data.len().min(STORED_BLOCK - self.block.len())];
        self.block.extend_from_slice(taken);
        self.crc = crate::zlib::crc32_extend(self.crc, taken);
        self.len += taken.len() as u64;
        Ok(taken.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Passes writes through to `out` and hashes them.
struct Hashed<W> {
    out: W,
    hash: sha256::Sha256,
}

impl<W: Write> Write for Hashed<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let written = self.out.write(data)?;
        self.hash.update(&data[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Writes the deterministic pack of `files` to `out`, streaming each file, and
/// returns `out`.
fn pack_files<W: Write>(files: &[(String, PathBuf)], out: W, out_name: &str) -> Result<W, String> {
    let written = |e: io::Error| format!("{out_name}: {e}");
    let mut gzip = GzipStored::new(out).map_err(written)?;
    write_tar(&mut gzip, out_name, files)?;
    gzip.finish().map_err(written)
}

/// `cargo xtask evidence pack DIR OUT`: write the pack and print its SHA-256. `OUT`
/// names the compression: `.tar.gz` stores the tar in gzip blocks, whose bytes depend
/// only on the files; `.tar.zst` compresses it with the system `zstd`, whose bytes
/// also depend on its version. The pack is written to `OUT.part` and renamed once
/// complete.
pub fn pack_command(dir: &Path, out: &Path) -> Result<String, String> {
    let compression = out
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(Compression::of)
        .ok_or_else(|| format!("{}: name the pack .tar.gz or .tar.zst", out.display()))?;
    let files = files(dir)?;
    let mut partial = out.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let name = partial.display().to_string();
    let file = File::create(&partial).map_err(|e| format!("{name}: {e}"))?;
    let packed = match compression {
        Compression::Gzip => pack_gzip(&files, file, &name),
        Compression::Zstd => pack_zstd(&files, file, &name)
            .and_then(|()| sha256::file_hex(&partial).map_err(|e| format!("{name}: {e}"))),
    };
    match packed {
        Ok(digest) => {
            fs::rename(&partial, out).map_err(|e| format!("{}: {e}", out.display()))?;
            Ok(digest)
        }
        Err(error) => {
            let _ = fs::remove_file(&partial);
            Err(error)
        }
    }
}

/// Write the stored-gzip pack of `files` to `file` and return its SHA-256.
fn pack_gzip(files: &[(String, PathBuf)], file: File, name: &str) -> Result<String, String> {
    let hashed = Hashed {
        out: BufWriter::new(file),
        hash: sha256::Sha256::default(),
    };
    let Hashed {
        out: mut written,
        hash,
    } = pack_files(files, hashed, name)?;
    written.flush().map_err(|e| format!("{name}: {e}"))?;
    Ok(hash.finish_hex())
}

/// Stream the tar of `files` through the system `zstd` into `file`.
fn pack_zstd(files: &[(String, PathBuf)], file: File, name: &str) -> Result<(), String> {
    use std::process::Stdio;
    let mut child = Command::new("zstd")
        .args(["--quiet", "-19", "-T0", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(file)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("zstd: {e}"))?;
    let mut input = BufWriter::new(child.stdin.take().expect("piped stdin"));
    let written = write_tar(&mut input, name, files)
        .and_then(|()| input.flush().map_err(|e| format!("{name}: {e}")));
    drop(input);
    let output = child.wait_with_output().map_err(|e| format!("zstd: {e}"))?;
    written?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "zstd failed: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// How long the archive keeps a set, which follows from where it lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retention {
    /// Under `qualification/`: expires 90 days after upload.
    Expiring,
    /// Under `evidence/`: kept, because a result cites it.
    Kept,
}

impl Retention {
    const ALL: [Self; 2] = [Self::Expiring, Self::Kept];

    fn prefix(self) -> &'static str {
        match self {
            Self::Expiring => "qualification/",
            Self::Kept => "evidence/",
        }
    }

    /// The index's `retention` value.
    fn label(self) -> &'static str {
        match self {
            Self::Expiring => "90 days",
            Self::Kept => "retained",
        }
    }

    /// The retention a set's key implies.
    fn of_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| key.starts_with(r.prefix()))
    }
}

fn archive_location() -> Result<String, String> {
    std::env::var("TRIERGON_EVIDENCE_ARCHIVE")
        .map(|archive| archive.trim_end_matches('/').to_owned())
        .map_err(|_| "set TRIERGON_EVIDENCE_ARCHIVE to the evidence archive location".into())
}

fn aws_cp(from: &str, to: &str) -> Result<(), String> {
    let status = Command::new("aws")
        .args(["s3", "cp", "--only-show-errors", from, to])
        .status()
        .map_err(|e| format!("aws: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("aws s3 cp {from} {to} failed: {status}"))
    }
}

/// `cargo xtask evidence upload SET ARCHIVE`: upload a packed set under
/// `qualification/SET/`, where it expires 90 days after upload unless promoted,
/// and print the entry to add to the index.
pub fn upload(root: &Path, name: &str, archive: &Path) -> Result<String, String> {
    let (key, entry) = entry(root, name, archive, None)?;
    let location = archive_location()?;
    aws_cp(&archive.display().to_string(), &format!("{location}/{key}"))?;
    Ok(entry)
}

/// The index entry `upload` would print for packed set `name`, and the key under
/// `qualification/` it goes to, without uploading: for a host that packs a set but
/// holds no archive credentials, whose upload streams through another machine.
/// `key` names another key under `qualification/` that the pack was or will be
/// uploaded to, ending in the pack's SHA-256 and suffix as every key does. Results
/// are graded against `root`'s policy for what they measured.
pub fn entry(
    root: &Path,
    name: &str,
    archive: &Path,
    key: Option<&str>,
) -> Result<(String, String), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
    {
        return Err(format!(
            "{name}: a set name is lowercase letters, digits and `-`"
        ));
    }
    let file_name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let compression = Compression::of(file_name)
        .ok_or_else(|| format!("{}: not a .tar.gz or .tar.zst", archive.display()))?;
    let sha = sha256::file_hex(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let bytes = fs::metadata(archive)
        .map_err(|e| format!("{}: {e}", archive.display()))?
        .len();
    let ending = format!("{sha}{}", compression.suffix());
    let key = match key {
        None => format!("{}{name}/{ending}", Retention::Expiring.prefix()),
        Some(key)
            if Retention::of_key(key) == Some(Retention::Expiring)
                && key.ends_with(&format!("/{ending}")) =>
        {
            key.to_owned()
        }
        Some(key) => {
            return Err(format!(
                "{key}: a set's key lies under {} and ends in /{ending}",
                Retention::Expiring.prefix()
            ));
        }
    };
    let results = gate_results(archive, compression, Policy::load(root)?)?;
    let mut entry = vec![
        ("name", Value::str(name)),
        ("path", Value::str(name)),
        ("retention", Value::str(Retention::Expiring.label())),
        ("reason", Value::str("")),
        ("key", Value::str(key.clone())),
        ("sha256", Value::str(sha)),
        ("bytes", Value::int(bytes)),
    ];
    if !results.is_empty() {
        entry.push(("results", Value::Array(results)));
    }
    let entry = Value::object(entry);
    Ok((key, json::to_python(&entry, Some(1))))
}

/// The fresh gate results a packed set holds ([`reuse::listed`]), which a later gate
/// run reuses once the set is kept, each with what it measured ([`Measured`]) where
/// its command measures, graded against `policy`. The pack is read twice: for the
/// results, then for the reports beside them.
fn gate_results(
    archive: &Path,
    compression: Compression,
    policy: Policy,
) -> Result<Vec<Value>, String> {
    let results = read_pack(archive, compression, |path| {
        path.rsplit('/').next() == Some(reuse::RESULT)
    })?;
    let mut listed = reuse::listed(&results)?;
    // Each result's directory and where its command writes what it measured.
    let sources: Vec<Option<(String, Source)>> = listed
        .iter()
        .map(|result| {
            let path = result.get("path").and_then(Value::as_str)?;
            let command = result.get("command").and_then(Value::as_str)?;
            let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
            Some((directory.to_owned(), Source::of(command)?))
        })
        .collect();
    if sources.iter().all(Option::is_none) {
        return Ok(listed);
    }
    let reports = read_pack(archive, compression, |path| {
        sources.iter().flatten().any(|(directory, source)| {
            below(path, directory).is_some_and(|file| source.reads(file))
        })
    })?;
    for (result, source) in listed.iter_mut().zip(&sources) {
        let (Some((directory, source)), Value::Object(fields)) = (source, result) else {
            continue;
        };
        let read: Vec<(String, Vec<u8>)> = reports
            .iter()
            .filter_map(|(path, data)| {
                let file = below(path, directory).filter(|file| source.reads(file))?;
                Some((file.to_owned(), data.clone()))
            })
            .collect();
        let rows = source.rows(&read).map_err(|e| format!("{directory}/{e}"))?;
        if let Some(measured) = Measured::of_rows(&rows, policy) {
            fields.push(("measured".into(), measured.to_value()));
        }
    }
    Ok(listed)
}

/// `path` below `directory` of a pack (the pack's root for an empty one).
fn below<'a>(path: &'a str, directory: &str) -> Option<&'a str> {
    if directory.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(directory)?.strip_prefix('/')
    }
}

/// Every file of a pack whose path, without a leading `./`, `keep` accepts, with its
/// bytes, in the pack's order.
fn read_pack(
    archive: &Path,
    compression: Compression,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    struct Kept<F> {
        keep: F,
        files: Vec<(String, Vec<u8>)>,
        current: bool,
    }
    impl<F: Fn(&str) -> bool> TarFiles for Kept<F> {
        fn begin(&mut self, path: &str, _: u64) -> Result<(), String> {
            let path = path.trim_start_matches("./");
            self.current = (self.keep)(path);
            if self.current {
                self.files.push((path.to_owned(), Vec::new()));
            }
            Ok(())
        }
        fn data(&mut self, chunk: &[u8]) -> Result<(), String> {
            if self.current
                && let Some((_, data)) = self.files.last_mut()
            {
                data.extend_from_slice(chunk);
            }
            Ok(())
        }
        fn end(&mut self) -> Result<(), String> {
            self.current = false;
            Ok(())
        }
    }
    let mut tar = TarReader::new(Kept {
        keep,
        files: Vec::new(),
        current: false,
    });
    compression.unpack(archive, &mut tar)?;
    Ok(tar.finish()?.files)
}

/// `cargo xtask evidence promote SET`: copy an indexed set from `qualification/`
/// to `evidence/` inside the archive, then record the new key and retention in
/// its file. Promote a set when a result cites it.
pub fn promote(root: &Path, name: &str) -> Result<String, String> {
    let mut set = canonical_set(root, name)?;
    let (from, to) = promote_set(&mut set, name)?;
    let location = archive_location()?;
    aws_cp(&format!("{location}/{from}"), &format!("{location}/{to}"))?;
    write_set(root, name, &set)?;
    Ok(format!("{name}: {from} -> {to}"))
}

/// The set `name` of the index, read strictly from a file in its canonical layout,
/// for a command that rewrites it.
fn canonical_set(root: &Path, name: &str) -> Result<Value, String> {
    set(root, name)?;
    let relative = set_path(name)?;
    let text = fs::read_to_string(root.join(&relative)).map_err(|e| format!("{relative}: {e}"))?;
    let set = json::parse_strict(&text).map_err(|e| format!("{relative}: {e}"))?;
    if json::to_python(&set, Some(1)) + "\n" != text {
        return Err(format!("{relative} is not in its canonical layout"));
    }
    Ok(set)
}

fn write_set(root: &Path, name: &str, set: &Value) -> Result<(), String> {
    let relative = set_path(name)?;
    fs::write(root.join(&relative), json::to_python(set, Some(1)) + "\n")
        .map_err(|e| format!("{relative}: {e}"))
}

/// `cargo xtask evidence measure [SET...]`: record in the index entries of the named
/// sets, or of every kept set listing a result without its gate target, each result's
/// target and what it measured, as `evidence entry` records them for a new set, read
/// from the set's archived pack. Every other field of each result must be the one the
/// pack holds. A pack this downloads is removed once read. Reports each set it
/// records, then fails naming every set it could not.
pub fn measure(root: &Path, names: &[String]) -> Result<String, String> {
    let policy = Policy::load(root)?;
    let names: Vec<String> = if names.is_empty() {
        sets(root)?
            .iter()
            .filter(|set| s(set, "retention") == Some(Retention::Kept.label()))
            .filter(|set| {
                set.get("results")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                    .iter()
                    .any(|result| result.get("target").is_none())
            })
            .filter_map(|set| s(set, "name").map(str::to_owned))
            .collect()
    } else {
        names.to_vec()
    };
    let mut lines = Vec::new();
    let mut failed = Vec::new();
    for name in &names {
        match measure_set(root, name, policy) {
            Ok(line) => lines.push(line),
            Err(error) => failed.push(format!("{name}: {error}")),
        }
    }
    let done = format!("{} of {} sets recorded", lines.len(), names.len());
    lines.extend(failed.iter().cloned());
    lines.push(done);
    if failed.is_empty() {
        Ok(lines.join("\n"))
    } else {
        Err(lines.join("\n"))
    }
}

fn measure_set(root: &Path, name: &str, policy: Policy) -> Result<String, String> {
    let mut set = canonical_set(root, name)?;
    let (cached, _) = cache_path(root, &set)?;
    let downloaded = !cached.is_file();
    let (pack, compression) = download(root, &set)?;
    let fresh = gate_results(&pack, compression, policy);
    if downloaded {
        let _ = fs::remove_file(&pack);
    }
    let fresh = fresh?;
    let Some(Value::Array(results)) = members_mut(&mut set, "results") else {
        return Err("the index lists no gate results of it".into());
    };
    if results.len() != fresh.len() {
        return Err(format!(
            "the index lists {} gate results, the pack holds {}",
            results.len(),
            fresh.len()
        ));
    }
    let same =
        |a: &Value, b: &Value| a.get("path") == b.get("path") && a.get("key") == b.get("key");
    let mut measured = 0;
    for result in results.iter_mut() {
        let path = s(result, "path").unwrap_or_default().to_owned();
        let read = fresh
            .iter()
            .find(|candidate| same(result, candidate))
            .ok_or_else(|| format!("{path}: the pack holds no such result"))?;
        let Value::Object(fields) = &*result else {
            return Err(format!("{path}: not an object"));
        };
        for (key, value) in fields {
            if key != "target" && key != "measured" && read.get(key) != Some(value) {
                return Err(format!("{path}: {key} differs from the pack's"));
            }
        }
        measured += usize::from(read.get("measured").is_some());
        *result = read.clone();
    }
    write_set(root, name, &set)?;
    Ok(format!(
        "{name}: {} results, {measured} with what they measured",
        fresh.len()
    ))
}

/// Rewrite set `name` as kept, returning its old and new keys.
fn promote_set(set: &mut Value, name: &str) -> Result<(String, String), String> {
    let from = s(set, "key").ok_or("set without key")?.to_owned();
    if Retention::of_key(&from) != Some(Retention::Expiring) {
        return Err(format!(
            "{name}: {from} is not under {}",
            Retention::Expiring.prefix()
        ));
    }
    let rest = &from[Retention::Expiring.prefix().len()..];
    let to = format!("{}{rest}", Retention::Kept.prefix());
    *members_mut(set, "key").ok_or("set without key")? = Value::str(to.clone());
    *members_mut(set, "retention").ok_or("set without retention")? =
        Value::str(Retention::Kept.label());
    Ok((from, to))
}

fn members_mut<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    match value {
        Value::Object(members) => members
            .iter_mut()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v),
        _ => None,
    }
}

/// `cargo xtask evidence fetch SET [DIR]`: download the named set, verify its
/// SHA-256 and every file's SHA-256 against the index, and unpack it into DIR
/// (default `.research/evidence/SET`).
pub fn fetch(root: &Path, name: &str, into: Option<&Path>) -> Result<String, String> {
    let set = &set(root, name)?;
    if set
        .get("files")
        .is_some_and(|files| files.as_array().is_none())
    {
        return Err(format!(
            "{name}: `files` must list {{path, sha256}} objects"
        ));
    }
    let target = into.map_or_else(
        || root.join(".research/evidence").join(name),
        Path::to_path_buf,
    );
    let unpacked = unpack(root, name, set, "", &target)?;
    if listed_len(set).is_some_and(|len| len != unpacked) {
        return Err(format!("{name}: archive and index list different files"));
    }
    Ok(format!(
        "{name}: {unpacked} files verified into {}",
        target.display()
    ))
}

/// Where a host without archive credentials downloads kept sets from: lines of a
/// set's name and a presigned address of its pack, which `tools/gate-aws.sh`
/// writes for the sets a gate run may restore ([`restorable`]).
pub const ADDRESSES: &str = ".research/evidence-addresses";

/// The pack of `set` in `.research/evidence-cache`, downloaded through its address
/// in [`ADDRESSES`] or else from the archive with the `aws` CLI, and checked
/// against the SHA-256 the index records.
fn download(root: &Path, set: &Value) -> Result<(PathBuf, Compression), String> {
    let (download, compression) = cache_path(root, set)?;
    let (key, expected) = (
        s(set, "key").ok_or("set without key")?,
        s(set, "sha256").ok_or("set without sha256")?,
    );
    let cache = download.parent().expect("a file of the cache");
    fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    if !download.is_file() {
        let partial = cache.join(format!("{expected}.{}.part", std::process::id()));
        let name = s(set, "name").unwrap_or_default();
        match address(root, name)? {
            Some(url) => curl(&url, &partial)?,
            None => aws_cp(
                &format!("{}/{key}", archive_location()?),
                &partial.display().to_string(),
            )?,
        }
        fs::rename(&partial, &download).map_err(|e| e.to_string())?;
    }
    if sha256::file_hex(&download).map_err(|e| format!("{}: {e}", download.display()))? != expected
    {
        let _ = fs::remove_file(&download);
        return Err(format!("{key}: SHA-256 differs from the index"));
    }
    Ok((download, compression))
}

/// Where [`download`] keeps the pack of `set`, and how it is compressed.
fn cache_path(root: &Path, set: &Value) -> Result<(PathBuf, Compression), String> {
    let (key, expected) = (
        s(set, "key").ok_or("set without key")?,
        s(set, "sha256").ok_or("set without sha256")?,
    );
    let compression =
        Compression::of(key).ok_or_else(|| format!("{key}: not a .tar.gz or .tar.zst"))?;
    let cache = root.join(".research/evidence-cache");
    Ok((
        cache.join(format!("{expected}{}", compression.suffix())),
        compression,
    ))
}

/// The address [`ADDRESSES`] gives set `name`, if the file lists it.
fn address(root: &Path, name: &str) -> Result<Option<String>, String> {
    let path = root.join(ADDRESSES);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    Ok(text.lines().find_map(|line| {
        let (set, url) = line.split_once(' ')?;
        (set == name).then(|| url.trim().to_owned())
    }))
}

fn curl(url: &str, to: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        // A stalled transfer fails, so a restoring step measures instead of waiting.
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--connect-timeout",
            "30",
            "--speed-limit",
            "1048576",
            "--speed-time",
            "60",
            "--max-time",
            "3600",
            "--output",
        ])
        .arg(to)
        .arg(url)
        .status()
        .map_err(|e| format!("curl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        let _ = fs::remove_file(to);
        Err(format!("downloading {}: curl {status}", to.display()))
    }
}

/// `cargo xtask evidence restorable`: the name and key of every kept set listing
/// a result a gate run restores when it reuses it ([`crate::harness::reuse::restored`]),
/// for `tools/gate-aws.sh` to give its hosts an address of each ([`ADDRESSES`]).
pub fn restorable(root: &Path) -> Result<Vec<(String, String)>, String> {
    Ok(sets(root)?
        .iter()
        .filter(|set| s(set, "retention") == Some(Retention::Kept.label()))
        .filter(|set| {
            set.get("results")
                .and_then(Value::as_array)
                .unwrap_or_default()
                .iter()
                .any(reuse::restored)
        })
        .filter_map(|set| Some((s(set, "name")?.to_owned(), s(set, "key")?.to_owned())))
        .collect())
}

/// Restore `directory` of kept set `name` into `into`: every file of the set under
/// that directory (all of them for an empty one), at its path below it, written
/// back to disk before it returns, so no writeback follows into a timed step.
/// Returns how many files it restored; a directory the set lacks is an error.
pub fn restore(root: &Path, name: &str, directory: &str, into: &Path) -> Result<usize, String> {
    let set = set(root, name)?;
    if s(&set, "retention") != Some(Retention::Kept.label()) {
        return Err(format!("{name}: not a kept set"));
    }
    let restored = unpack(root, name, &set, directory, into)?;
    let synced = Command::new("sync")
        .status()
        .map_err(|e| format!("sync: {e}"))?;
    if !synced.success() {
        return Err(format!("sync: {synced}"));
    }
    match restored {
        0 => Err(format!("{name}: no files under {directory}/")),
        restored => Ok(restored),
    }
}

/// Download `set` and write its files under `directory` (all of them for an empty
/// one) into `target`, each at its path below the directory, checking every file
/// the index lists; returns how many it wrote.
fn unpack(
    root: &Path,
    name: &str,
    set: &Value,
    directory: &str,
    target: &Path,
) -> Result<usize, String> {
    let (download, compression) = download(root, set)?;
    let directory = directory.trim_start_matches("./").trim_end_matches('/');
    // A set packed from repository files lists each file; an archived run copied
    // in whole is verified by its archive hash alone.
    let mut tar = TarReader::new(Unpack {
        name,
        listed: set.get("files").and_then(Value::as_array),
        set_path: s(set, "path").unwrap_or_default(),
        prefix: if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        },
        target,
        seen: 0,
        unpacked: 0,
        current: None,
    });
    compression.unpack(&download, &mut tar)?;
    Ok(tar.finish()?.unpacked)
}

fn listed_len(set: &Value) -> Option<usize> {
    set.get("files")
        .and_then(Value::as_array)
        .map(<[Value]>::len)
}

/// Writes each file of a set under `prefix` into `target` as it streams out of
/// the archive, checking it against the index's list when the set has one.
struct Unpack<'a> {
    name: &'a str,
    listed: Option<&'a [Value]>,
    set_path: &'a str,
    /// The directory whose files are written, with its trailing `/`; empty for all.
    prefix: String,
    target: &'a Path,
    /// The files the archive has held so far, written or not: the index's position.
    seen: usize,
    unpacked: usize,
    current: Option<(BufWriter<File>, sha256::Sha256, PathBuf)>,
}

impl Unpack<'_> {
    fn indexed(&self) -> Result<Option<&Value>, String> {
        self.listed
            .map(|listed| {
                listed
                    .get(self.seen)
                    .ok_or_else(|| format!("{}: archive and index list different files", self.name))
            })
            .transpose()
    }
}

impl TarFiles for Unpack<'_> {
    fn begin(&mut self, path: &str, _size: u64) -> Result<(), String> {
        let name = self.name;
        let path = path.trim_start_matches("./");
        if let Some(file) = self.indexed()? {
            let listed = s(file, "path").unwrap_or_default();
            let indexed = listed
                .strip_prefix(self.set_path)
                .and_then(|p| p.strip_prefix('/'))
                .unwrap_or(listed);
            if path != indexed {
                return Err(format!("{name}: {path} differs from the index"));
            }
        }
        let Some(relative) = path.strip_prefix(&self.prefix) else {
            return Ok(());
        };
        if relative.is_empty()
            || Path::new(relative)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{name}: {path} escapes the set"));
        }
        let out = self.target.join(relative);
        fs::create_dir_all(out.parent().expect("file has a parent"))
            .map_err(|e| format!("{}: {e}", out.display()))?;
        let file = File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        self.current = Some((BufWriter::new(file), sha256::Sha256::default(), out));
        Ok(())
    }

    fn data(&mut self, chunk: &[u8]) -> Result<(), String> {
        let Some((file, hash, out)) = self.current.as_mut() else {
            return Ok(());
        };
        hash.update(chunk);
        file.write_all(chunk)
            .map_err(|e| format!("{}: {e}", out.display()))
    }

    fn end(&mut self) -> Result<(), String> {
        if let Some((mut file, hash, out)) = self.current.take() {
            file.flush()
                .map_err(|e| format!("{}: {e}", out.display()))?;
            if let Some(indexed) = self.indexed()?
                && Some(hash.finish_hex().as_str()) != s(indexed, "sha256")
            {
                return Err(format!(
                    "{}: {} differs from the index",
                    self.name,
                    out.strip_prefix(self.target).unwrap_or(&out).display()
                ));
            }
            self.unpacked += 1;
        }
        self.seen += 1;
        Ok(())
    }
}

/// How an archived set is compressed, by its key's extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compression {
    Gzip,
    Zstd,
}

/// The largest tar an archived set may decompress to. Sets are unpacked as they
/// stream, so this bounds disk use, not memory.
const TAR_LIMIT: u64 = 1 << 40;

impl Compression {
    fn of(key: &str) -> Option<Self> {
        if key.ends_with(".tar.gz") {
            Some(Self::Gzip)
        } else if key.ends_with(".tar.zst") {
            Some(Self::Zstd)
        } else {
            None
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::Gzip => ".tar.gz",
            Self::Zstd => ".tar.zst",
        }
    }

    /// Decompress the archive at `path` into `out` as it is read.
    fn unpack(self, path: &Path, out: &mut impl Write) -> Result<(), String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        match self {
            Self::Gzip => {
                crate::archive::gunzip_to(io::BufReader::new(file), out, TAR_LIMIT).map(drop)
            }
            Self::Zstd => unzstd(file, out),
        }
    }
}

/// Decompress a zstd stream with the system `zstd`, refusing more than
/// [`TAR_LIMIT`] bytes of output.
fn unzstd(input: File, out: &mut impl Write) -> Result<(), String> {
    use std::process::Stdio;
    let mut child = Command::new("zstd")
        .args(["--decompress", "--stdout", "--quiet"])
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("zstd: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let copied = io::copy(&mut stdout.take(TAR_LIMIT + 1), out);
    let refuse = |child: &mut std::process::Child, error: String| {
        let _ = child.kill();
        let _ = child.wait();
        Err(error)
    };
    match copied {
        Err(e) => return refuse(&mut child, format!("zstd: {e}")),
        Ok(n) if n > TAR_LIMIT => {
            return refuse(&mut child, format!("zstd: more than {TAR_LIMIT} bytes"));
        }
        Ok(_) => {}
    }
    let output = child.wait_with_output().map_err(|e| format!("zstd: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "zstd failed: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
