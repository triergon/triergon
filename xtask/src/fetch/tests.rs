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

    fn github_asset(&self, _: &str, _: u64, _: &str, _: usize) -> Result<Vec<u8>, String> {
        unreachable!("vector and specification downloads use plain URLs")
    }

    fn rsync(&self, url: &str, target: &Path, _: usize) -> Result<(), String> {
        self.calls.borrow_mut().push(url.to_owned());
        let data = self.rsync.clone().ok_or("connection refused")?;
        fs::write(target, data).map_err(|e| e.to_string())
    }
}

fn contract(root: &Path, file: &str, content: &[u8]) {
    fs::create_dir_all(root.join(VECTOR_CONTRACT).parent().unwrap()).unwrap();
    let text = format!(
        r#"{{"required_additional_vectors": [{{"file": "{file}", "sha256": "{}"}}]}}"#,
        sha256::hex(content)
    );
    fs::write(root.join(VECTOR_CONTRACT), text).unwrap();
}

const VECTOR: &str = ".research/fate-samples/mkv/required.mkv";

#[test]
fn fetched_cases_share_a_pinned_input_and_reuse_the_download_cache() {
    let root = crate::walk::scratch("fate-corpus");
    let feature = root.join("harness/features/range");
    let cache = root.join("downloads");
    let relative = "mov/prefix.mp4";
    let bytes = b"pinned vector";
    let hash = sha256::hex(bytes);
    fs::create_dir_all(&feature).unwrap();
    fs::write(
        feature.join("contract.json"),
        r#"{"id":"range","fetch_fate_cases":true}"#,
    )
    .unwrap();
    let case =
        format!(r#"{{"file":"{relative}","source":"fate-suite/{relative}","sha256":"{hash}"}}"#);
    fs::write(
        feature.join("corpus.json"),
        format!(r#"{{"directory":".research/range","cases":[{case},{case}]}}"#),
    )
    .unwrap();

    let net = Fake {
        https: Some(bytes.to_vec()),
        ..Fake::default()
    };
    let input = root.join(".research/range").join(relative);
    let run = |transport| fate_corpus_vectors(&root, transport, Some(&cache), VECTOR_LIMIT);
    assert_eq!(run(&net).unwrap(), [relative]);
    assert_eq!(net.calls.borrow().len(), 1);
    assert_eq!(fs::read(&input).unwrap(), bytes);
    assert_eq!(fs::read(cache.join(hash)).unwrap(), bytes);
    fs::remove_file(&input).unwrap();

    let offline = Fake::default();
    assert_eq!(run(&offline).unwrap(), [relative]);
    assert!(offline.calls.borrow().is_empty());
    assert_eq!(fs::read(&input).unwrap(), bytes);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cached_original_is_verified_without_network() {
    let root = crate::walk::scratch("vectors");
    contract(&root, "mkv/required.mkv", b"original vector");
    fs::create_dir_all(root.join(".research/fate-samples/mkv")).unwrap();
    fs::write(root.join(VECTOR), b"original vector").unwrap();
    let net = Fake::default();
    assert_eq!(
        required_vectors(&root, &net, None, VECTOR_LIMIT).unwrap(),
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
    let error = required_vectors(&root, &Fake::default(), None, VECTOR_LIMIT).unwrap_err();
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
        let error = required_vectors(&root, &net, None, VECTOR_LIMIT).unwrap_err();
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
        required_vectors(&root, &net, None, VECTOR_LIMIT).unwrap(),
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
    let error = required_vectors(&root, &Fake::default(), None, VECTOR_LIMIT);
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
        required_vectors(&root, &net, None, 1024)
            .unwrap_err()
            .contains("exceeds download bound")
    );
    let error = required_vectors(&root, &Fake::default(), None, 1024).unwrap_err();
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
    let path =
        std::env::join_paths(std::iter::once(dir.clone()).chain(std::env::split_paths(&inherited)))
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
            required_vectors(&root, &Fake::default(), None, VECTOR_LIMIT).unwrap_err(),
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

/// A transport that answers each URL from a table and records what was asked.
struct Sources {
    answers: Vec<(&'static str, Result<&'static [u8], &'static str>)>,
    calls: RefCell<Vec<String>>,
}

impl Sources {
    fn new(answers: Vec<(&'static str, Result<&'static [u8], &'static str>)>) -> Self {
        Self {
            answers,
            calls: RefCell::default(),
        }
    }
}

impl Transport for Sources {
    fn https(&self, url: &str, _: &str, _: usize) -> Result<Vec<u8>, String> {
        self.answer(url)
    }

    fn github_asset(&self, repo: &str, id: u64, _: &str, _: usize) -> Result<Vec<u8>, String> {
        self.answer(&format!("asset:{repo}#{id}"))
    }

    fn rsync(&self, _: &str, _: &Path, _: usize) -> Result<(), String> {
        unreachable!("pinned downloads are HTTPS")
    }
}

impl Sources {
    fn answer(&self, key: &str) -> Result<Vec<u8>, String> {
        self.calls.borrow_mut().push(key.to_owned());
        let (_, answer) = self.answers.iter().find(|(k, _)| *k == key).unwrap();
        answer.map(<[u8]>::to_vec).map_err(str::to_owned)
    }
}

const PINNED: &[u8] = b"pinned archive";

fn urls<'a>(urls: &[&'a str]) -> Vec<Source<'a>> {
    urls.iter().map(|&url| Source::Url(url)).collect()
}

#[test]
fn pinned_reaches_a_github_asset_through_the_api() {
    let pin = sha256::hex(PINNED);
    let net = Sources::new(vec![
        (
            "https://release",
            Err("curl https://release: exit status: 22"),
        ),
        ("asset:owner/name#7", Ok(PINNED)),
    ]);
    let sources = [
        Source::Url("https://release"),
        Source::GitHubAsset {
            repo: "owner/name",
            id: 7,
        },
    ];
    assert_eq!(
        pinned_in(&net, None, &sources, &pin, 64, "test").unwrap(),
        PINNED
    );
    assert_eq!(
        *net.calls.borrow(),
        ["https://release", "asset:owner/name#7"]
    );
}

#[test]
fn pinned_falls_back_past_failing_and_wrong_sources_and_fills_the_cache() {
    let cache = crate::walk::scratch("download-cache");
    let pin = sha256::hex(PINNED);
    let net = Sources::new(vec![
        ("https://down", Err("curl https://down: exit status: 22")),
        ("https://other-bytes", Ok(b"another archive")),
        ("https://mirror", Ok(PINNED)),
    ]);
    let sources = ["https://down", "https://other-bytes", "https://mirror"];
    let data = pinned_in(&net, Some(&cache), &urls(&sources), &pin, 64, "test").unwrap();
    assert_eq!(data, PINNED);
    assert_eq!(fs::read(cache.join(&pin)).unwrap(), PINNED);
    assert_eq!(net.calls.borrow().len(), 3);
    fs::remove_dir_all(cache).unwrap();
}

#[test]
fn pinned_uses_a_verified_cache_entry_without_network() {
    let cache = crate::walk::scratch("download-cache");
    let pin = sha256::hex(PINNED);
    fs::write(cache.join(&pin), PINNED).unwrap();
    let net = Sources::new(vec![]);
    let data = pinned_in(&net, Some(&cache), &urls(&["https://x"]), &pin, 64, "test").unwrap();
    assert_eq!(data, PINNED);
    assert!(net.calls.borrow().is_empty());
    fs::remove_dir_all(cache).unwrap();
}

#[test]
fn pinned_replaces_a_damaged_cache_entry() {
    let cache = crate::walk::scratch("download-cache");
    let pin = sha256::hex(PINNED);
    fs::write(cache.join(&pin), b"damaged").unwrap();
    let net = Sources::new(vec![("https://x", Ok(PINNED))]);
    let data = pinned_in(&net, Some(&cache), &urls(&["https://x"]), &pin, 64, "test").unwrap();
    assert_eq!(data, PINNED);
    assert_eq!(fs::read(cache.join(&pin)).unwrap(), PINNED);
    fs::remove_dir_all(cache).unwrap();
}

#[test]
fn pinned_names_every_source_when_none_serves_the_pin() {
    let pin = sha256::hex(PINNED);
    let net = Sources::new(vec![
        ("https://down", Err("curl https://down: exit status: 22")),
        ("https://large", Ok(b"an archive larger than the bound")),
    ]);
    let error = pinned_in(
        &net,
        None,
        &urls(&["https://down", "https://large"]),
        &pin,
        16,
        "test",
    )
    .unwrap_err();
    assert!(error.contains("https://down: exit status: 22"), "{error}");
    assert!(error.contains("https://large: over 16 bytes"), "{error}");
}
