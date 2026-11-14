//! Raw evidence kept outside the repository: deterministic packing, uploading,
//! promoting and fetching the sets named in `harness/evidence/index.json`, in the
//! archive that `TRIERGON_EVIDENCE_ARCHIVE` names (an `s3://` location reached
//! with the `aws` CLI).
//!
//! A pack is a `.tar.gz` whose bytes depend only on the files: entries sorted by
//! path, zero timestamps and owners, and gzip stored blocks with no name or time.
//! A set archived whole may instead be a `.tar.zst`, decompressed with the
//! system `zstd`.
//!
//! Every upload lands under `qualification/`, which the archive expires 90 days
//! after upload. A set moves under `evidence/`, which is kept, only when a result
//! cites it, so nothing is kept that nothing cites. A set's index entry lists the
//! gate results it holds, which later gate runs reuse once it is kept.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::archive::{TarFiles, TarReader};
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

/// `cargo xtask evidence pack DIR OUT`: write the pack and print its SHA-256. The
/// pack is written to `OUT.part` and renamed once complete.
pub fn pack_command(dir: &Path, out: &Path) -> Result<String, String> {
    let files = files(dir)?;
    let mut partial = out.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let name = partial.display().to_string();
    let file = File::create(&partial).map_err(|e| format!("{name}: {e}"))?;
    let hashed = Hashed {
        out: BufWriter::new(file),
        hash: sha256::Sha256::default(),
    };
    let packed = pack_files(&files, hashed, &name).and_then(
        |Hashed {
             out: mut written,
             hash,
         }| {
            written.flush().map_err(|e| format!("{name}: {e}"))?;
            Ok(hash.finish_hex())
        },
    );
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
pub fn upload(name: &str, archive: &Path) -> Result<String, String> {
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
    let key = format!(
        "{}{name}/{sha}{}",
        Retention::Expiring.prefix(),
        compression.suffix()
    );
    let results = gate_results(archive, compression)?;
    let location = archive_location()?;
    aws_cp(&archive.display().to_string(), &format!("{location}/{key}"))?;
    let mut entry = vec![
        ("name", Value::str(name)),
        ("path", Value::str(name)),
        ("retention", Value::str(Retention::Expiring.label())),
        ("reason", Value::str("")),
        ("key", Value::str(key)),
        ("sha256", Value::str(sha)),
        ("bytes", Value::int(bytes)),
    ];
    if !results.is_empty() {
        entry.push(("results", Value::Array(results)));
    }
    let entry = Value::object(entry);
    Ok(crate::json::to_python(&entry, Some(1)))
}

/// The fresh gate results a packed set holds ([`crate::harness::reuse`]), which a
/// later gate run reuses once the set is kept.
fn gate_results(archive: &Path, compression: Compression) -> Result<Vec<Value>, String> {
    struct Results {
        entries: Vec<(String, Vec<u8>)>,
        current: bool,
    }
    impl TarFiles for Results {
        fn begin(&mut self, path: &str, _: u64) -> Result<(), String> {
            self.current = path.rsplit('/').next() == Some(crate::harness::reuse::RESULT);
            if self.current {
                self.entries
                    .push((path.trim_start_matches("./").to_owned(), Vec::new()));
            }
            Ok(())
        }
        fn data(&mut self, chunk: &[u8]) -> Result<(), String> {
            if self.current {
                self.entries
                    .last_mut()
                    .expect("begun")
                    .1
                    .extend_from_slice(chunk);
            }
            Ok(())
        }
        fn end(&mut self) -> Result<(), String> {
            self.current = false;
            Ok(())
        }
    }
    let mut tar = TarReader::new(Results {
        entries: Vec::new(),
        current: false,
    });
    compression.unpack(archive, &mut tar)?;
    crate::harness::reuse::listed(&tar.finish()?.entries)
}

/// `cargo xtask evidence promote SET`: copy an indexed set from `qualification/`
/// to `evidence/` inside the archive, then record the new key and retention in
/// the index. Promote a set when a result cites it.
pub fn promote(root: &Path, name: &str) -> Result<String, String> {
    let path = root.join(INDEX);
    let text = fs::read_to_string(&path).map_err(|e| format!("{INDEX}: {e}"))?;
    let mut index = crate::json::parse_strict(&text).map_err(|e| format!("{INDEX}: {e}"))?;
    if crate::json::to_python(&index, Some(1)) + "\n" != text {
        return Err(format!("{INDEX} is not in its canonical layout"));
    }
    let (from, to) = promote_entry(&mut index, name)?;
    let location = archive_location()?;
    aws_cp(&format!("{location}/{from}"), &format!("{location}/{to}"))?;
    fs::write(&path, crate::json::to_python(&index, Some(1)) + "\n")
        .map_err(|e| format!("{INDEX}: {e}"))?;
    Ok(format!("{name}: {from} -> {to}"))
}

/// Rewrite set `name` in the index as kept, returning its old and new keys.
fn promote_entry(index: &mut Value, name: &str) -> Result<(String, String), String> {
    let Some(Value::Array(sets)) = members_mut(index, "sets") else {
        return Err(format!("{INDEX} has no sets"));
    };
    let set = sets
        .iter_mut()
        .find(|set| s(set, "name") == Some(name))
        .ok_or_else(|| format!("{name} is not in {INDEX}"))?;
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
    let index = crate::checks::read_json(&root.join(INDEX))?;
    let set = index
        .get("sets")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .find(|set| s(set, "name") == Some(name))
        .ok_or_else(|| format!("{name} is not in {INDEX}"))?;
    if set
        .get("files")
        .is_some_and(|files| files.as_array().is_none())
    {
        return Err(format!(
            "{name}: `files` must list {{path, sha256}} objects"
        ));
    }
    let (key, expected) = (
        s(set, "key").ok_or("set without key")?,
        s(set, "sha256").ok_or("set without sha256")?,
    );
    let extension =
        Compression::of(key).ok_or_else(|| format!("{key}: not a .tar.gz or .tar.zst"))?;
    let archive = archive_location()?;
    let cache = root.join(".research/evidence-cache");
    fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let download = cache.join(format!("{expected}{}", extension.suffix()));
    if !download.is_file() {
        let partial = cache.join(format!("{expected}.part"));
        aws_cp(&format!("{archive}/{key}"), &partial.display().to_string())?;
        fs::rename(&partial, &download).map_err(|e| e.to_string())?;
    }
    if sha256::file_hex(&download).map_err(|e| format!("{}: {e}", download.display()))? != expected
    {
        let _ = fs::remove_file(&download);
        return Err(format!("{key}: SHA-256 differs from the index"));
    }
    let target = into.map_or_else(
        || root.join(".research/evidence").join(name),
        Path::to_path_buf,
    );
    // A set packed from repository files lists each file; an archived run copied
    // in whole is verified by its archive hash alone.
    let mut tar = TarReader::new(Unpack {
        name,
        listed: set.get("files").and_then(Value::as_array),
        set_path: s(set, "path").unwrap_or_default(),
        target: &target,
        unpacked: 0,
        current: None,
    });
    extension.unpack(&download, &mut tar)?;
    let unpacked = tar.finish()?.unpacked;
    if listed_len(set).is_some_and(|len| len != unpacked) {
        return Err(format!("{name}: archive and index list different files"));
    }
    Ok(format!(
        "{name}: {unpacked} files verified into {}",
        target.display()
    ))
}

fn listed_len(set: &Value) -> Option<usize> {
    set.get("files")
        .and_then(Value::as_array)
        .map(<[Value]>::len)
}

/// Writes each file of a fetched set under `target` as it streams out of the
/// archive, checking it against the index's list when the set has one.
struct Unpack<'a> {
    name: &'a str,
    listed: Option<&'a [Value]>,
    set_path: &'a str,
    target: &'a Path,
    unpacked: usize,
    current: Option<(BufWriter<File>, sha256::Sha256, PathBuf)>,
}

impl Unpack<'_> {
    fn indexed(&self) -> Result<Option<&Value>, String> {
        self.listed
            .map(|listed| {
                listed
                    .get(self.unpacked)
                    .ok_or_else(|| format!("{}: archive and index list different files", self.name))
            })
            .transpose()
    }
}

impl TarFiles for Unpack<'_> {
    fn begin(&mut self, relative: &str, _size: u64) -> Result<(), String> {
        let name = self.name;
        if let Some(file) = self.indexed()? {
            let path = s(file, "path").unwrap_or_default();
            let indexed = path
                .strip_prefix(self.set_path)
                .and_then(|p| p.strip_prefix('/'))
                .unwrap_or(path);
            if relative != indexed {
                return Err(format!("{name}: {relative} differs from the index"));
            }
        }
        if Path::new(relative)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{name}: {relative} escapes the set"));
        }
        let out = self.target.join(relative);
        fs::create_dir_all(out.parent().expect("file has a parent"))
            .map_err(|e| format!("{}: {e}", out.display()))?;
        let file = File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        self.current = Some((BufWriter::new(file), sha256::Sha256::default(), out));
        Ok(())
    }

    fn data(&mut self, chunk: &[u8]) -> Result<(), String> {
        let (file, hash, out) = self.current.as_mut().expect("a file is open");
        hash.update(chunk);
        file.write_all(chunk)
            .map_err(|e| format!("{}: {e}", out.display()))
    }

    fn end(&mut self) -> Result<(), String> {
        let (mut file, hash, out) = self.current.take().expect("a file is open");
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
mod tests {
    use super::*;

    fn pack<W: Write>(dir: &Path, out: W) -> Result<W, String> {
        pack_files(&files(dir)?, out, "the pack")
    }

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
        let first = pack(&dir, Vec::new()).unwrap();
        assert_eq!(first, pack(&dir, Vec::new()).unwrap());
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

    /// The pack of a fixed tree, byte for byte: stored gzip blocks, a pax record,
    /// a ustar prefix, a hidden file, an empty file and an executable one.
    #[test]
    fn pack_bytes_are_fixed() {
        let dir = crate::walk::scratch("evidence-golden");
        let long = "a".repeat(120);
        let paxed = format!("d/{}", "e".repeat(150));
        let tree: [(&str, Vec<u8>, u32); 6] = [
            ("x/.hidden/f.json", b"{}\n".to_vec(), 0o644),
            (
                "b.txt",
                (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect(),
                0o644,
            ),
            (&format!("{long}/{long}.json"), b"long".to_vec(), 0o644),
            ("empty", Vec::new(), 0o644),
            ("tool.sh", b"#!/bin/sh\n".to_vec(), 0o755),
            (&paxed, b"pax".to_vec(), 0o644),
        ];
        for (path, data, mode) in &tree {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, data).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&file, fs::Permissions::from_mode(*mode)).unwrap();
            }
            #[cfg(not(unix))]
            let _ = mode;
        }
        let out = dir.with_extension("tar.gz");
        let digest = pack_command(&dir, &out).unwrap();
        assert_eq!(sha256::file_hex(&out).unwrap(), digest);
        #[cfg(unix)]
        assert_eq!(
            digest,
            "4a08933099a9a4b3771bee9e2223824efd7b39413ca17547e4e60fcfafce4fcd"
        );
        fs::remove_file(out).unwrap();

        // A missing directory fails before OUT exists.
        let missing = dir.join("missing");
        let target = dir.join("never.tar.gz");
        let error = pack_command(&missing, &target).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        assert!(!target.exists() && !dir.join("never.tar.gz.part").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zstd_sets_unpack_through_the_system_zstd() {
        let dir = crate::walk::scratch("evidence-zstd");
        fs::create_dir_all(dir.join("in/a")).unwrap();
        fs::write(dir.join("in/a/f.json"), b"{}\n").unwrap();
        fs::write(dir.join("in/b.bin"), vec![3u8; 70_000]).unwrap();
        let tar =
            crate::archive::gunzip(&pack(&dir.join("in"), Vec::new()).unwrap(), 1 << 30).unwrap();
        fs::write(dir.join("set.tar"), &tar).unwrap();
        let status = Command::new("zstd")
            .args(["--quiet", "-19"])
            .arg(dir.join("set.tar"))
            .arg("-o")
            .arg(dir.join("set.tar.zst"))
            .status()
            .expect("zstd is installed");
        assert!(status.success());
        assert_eq!(Compression::of("x/abc.tar.zst"), Some(Compression::Zstd));
        assert_eq!(Compression::of("x/abc.tar.gz"), Some(Compression::Gzip));
        assert_eq!(Compression::of("x/abc.tar.xz"), None);
        let mut unpacked = Vec::new();
        Compression::Zstd
            .unpack(&dir.join("set.tar.zst"), &mut unpacked)
            .unwrap();
        assert_eq!(unpacked, tar);
        let entries = crate::archive::tar_files(&unpacked).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, ["a/f.json", "b.bin"]);
        fs::write(dir.join("bad.tar.zst"), b"not zstd").unwrap();
        assert!(
            Compression::Zstd
                .unpack(&dir.join("bad.tar.zst"), &mut Vec::new())
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stored_gzip_blocks_split_at_the_block_size_whatever_the_writes() {
        for len in [0, 1, STORED_BLOCK, STORED_BLOCK + 1, 2 * STORED_BLOCK] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            let mut expected = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
            let blocks: Vec<&[u8]> = data.chunks(STORED_BLOCK).collect();
            for (at, block) in blocks.iter().enumerate() {
                let n = block.len() as u16;
                expected.push(u8::from(at + 1 == blocks.len()));
                expected.extend(n.to_le_bytes());
                expected.extend((!n).to_le_bytes());
                expected.extend(*block);
            }
            if blocks.is_empty() {
                expected.extend([1, 0, 0, 0xff, 0xff]);
            }
            expected.extend(crate::zlib::crc32(&data).to_le_bytes());
            expected.extend((len as u32).to_le_bytes());
            let mut gzip = GzipStored::new(Vec::new()).unwrap();
            for piece in data.chunks(4_099) {
                gzip.write_all(piece).unwrap();
            }
            let streamed = gzip.finish().unwrap();
            assert_eq!(streamed, expected, "{len} bytes");
            assert_eq!(crate::archive::gunzip(&streamed, 1 << 20).unwrap(), data);
        }
    }

    #[test]
    fn promotion_moves_an_expiring_set_under_evidence() {
        let mut index = crate::json::parse_strict(
            r#"{"sets": [
                {"name": "a", "retention": "90 days", "key": "qualification/a/0.tar.gz"},
                {"name": "b", "retention": "retained", "key": "evidence/b/1.tar.gz"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            promote_entry(&mut index, "a").unwrap(),
            (
                "qualification/a/0.tar.gz".into(),
                "evidence/a/0.tar.gz".into()
            )
        );
        let a = &index.get("sets").and_then(Value::as_array).unwrap()[0];
        assert_eq!(s(a, "key"), Some("evidence/a/0.tar.gz"));
        assert_eq!(s(a, "retention"), Some("retained"));
        assert!(
            promote_entry(&mut index, "a")
                .unwrap_err()
                .contains("not under")
        );
        assert!(
            promote_entry(&mut index, "b")
                .unwrap_err()
                .contains("not under")
        );
        assert!(
            promote_entry(&mut index, "c")
                .unwrap_err()
                .contains("not in")
        );
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
        assert_eq!(
            crate::json::to_python(&index, Some(1)) + "\n",
            fs::read_to_string(root.join(INDEX)).unwrap(),
            "{INDEX} must stay in its canonical layout, which `evidence promote` rewrites"
        );
        let mut paths = std::collections::BTreeSet::new();
        for set in index.get("sets").and_then(Value::as_array).unwrap() {
            let sha = s(set, "sha256").unwrap();
            assert!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
            let key = s(set, "key").unwrap();
            assert!(
                [".tar.gz", ".tar.zst"]
                    .iter()
                    .any(|ext| key.ends_with(&format!("{sha}{ext}")))
            );
            let retention = Retention::of_key(key)
                .unwrap_or_else(|| panic!("{key}: neither under qualification/ nor evidence/"));
            assert_eq!(
                s(set, "retention"),
                Some(retention.label()),
                "{key}: retention must follow the prefix"
            );
            let files = set.get("files");
            assert!(
                files.is_none_or(|files| files.as_array().is_some()),
                "{}: `files` must list {{path, sha256}} objects",
                s(set, "name").unwrap()
            );
            for file in files.and_then(Value::as_array).unwrap_or_default() {
                assert!(
                    s(file, "sha256").is_some(),
                    "{} has no sha256",
                    s(file, "path").unwrap()
                );
                let path = s(file, "path").unwrap();
                assert!(paths.insert(path.to_owned()), "{path} listed twice");
                if path.starts_with("research/") {
                    assert!(!root.join(path).exists(), "{path} is still tracked");
                }
            }
        }
    }
}
