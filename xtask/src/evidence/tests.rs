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
    let tar = crate::archive::gunzip(&pack(&dir.join("in"), Vec::new()).unwrap(), 1 << 30).unwrap();
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

/// A gate shard's pack is `tar -cf - .` of its run directory through zstd, so
/// its names start with `./`; a reused step restores its own directory alone.
#[test]
fn a_kept_shard_pack_restores_one_step_directory() {
    let root = crate::walk::scratch("evidence-restore");
    let run = root.join("run");
    for (file, text) in [
        ("capture-c01-timing/identity.json", "{}"),
        ("capture-c01-timing/c01/timing.json", "{\"case\": 1}"),
        ("capture-c01/result.json", "{}"),
        ("capture-inputs/noise.rgba", "pixels"),
    ] {
        fs::create_dir_all(run.join(file).parent().unwrap()).unwrap();
        fs::write(run.join(file), text).unwrap();
    }
    let tar = root.join("shard.tar");
    let packed = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(&run)
        .arg(".")
        .status()
        .expect("tar is installed");
    assert!(packed.success());
    let compressed = Command::new("zstd")
        .arg("--quiet")
        .arg(&tar)
        .arg("-o")
        .arg(root.join("shard.tar.zst"))
        .status()
        .expect("zstd is installed");
    assert!(compressed.success());
    let sha = sha256::file_hex(&root.join("shard.tar.zst")).unwrap();
    let cache = root.join(".research/evidence-cache");
    fs::create_dir_all(&cache).unwrap();
    fs::rename(
        root.join("shard.tar.zst"),
        cache.join(format!("{sha}.tar.zst")),
    )
    .unwrap();
    let index = |retention: &str| {
        let set = Value::object([
            ("name", Value::str("shard")),
            ("path", Value::str("shard")),
            ("retention", Value::str(retention)),
            ("key", Value::str(format!("evidence/shard/{sha}.tar.zst"))),
            ("sha256", Value::str(&sha)),
        ]);
        let file = root.join(SETS).join("shard.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        json::write(&file, &set).unwrap();
    };
    index("retained");
    let into = root.join("restored");
    assert_eq!(
        restore(&root, "shard", "capture-c01-timing", &into).unwrap(),
        2
    );
    assert_eq!(
        fs::read_to_string(into.join("c01/timing.json")).unwrap(),
        "{\"case\": 1}"
    );
    assert!(into.join("identity.json").is_file());
    assert!(!into.join("result.json").exists() && !into.join("noise.rgba").exists());
    assert!(restore(&root, "shard", "capture-c02-timing", &root.join("none")).is_err());
    index("90 days");
    assert!(restore(&root, "shard", "capture-c01-timing", &root.join("again")).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_pack_name_picks_its_compression() {
    let dir = crate::walk::scratch("evidence-pack-zstd");
    fs::create_dir_all(dir.join("in/a")).unwrap();
    fs::write(dir.join("in/a/f.json"), b"{}\n").unwrap();
    fs::write(dir.join("in/b.json"), "[1, 2, 3]\n".repeat(50_000)).unwrap();
    let (gz, zst) = (dir.join("set.tar.gz"), dir.join("set.tar.zst"));
    pack_command(&dir.join("in"), &gz).unwrap();
    let digest = pack_command(&dir.join("in"), &zst).unwrap();
    assert_eq!(sha256::file_hex(&zst).unwrap(), digest);
    assert_eq!(
        &fs::read(&zst).unwrap()[..4],
        b"\x28\xb5\x2f\xfd",
        "a zstd frame"
    );
    assert!(fs::metadata(&zst).unwrap().len() * 20 < fs::metadata(&gz).unwrap().len());
    // The same tar inside either compression.
    let (mut from_gz, mut from_zst) = (Vec::new(), Vec::new());
    Compression::Gzip.unpack(&gz, &mut from_gz).unwrap();
    Compression::Zstd.unpack(&zst, &mut from_zst).unwrap();
    assert_eq!(from_gz, from_zst);
    for name in ["set.tar", "set.tgz", "set.tar.xz"] {
        let error = pack_command(&dir.join("in"), &dir.join(name)).unwrap_err();
        assert!(error.contains(".tar.gz or .tar.zst"), "{error}");
        assert!(!dir.join(name).exists());
    }
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
fn an_entry_takes_the_key_its_pack_lies_at() {
    let dir = crate::walk::scratch("evidence-entry");
    fs::create_dir_all(dir.join("set/memory/f")).unwrap();
    fs::write(dir.join("set/run.log"), b"done\n").unwrap();
    let out = dir.join("set.tar.gz");
    let sha = pack_command(&dir.join("set"), &out).unwrap();
    let root = crate::root();
    let (own, _) = entry(&root, "gate-shard", &out, None).unwrap();
    assert_eq!(own, format!("qualification/gate-shard/{sha}.tar.gz"));
    let at = format!("qualification/gate-runs/run-1/shard-1/{sha}.tar.gz");
    let (key, printed) = entry(&root, "gate-shard", &out, Some(&at)).unwrap();
    assert_eq!(key, at);
    assert!(printed.contains(&format!("\"key\": \"{at}\"")), "{printed}");
    for wrong in [
        format!("evidence/gate-runs/run-1/shard-1/{sha}.tar.gz"),
        "qualification/gate-runs/run-1/shard-1.tar.gz".to_owned(),
        format!("qualification/gate-runs/run-1/shard-1/{sha}.tar.zst"),
        format!("qualification/gate-runs/run-1/x{sha}.tar.gz"),
    ] {
        assert!(
            entry(&root, "gate-shard", &out, Some(&wrong)).is_err(),
            "{wrong}"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

/// A pack of a graded feature, its report, and a test profile's result.
fn graded_pack(dir: &Path) -> PathBuf {
    let result = |command: &str| {
        format!(
            r#"{{"schema": "gate-result-v1", "key": "{command}", "status": 0, "report_sha256": null,
                 "reused": null, "components": {{"command": "{command}", "target": "aarch64-unknown-linux-gnu"}}}}"#
        )
    };
    let set = dir.join("set");
    fs::create_dir_all(set.join("graded/f")).unwrap();
    fs::create_dir_all(set.join("tests/checked")).unwrap();
    fs::write(set.join("graded/f/gate-result.json"), result("grade")).unwrap();
    fs::write(
        set.join("graded/f/report.json"),
        r#"{"cases": [{"id": "a", "timing": {"status": "measured", "ratio": {"upper_95": 0.5}},
                       "memory": {"status": "qualified", "heap_ratio_upper_95": 0.25,
                                  "total_rss_ratio_upper_95": 0.75}}]}"#,
    )
    .unwrap();
    fs::write(
        set.join("tests/checked/gate-result.json"),
        result("gate test"),
    )
    .unwrap();
    let out = dir.join("set.tar.gz");
    pack_command(&set, &out).unwrap();
    out
}

#[test]
fn an_entry_records_each_results_target_and_what_it_measured() {
    let dir = crate::walk::scratch("evidence-measured");
    let out = graded_pack(&dir);
    let (_, printed) = entry(&crate::root(), "graded", &out, None).unwrap();
    let entry = json::parse(&printed).unwrap();
    let results = entry.get("results").and_then(Value::as_array).unwrap();
    let graded = json::parse(
        r#"{"path": "graded/f/gate-result.json", "command": "grade",
            "target": "aarch64-unknown-linux-gnu", "key": "grade", "status": 0,
            "report_sha256": null, "measured": {
              "runtime": {"upper": 0.5, "worst": "a", "passing": 1, "cases": 1},
              "memory": {"heap": 0.25, "total_rss": 0.75, "lower": 1, "cases": 1}}}"#,
    )
    .unwrap();
    assert_eq!(results[0], graded);
    // A test profile measures nothing.
    assert_eq!(s(&results[1], "command"), Some("gate test"));
    assert!(results[1].get("measured").is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn measuring_an_indexed_set_records_what_its_pack_holds() {
    let root = crate::walk::scratch("evidence-measure");
    let out = graded_pack(&root);
    let sha = sha256::file_hex(&out).unwrap();
    let cache = root.join(".research/evidence-cache");
    fs::create_dir_all(&cache).unwrap();
    fs::copy(&out, cache.join(format!("{sha}.tar.gz"))).unwrap();
    // The entry as indexed before results recorded their target.
    let indexed = |key: &str| {
        format!(
            r#"{{"name": "graded", "retention": "retained", "key": "evidence/graded/{sha}.tar.gz",
                 "sha256": "{sha}", "results": [
                 {{"path": "graded/f/gate-result.json", "command": "grade", "key": "{key}", "status": 0,
                   "report_sha256": null}},
                 {{"path": "tests/checked/gate-result.json", "command": "gate test", "key": "gate test",
                   "status": 0, "report_sha256": null}}]}}"#
        )
    };
    let write = |key: &str| {
        let set = json::parse(&indexed(key)).unwrap();
        fs::create_dir_all(root.join(SETS)).unwrap();
        write_set(&root, "graded", &set).unwrap();
    };
    let policy = Policy::load(&crate::root()).unwrap();
    write("grade");
    assert_eq!(
        measure_set(&root, "graded", policy).unwrap(),
        "graded: 2 results, 1 with what they measured"
    );
    let set = set(&root, "graded").unwrap();
    let results = set.get("results").and_then(Value::as_array).unwrap();
    assert_eq!(s(&results[0], "target"), Some("aarch64-unknown-linux-gnu"));
    assert!(results[0].get("measured").is_some());
    // A pack already cached stays; an entry the pack does not hold is refused.
    assert!(cache.join(format!("{sha}.tar.gz")).is_file());
    write("another");
    assert_eq!(
        measure_set(&root, "graded", policy).unwrap_err(),
        "graded/f/gate-result.json: the pack holds no such result"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn promotion_moves_an_expiring_set_under_evidence() {
    let parse = |text| json::parse_strict(text).unwrap();
    let mut a =
        parse(r#"{"name": "a", "retention": "90 days", "key": "qualification/a/0.tar.gz"}"#);
    assert_eq!(
        promote_set(&mut a, "a").unwrap(),
        (
            "qualification/a/0.tar.gz".into(),
            "evidence/a/0.tar.gz".into()
        )
    );
    assert_eq!(s(&a, "key"), Some("evidence/a/0.tar.gz"));
    assert_eq!(s(&a, "retention"), Some("retained"));
    assert!(promote_set(&mut a, "a").unwrap_err().contains("not under"));
    let mut b = parse(r#"{"name": "b", "retention": "retained", "key": "evidence/b/1.tar.gz"}"#);
    assert!(promote_set(&mut b, "b").unwrap_err().contains("not under"));
}

#[test]
fn a_set_is_read_from_its_own_file_by_name() {
    let root = crate::walk::scratch("evidence-sets");
    fs::create_dir_all(root.join(SETS)).unwrap();
    for (file, name) in [("b", "b"), ("a", "a"), ("c", "other")] {
        fs::write(
            root.join(format!("{SETS}/{file}.json")),
            format!("{{\"name\": \"{name}\"}}\n"),
        )
        .unwrap();
    }
    assert_eq!(s(&set(&root, "a").unwrap(), "name"), Some("a"));
    assert!(
        set(&root, "d")
            .unwrap_err()
            .contains("not in the evidence index")
    );
    assert!(set(&root, "c").unwrap_err().contains("does not hold set c"));
    assert!(set(&root, "../a").unwrap_err().contains("not a set's name"));
    assert!(sets(&root).unwrap_err().contains("does not hold set c"));
    fs::remove_file(root.join(format!("{SETS}/c.json"))).unwrap();
    let names: Vec<_> = sets(&root)
        .unwrap()
        .iter()
        .map(|set| s(set, "name").unwrap().to_owned())
        .collect();
    assert_eq!(names, ["a", "b"]);
    fs::write(root.join(format!("{SETS}/notes.txt")), "").unwrap();
    assert!(sets(&root).unwrap_err().contains("not a set's file"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_index_lists_every_archived_file_once() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let Ok(index) = json::read(&root.join(INDEX)) else {
        return;
    };
    let canonical = |path: &str, value: &Value| {
        assert_eq!(
            json::to_python(value, Some(1)) + "\n",
            fs::read_to_string(root.join(path)).unwrap(),
            "{path} must stay in its canonical layout, which `evidence promote` rewrites"
        );
    };
    canonical(INDEX, &index);
    assert!(index.get("sets").is_none(), "{INDEX}: sets lie in {SETS}");
    let sets = sets(&root).unwrap();
    assert!(!sets.is_empty());
    let mut paths = std::collections::BTreeSet::new();
    let mut kept_results = std::collections::BTreeMap::new();
    for set in &sets {
        let name = s(set, "name").unwrap();
        canonical(&set_path(name).unwrap(), set);
        if s(set, "retention") == Some(Retention::Kept.label()) {
            for result in set
                .get("results")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                let key = s(result, "key").unwrap();
                if let Some(other) = kept_results.insert(key.to_owned(), name.to_owned()) {
                    // Reuse takes a key's result from the one kept set that lists it.
                    assert_eq!(other, name, "result {key} is kept in {other} and {name}");
                }
            }
        }
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
