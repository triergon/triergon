//! Downloads into the ignored `.research/` cache, each verified by a pinned
//! SHA-256: specification texts, required external test vectors and the FFmpeg
//! reference source. Transfers shell out to `curl` and `rsync`.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::json::Value;
use crate::sha256;

/// Network access, replaceable in tests.
pub trait Transport {
    /// GET `url` over HTTPS; at most `limit + 1` bytes are returned.
    fn https(&self, url: &str, agent: &str, limit: usize) -> Result<Vec<u8>, String>;
    /// Receive `url` over rsync into `target`, never writing more than `limit` bytes.
    fn rsync(&self, url: &str, target: &Path, limit: usize) -> Result<(), String>;
}

pub struct Network;

impl Transport for Network {
    fn https(&self, url: &str, agent: &str, limit: usize) -> Result<Vec<u8>, String> {
        let dir = scratch_dir("download")?;
        let target = dir.join("body");
        let status = Command::new("curl")
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
        crate::checks::read_json(&manifest)?
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
        crate::corpus::write_json(&manifest, &document)?;
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
const VECTOR_CONTRACT: &str = "harness/catalogue-slice.json";

/// Fetch every required vector named by the metadata contract into
/// `.research/fate-samples/`, verifying cached copies; returns their paths.
pub fn required_vectors(
    root: &Path,
    net: &dyn Transport,
    limit: usize,
) -> Result<Vec<String>, String> {
    let cache = root.join(".research/fate-samples");
    let contract = crate::checks::read_json(&root.join(VECTOR_CONTRACT))?;
    let mut verified = Vec::new();
    for vector in contract
        .get("required_additional_vectors")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let relative = vector
            .get("file")
            .and_then(Value::as_str)
            .ok_or("required vector without a file")?;
        let expected = vector
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or("required vector without a hash")?;
        let parts = Path::new(relative).components();
        if relative.contains('\\')
            || parts.clone().any(|c| !matches!(c, Component::Normal(_)))
            || relative.is_empty()
        {
            return Err("invalid required vector path".into());
        }
        let path = cache.join(relative);
        if path.exists() {
            let data = read_bounded(&path, limit)?;
            if data.len() > limit {
                return Err("cached required vector exceeds download bound".into());
            }
            if sha256::hex(&data) != expected {
                return Err(format!("cached required vector changed: {relative}"));
            }
        } else {
            let parent = path.parent().expect("vector inside the cache");
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            let data = download_vector(relative, net, limit)?;
            if data.len() > limit {
                return Err("required vector exceeds download bound".into());
            }
            if sha256::hex(&data) != expected {
                return Err(format!(
                    "required vector download hash mismatch: {relative}"
                ));
            }
            publish(&path, &data)?;
        }
        verified.push(relative.to_owned());
    }
    Ok(verified)
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
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A transport that serves fixed answers and records what was asked.
    #[derive(Default)]
    struct Fake {
        https: Option<Vec<u8>>,
        rsync: Option<Vec<u8>>,
        calls: RefCell<Vec<String>>,
    }

    impl Transport for Fake {
        fn https(&self, url: &str, _: &str, _: usize) -> Result<Vec<u8>, String> {
            self.calls.borrow_mut().push(url.to_owned());
            self.https.clone().ok_or_else(|| "timed out".to_owned())
        }

        fn rsync(&self, url: &str, target: &Path, _: usize) -> Result<(), String> {
            self.calls.borrow_mut().push(url.to_owned());
            let data = self.rsync.clone().ok_or("connection refused")?;
            fs::write(target, data).map_err(|e| e.to_string())
        }
    }

    fn contract(root: &Path, file: &str, content: &[u8]) {
        fs::create_dir_all(root.join("harness")).unwrap();
        let text = format!(
            r#"{{"required_additional_vectors": [{{"file": "{file}", "sha256": "{}"}}]}}"#,
            sha256::hex(content)
        );
        fs::write(root.join(VECTOR_CONTRACT), text).unwrap();
    }

    const VECTOR: &str = ".research/fate-samples/mkv/required.mkv";

    #[test]
    fn cached_original_is_verified_without_network() {
        let root = crate::walk::scratch("vectors");
        contract(&root, "mkv/required.mkv", b"original vector");
        fs::create_dir_all(root.join(".research/fate-samples/mkv")).unwrap();
        fs::write(root.join(VECTOR), b"original vector").unwrap();
        let net = Fake::default();
        assert_eq!(
            required_vectors(&root, &net, VECTOR_LIMIT).unwrap(),
            ["mkv/required.mkv"]
        );
        assert!(net.calls.borrow().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_cache_is_preserved_and_refused() {
        let root = crate::walk::scratch("vectors");
        contract(&root, "mkv/required.mkv", b"original vector");
        fs::create_dir_all(root.join(".research/fate-samples/mkv")).unwrap();
        fs::write(root.join(VECTOR), b"changed").unwrap();
        let error = required_vectors(&root, &Fake::default(), VECTOR_LIMIT).unwrap_err();
        assert!(error.contains("cached required vector changed"), "{error}");
        assert_eq!(fs::read(root.join(VECTOR)).unwrap(), b"changed");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrong_downloads_are_never_published() {
        for net in [
            Fake {
                https: Some(b"wrong".to_vec()),
                ..Fake::default()
            },
            Fake {
                rsync: Some(b"wrong".to_vec()),
                ..Fake::default()
            },
        ] {
            let root = crate::walk::scratch("vectors");
            contract(&root, "mkv/required.mkv", b"original vector");
            let error = required_vectors(&root, &net, VECTOR_LIMIT).unwrap_err();
            assert!(error.contains("download hash mismatch"), "{error}");
            assert!(!root.join(VECTOR).exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn https_failure_falls_back_to_the_exact_rsync_path() {
        let root = crate::walk::scratch("vectors");
        contract(&root, "mkv/required.mkv", b"original vector");
        let net = Fake {
            rsync: Some(b"original vector".to_vec()),
            ..Fake::default()
        };
        assert_eq!(
            required_vectors(&root, &net, VECTOR_LIMIT).unwrap(),
            ["mkv/required.mkv"]
        );
        assert_eq!(
            *net.calls.borrow(),
            [
                "https://fate-suite.ffmpeg.org/mkv/required.mkv",
                "rsync://fate-suite.ffmpeg.org/fate-suite/mkv/required.mkv"
            ]
        );
        assert_eq!(fs::read(root.join(VECTOR)).unwrap(), b"original vector");
        let error = required_vectors(&root, &Fake::default(), VECTOR_LIMIT);
        assert!(error.is_ok(), "the published copy is verified from cache");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversize_and_failed_transfers_are_refused() {
        let root = crate::walk::scratch("vectors");
        contract(&root, "mkv/required.mkv", &[0; 2048]);
        let net = Fake {
            rsync: Some(vec![0; 2048]),
            ..Fake::default()
        };
        assert!(
            required_vectors(&root, &net, 1024)
                .unwrap_err()
                .contains("exceeds download bound")
        );
        let error = required_vectors(&root, &Fake::default(), 1024).unwrap_err();
        assert!(error.contains("could not fetch required vector"), "{error}");
        assert!(!root.join(VECTOR).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rsync_receiver_cannot_write_past_the_limit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::walk::scratch("rsync-limit");
        let fake = dir.join("rsync");
        fs::write(
            &fake,
            "#!/bin/sh\nfor last; do :; done\nhead -c 8192 /dev/zero > \"$last\"\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(dir.clone()).chain(std::env::split_paths(&inherited)),
        )
        .unwrap();
        let target = dir.join("out");
        let status = rsync_command("rsync://example.invalid/x", &target, 1024)
            .env("PATH", path)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        let written = fs::metadata(&target).map_or(0, |m| m.len());
        assert!(
            !status.success() && written <= 2048,
            "{status} wrote {written} bytes"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn contract_paths_cannot_escape_the_cache() {
        for file in ["../outside.mkv", "/abs.mkv", "a/../../b", "a\\\\b"] {
            let root = crate::walk::scratch("vectors");
            contract(&root, file, b"x");
            assert_eq!(
                required_vectors(&root, &Fake::default(), VECTOR_LIMIT).unwrap_err(),
                "invalid required vector path"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn spec_catalog_matches_the_manifest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = crate::checks::read_json(&root.join("specs/sources.json")).unwrap();
        let mut ids: Vec<_> = manifest
            .get("sources")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|s| s.get("id").and_then(Value::as_str).unwrap().to_owned())
            .collect();
        ids.sort();
        let mut catalog: Vec<_> = SPECS.iter().map(|s| s.0.to_owned()).collect();
        catalog.sort();
        assert_eq!(ids, catalog);
    }

    #[test]
    fn every_specification_including_zlib_is_checked_for_drift() {
        for damaged in ["rfc1950", "rfc1951", "rfc9559"] {
            let root = crate::walk::scratch("specs");
            fs::create_dir_all(root.join(".research/specs")).unwrap();
            fs::create_dir_all(root.join("specs")).unwrap();
            let rows: Vec<String> = SPECS
                .iter()
                .map(|(id, _, _)| {
                    fs::write(root.join(".research/specs").join(id), id.as_bytes()).unwrap();
                    format!(
                        r#"{{"id": "{id}", "sha256": "{}"}}"#,
                        sha256::hex(id.as_bytes())
                    )
                })
                .collect();
            fs::write(
                root.join("specs/sources.json"),
                format!(r#"{{"sources": [{}]}}"#, rows.join(", ")),
            )
            .unwrap();
            let net = Fake::default();
            assert_eq!(
                specs(&root, false, &net).unwrap(),
                "Verified 7 specification snapshots"
            );
            fs::write(root.join(".research/specs").join(damaged), b"corrupt").unwrap();
            let error = specs(&root, false, &net).unwrap_err();
            assert!(error.starts_with(damaged), "{error}");
            assert!(
                net.calls.borrow().is_empty(),
                "cached verification must not fetch"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn formats_dates() {
        assert_eq!(today().len(), 10);
        assert!(today().as_str() >= "2026-01-01");
    }
}
