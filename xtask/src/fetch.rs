//! Downloads into the ignored `.research/` cache, each verified by a pinned
//! SHA-256: specification texts, required external test vectors and the FFmpeg
//! reference source. Transfers shell out to `curl` and `rsync`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};
use crate::sha256;

/// Network access, replaceable in tests.
pub trait Transport {
    /// GET `url` over HTTPS; at most `limit + 1` bytes are returned.
    fn https(&self, url: &str, agent: &str, limit: usize) -> Result<Vec<u8>, String>;
    /// GET the bytes of a GitHub release asset through the API, which serves them
    /// only to a client asking for `application/octet-stream` and otherwise returns
    /// the asset's metadata.
    fn github_asset(
        &self,
        repo: &str,
        id: u64,
        agent: &str,
        limit: usize,
    ) -> Result<Vec<u8>, String>;
    /// Receive `url` over rsync into `target`, never writing more than `limit` bytes.
    fn rsync(&self, url: &str, target: &Path, limit: usize) -> Result<(), String>;
}

pub struct Network;

impl Transport for Network {
    fn https(&self, url: &str, agent: &str, limit: usize) -> Result<Vec<u8>, String> {
        get(url, None, agent, limit)
    }

    fn github_asset(
        &self,
        repo: &str,
        id: u64,
        agent: &str,
        limit: usize,
    ) -> Result<Vec<u8>, String> {
        let url = format!("https://api.github.com/repos/{repo}/releases/assets/{id}");
        get(&url, Some("application/octet-stream"), agent, limit)
    }

    fn rsync(&self, url: &str, target: &Path, limit: usize) -> Result<(), String> {
        let status = rsync_command(url, target, limit)
            .status()
            .map_err(|e| format!("cannot run rsync: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("rsync {url}: {status}"))
        }
    }
}

/// One bounded HTTPS GET through `curl`, optionally asking for a media type.
fn get(url: &str, accept: Option<&str>, agent: &str, limit: usize) -> Result<Vec<u8>, String> {
    let dir = scratch_dir("download")?;
    let target = dir.join("body");
    let mut command = Command::new("curl");
    if let Some(accept) = accept {
        command.arg("--header").arg(format!("Accept: {accept}"));
    }
    let status = command
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--connect-timeout",
            "30",
        ])
        .args(["--speed-limit", "1", "--speed-time", "60", "--max-filesize"])
        .arg((limit + 1).to_string())
        .args(["--user-agent", agent, "--output"])
        .arg(&target)
        .arg(url)
        .status();
    let result = match status {
        Ok(status) if status.success() => read_bounded(&target, limit),
        Ok(status) => Err(format!("curl {url}: {status}")),
        Err(error) => Err(format!("cannot run curl: {error}")),
    };
    let _ = fs::remove_dir_all(dir);
    result
}

/// The sender's file list can understate what arrives, so the receiver runs
/// under a file-size limit (`ulimit -f` blocks of 512 or 1024 bytes, at least
/// `limit`); the caller still rejects anything over `limit`.
fn rsync_command(url: &str, target: &Path, limit: usize) -> Command {
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "ulimit -f \"$0\" && exec \"$@\"",
            &limit.div_ceil(512).to_string(),
        ])
        .args([
            "rsync",
            "--contimeout=10",
            "--timeout=20",
            &format!("--max-size={limit}"),
            "--",
            url,
        ])
        .arg(target);
    command
}

/// A fresh private directory under the system temporary directory.
fn scratch_dir(purpose: &str) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir =
        std::env::temp_dir().join(format!("triergon-{purpose}-{}-{nanos}", std::process::id()));
    fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Read at most `limit + 1` bytes, so callers can reject oversize files.
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut data = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(data)
}

/// Replace `path` with `data` through a sibling temporary file.
pub fn publish(path: &Path, data: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("partial");
    fs::write(&temporary, data).map_err(|e| format!("{}: {e}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("{}: {e}", path.display())
    })
}

/// The directory named by `TRIERGON_DOWNLOAD_CACHE`, if any: pinned downloads are
/// kept there under their SHA-256, so a later run needs no network for them.
fn download_cache() -> Option<PathBuf> {
    std::env::var_os("TRIERGON_DOWNLOAD_CACHE")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Where a pinned download may be fetched from.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    /// A plain HTTPS URL.
    Url(&'a str),
    /// A GitHub release asset through the API, by repository (`owner/name`) and id.
    GitHubAsset { repo: &'a str, id: u64 },
}

impl Source<'_> {
    fn fetch(self, net: &dyn Transport, agent: &str, limit: usize) -> Result<Vec<u8>, String> {
        match self {
            Source::Url(url) => net.https(url, agent, limit),
            Source::GitHubAsset { repo, id } => net.github_asset(repo, id, agent, limit),
        }
    }
}

impl std::fmt::Display for Source<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Url(url) => f.write_str(url),
            Source::GitHubAsset { repo, id } => write!(f, "GitHub asset {repo}#{id}"),
        }
    }
}

/// The bytes whose SHA-256 is `sha256`, at most `limit` of them: from the download
/// cache if it holds them, otherwise from the first of `sources` that serves them.
/// Every candidate is verified, so a stale cache entry or a mirror serving other
/// bytes is passed over, never used.
pub fn pinned(
    net: &dyn Transport,
    sources: &[Source<'_>],
    sha256: &str,
    limit: usize,
    agent: &str,
) -> Result<Vec<u8>, String> {
    pinned_in(
        net,
        download_cache().as_deref(),
        sources,
        sha256,
        limit,
        agent,
    )
}

fn pinned_in(
    net: &dyn Transport,
    cache: Option<&Path>,
    sources: &[Source<'_>],
    sha256: &str,
    limit: usize,
    agent: &str,
) -> Result<Vec<u8>, String> {
    let matches = |data: &[u8]| data.len() <= limit && sha256::hex(data) == sha256;
    if let Some(data) = cache.and_then(|dir| cached(dir, sha256, limit, matches)) {
        return Ok(data);
    }
    let mut failures = Vec::new();
    for &source in sources {
        match source.fetch(net, agent, limit) {
            Ok(data) if matches(&data) => {
                store_download(cache, sha256, &data);
                return Ok(data);
            }
            Ok(data) if data.len() > limit => {
                failures.push(format!("{source}: over {limit} bytes"));
            }
            Ok(_) => failures.push(format!("{source}: not the pinned SHA-256 {sha256}")),
            Err(error) => failures.push(format!("{source}: {error}")),
        }
    }
    Err(format!(
        "no source served the pinned download {sha256}: {}",
        failures.join("; ")
    ))
}

fn store_download(cache: Option<&Path>, sha256: &str, data: &[u8]) {
    if let Some(dir) = cache {
        let stored = fs::create_dir_all(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))
            .and_then(|()| publish(&dir.join(sha256), data));
        if let Err(error) = stored {
            eprintln!("download cache not updated: {error}");
        }
    }
}

/// A verified cache entry, marked as used by this run so a pruned cache keeps it.
/// An entry that fails verification is removed and the caller downloads afresh.
fn cached(
    dir: &Path,
    sha256: &str,
    limit: usize,
    matches: impl Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    let path = dir.join(sha256);
    let data = read_bounded(&path, limit).ok()?;
    if !matches(&data) {
        let _ = fs::remove_file(&path);
        return None;
    }
    let _ = fs::File::options()
        .append(true)
        .open(&path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()));
    Some(data)
}

// ---------------------------------------------------------------------------
// Specification texts.

pub const SPECS: [(&str, &str, &str); 7] = [
    (
        "rfc8794",
        "https://www.rfc-editor.org/rfc/rfc8794.txt",
        "EBML normative publication",
    ),
    (
        "rfc9559",
        "https://www.rfc-editor.org/rfc/rfc9559.txt",
        "Matroska normative publication; updates RFC 8794",
    ),
    (
        "matroska-schema",
        "https://raw.githubusercontent.com/ietf-wg-cellar/matroska-specification/f93ab02b845ec2a6a2fe4dce5f4c81ad21c34b4c/ebml_matroska.xml",
        "Pinned current element schema; differences from RFC require explicit disposition",
    ),
    (
        "matroska-notes",
        "https://www.matroska.org/technical/notes.html",
        "Explanatory timestamp/block/lacing notes; moving page frozen by content hash",
    ),
    (
        "matroska-codecs",
        "https://www.matroska.org/technical/codec_specs.html",
        "Codec carriage mappings; moving page frozen by content hash",
    ),
    (
        "rfc1950",
        "https://www.rfc-editor.org/rfc/rfc1950.txt",
        "zlib wrapper",
    ),
    (
        "rfc1951",
        "https://www.rfc-editor.org/rfc/rfc1951.txt",
        "DEFLATE stored/fixed/dynamic blocks",
    ),
];

const SPEC_LIMIT: usize = 4 * 1024 * 1024;

/// Cache every specification text and verify it against `specs/sources.json`;
/// with `record`, rewrite the manifest from what is cached instead.
pub fn specs(root: &Path, record: bool, net: &dyn Transport) -> Result<String, String> {
    let cache = root.join(".research/specs");
    fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let manifest = root.join("specs/sources.json");
    let expected = if record {
        Value::Null
    } else {
        json::read(&manifest)?
    };
    let pinned = |id: &str| {
        expected
            .get("sources")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|s| s.get("sha256"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    if !record {
        let ids: Vec<&str> = expected
            .get("sources")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.get("id").and_then(Value::as_str))
            .collect();
        let mut listed: Vec<&str> = ids.clone();
        listed.sort_unstable();
        listed.dedup();
        let mut catalog: Vec<&str> = SPECS.iter().map(|s| s.0).collect();
        catalog.sort_unstable();
        if listed != catalog || ids.len() != listed.len() {
            return Err("specification manifest and fetch catalog differ".into());
        }
    }
    let mut rows = Vec::new();
    for (id, url, role) in SPECS {
        let path = cache.join(id);
        if !path.exists() {
            let data = net.https(url, "triergon-specification-cache", SPEC_LIMIT)?;
            if data.len() > SPEC_LIMIT {
                return Err("specification size limit".into());
            }
            fs::write(&path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let sha = sha256::hex(&data);
        if !record && pinned(id).as_deref() != Some(sha.as_str()) {
            return Err(format!(
                "{id}: specification drift; review before updating pin"
            ));
        }
        rows.push(Value::object([
            ("id", Value::str(id)),
            ("url", Value::str(url)),
            ("sha256", Value::str(sha)),
            ("bytes", Value::int(data.len() as i64)),
            ("role", Value::str(role)),
            ("cached_path", Value::str(format!(".research/specs/{id}"))),
        ]));
    }
    let count = rows.len();
    if record {
        let document = Value::object([
            ("schema", Value::int(1)),
            ("recorded", Value::str(today())),
            ("sources", Value::Array(rows)),
        ]);
        json::write(&manifest, &document)?;
    }
    Ok(format!("Verified {count} specification snapshots"))
}

/// Today's UTC date as `YYYY-MM-DD`.
fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400);
    // Civil-from-days (proleptic Gregorian), for days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

// ---------------------------------------------------------------------------
// Required external vectors.

pub const VECTOR_LIMIT: usize = 8 * 1024 * 1024;
const VECTOR_CONTRACT: &str = "harness/features/matroska-segment-metadata/contract.json";

/// Fetch and verify the external vectors needed by the workspace check.
pub fn vectors(root: &Path, net: &dyn Transport, limit: usize) -> Result<Vec<String>, String> {
    let cache = download_cache();
    let mut verified = required_vectors(root, net, cache.as_deref(), limit)?;
    verified.extend(fate_corpus_vectors(root, net, cache.as_deref(), limit)?);
    Ok(verified)
}

/// A vector `fetch-vectors` fetches with its pin: the repository-relative
/// directory it goes to, its file there and its SHA-256.
pub struct Pin {
    pub directory: String,
    pub file: String,
    pub sha256: String,
}

/// Every vector `fetch-vectors` fetches, without fetching any.
pub fn pins(root: &Path) -> Result<Vec<Pin>, String> {
    let mut pins = required_pins(root)?;
    pins.extend(fate_corpus_pins(root)?);
    Ok(pins)
}

fn required_vectors(
    root: &Path,
    net: &dyn Transport,
    cache: Option<&Path>,
    limit: usize,
) -> Result<Vec<String>, String> {
    fetch_pins(root, required_pins(root)?, net, cache, limit)
}

/// Fetch the FATE cases of registered corpora that request them.
fn fate_corpus_vectors(
    root: &Path,
    net: &dyn Transport,
    cache: Option<&Path>,
    limit: usize,
) -> Result<Vec<String>, String> {
    fetch_pins(root, fate_corpus_pins(root)?, net, cache, limit)
}

fn fetch_pins(
    root: &Path,
    pins: Vec<Pin>,
    net: &dyn Transport,
    cache: Option<&Path>,
    limit: usize,
) -> Result<Vec<String>, String> {
    let mut verified = Vec::new();
    for pin in pins {
        fetch_vector(
            &root.join(&pin.directory),
            &pin.file,
            &pin.sha256,
            net,
            cache,
            limit,
        )?;
        verified.push(pin.file);
    }
    Ok(verified)
}

/// The mandatory external vectors, in the sample cache.
fn required_pins(root: &Path) -> Result<Vec<Pin>, String> {
    let contract = json::read(&root.join(VECTOR_CONTRACT))?;
    let mut pins = Vec::new();
    for vector in contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let file = vector
            .get("file")
            .and_then(Value::as_str)
            .ok_or("required vector without a file")?;
        let sha256 = vector
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or("required vector without a hash")?;
        pins.push(Pin {
            directory: ".research/fate-samples".into(),
            file: file.to_owned(),
            sha256: sha256.to_owned(),
        });
    }
    Ok(pins)
}

/// The FATE cases of registered corpora that request them, once each.
fn fate_corpus_pins(root: &Path) -> Result<Vec<Pin>, String> {
    let mut all = Vec::new();
    for contract in crate::harness::features::discover(root)? {
        if contract.document.get("fetch_fate_cases") != Some(&Value::Bool(true)) {
            continue;
        }
        let manifest = json::read(&contract.manifest())?;
        let directory = manifest
            .get("directory")
            .and_then(Value::as_str)
            .filter(|path| path.starts_with(".research/") && relative_path(path))
            .ok_or_else(|| format!("{}: invalid fetched corpus directory", contract.id))?;
        let mut pins = BTreeMap::new();
        for case in manifest
            .get("cases")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let Some(relative) = case
                .get("source")
                .and_then(Value::as_str)
                .and_then(|source| source.strip_prefix("fate-suite/"))
            else {
                continue;
            };
            if case.get("file").and_then(Value::as_str) != Some(relative) {
                return Err(format!(
                    "{}: FATE source and input paths differ",
                    contract.id
                ));
            }
            let expected = case
                .get("sha256")
                .and_then(Value::as_str)
                .ok_or("required vector without a hash")?;
            if pins
                .insert(relative, expected)
                .is_some_and(|sha| sha != expected)
            {
                return Err(format!(
                    "{}: conflicting FATE hashes for {relative}",
                    contract.id
                ));
            }
        }
        all.extend(pins.into_iter().map(|(file, sha256)| Pin {
            directory: directory.to_owned(),
            file: file.to_owned(),
            sha256: sha256.to_owned(),
        }));
    }
    Ok(all)
}

/// The FATE sample at `relative` below `directory`: fetched by its pin when it is
/// absent, verified when it is there.
pub fn fate_sample(directory: &Path, relative: &str, sha256: &str) -> Result<(), String> {
    fetch_vector(
        directory,
        relative,
        sha256,
        &Network,
        download_cache().as_deref(),
        VECTOR_LIMIT,
    )
}

fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn fetch_vector(
    directory: &Path,
    relative: &str,
    expected: &str,
    net: &dyn Transport,
    cache: Option<&Path>,
    limit: usize,
) -> Result<(), String> {
    if !relative_path(relative) {
        return Err("invalid required vector path".into());
    }
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid required vector hash".into());
    }
    let path = directory.join(relative);
    if path.exists() {
        let data = read_bounded(&path, limit)?;
        if data.len() > limit {
            return Err("cached required vector exceeds download bound".into());
        }
        if sha256::hex(&data) != expected {
            return Err(format!("cached required vector changed: {relative}"));
        }
        store_download(cache, expected, &data);
        return Ok(());
    }
    let matches = |bytes: &[u8]| bytes.len() <= limit && sha256::hex(bytes) == expected;
    let data = if let Some(data) = cache.and_then(|dir| cached(dir, expected, limit, matches)) {
        data
    } else {
        let data = download_vector(relative, net, limit)?;
        if data.len() > limit {
            return Err("required vector exceeds download bound".into());
        }
        if !matches(&data) {
            return Err(format!(
                "required vector download hash mismatch: {relative}"
            ));
        }
        store_download(cache, expected, &data);
        data
    };
    let parent = path.parent().expect("vector inside the cache");
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    publish(&path, &data)
}

/// HTTPS first; FFmpeg also serves the FATE suite over rsync. The caller checks
/// the same digest whichever transport delivered the bytes.
fn download_vector(relative: &str, net: &dyn Transport, limit: usize) -> Result<Vec<u8>, String> {
    let https_error = match net.https(
        &format!("https://fate-suite.ffmpeg.org/{relative}"),
        "triergon-required-test-vectors",
        limit,
    ) {
        Ok(data) => return Ok(data),
        Err(error) => error,
    };
    let dir = scratch_dir("fate")?;
    let target = dir.join("vector");
    let result = net
        .rsync(
            &format!("rsync://fate-suite.ffmpeg.org/fate-suite/{relative}"),
            &target,
            limit,
        )
        .and_then(|()| read_bounded(&target, limit));
    let _ = fs::remove_dir_all(&dir);
    result.map_err(|error| {
        format!("could not fetch required vector {relative} over HTTPS ({https_error}) or rsync ({error})")
    })
}

#[cfg(test)]
mod tests;
