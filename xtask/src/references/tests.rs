use super::*;

fn repository(root: &Path) {
    for file in COMMON_RECIPE
        .iter()
        .chain(BUILDS.iter().flat_map(|b| b.recipe))
    {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, file.as_bytes()).unwrap();
    }
}

#[test]
fn every_ffmpeg_profile_has_a_build_and_names_are_unique() {
    use crate::prepare::Profile;
    for name in [
        "matroska",
        "formats",
        "video",
        "subtitles",
        "decode",
        "capture",
        "capture-small",
    ] {
        let build = ffmpeg_build(Profile::parse(name).unwrap());
        assert_eq!(build.name, format!("ffmpeg-{name}"));
    }
    for (i, build) in BUILDS.iter().enumerate() {
        assert!(BUILDS[..i].iter().all(|b| b.name != build.name));
        assert!(build.links.iter().all(|name| super::build(name).is_ok()));
    }
}

#[test]
fn a_recipe_change_changes_the_key_of_every_build_that_links_it() {
    let root = crate::walk::scratch("reference-keys");
    repository(&root);
    let keys = |root: &Path| -> Vec<String> {
        BUILDS.iter().map(|b| b.key(root).unwrap().sha256).collect()
    };
    let before = keys(&root);
    assert_eq!(before, keys(&root));
    fs::write(root.join("xtask/src/fonts/mod.rs"), b"changed").unwrap();
    let after = keys(&root);
    let changed: Vec<&str> = BUILDS
        .iter()
        .zip(before.iter().zip(&after))
        .filter(|(_, (a, b))| a != b)
        .map(|(build, _)| build.name)
        .collect();
    assert_eq!(changed, ["freetype", "libass"]);
    fs::write(root.join("xtask/src/harness/gate_build.rs"), b"policy").unwrap();
    assert!(keys(&root).iter().zip(&after).all(|(a, b)| a != b));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_build_is_made_once_then_packed_and_restored_unchanged() {
    let root = crate::walk::scratch("reference-cache");
    repository(&root);
    let freetype = build("freetype").unwrap();
    let dir = root.join(freetype.directory);
    let make = || {
        fs::create_dir_all(dir.join("install/lib")).unwrap();
        fs::write(dir.join("install/lib/libfreetype.a"), b"archive").unwrap();
        fs::write(dir.join("build.json"), b"{}").unwrap();
        Ok("made".to_owned())
    };
    assert_eq!(freetype.prepare(&root, make).unwrap(), "made");
    let unused = || -> R<String> { panic!("built twice") };
    freetype.prepare(&root, unused).unwrap();
    let key = freetype.key(&root).unwrap();
    let pack = freetype.pack(&root, &key).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    freetype.prepare(&root, unused).unwrap();
    assert_eq!(
        fs::read(dir.join("install/lib/libfreetype.a")).unwrap(),
        b"archive"
    );
    assert!(freetype.holds(&root, &key));
    // A pack of another build is removed, and the build made from source.
    fs::write(root.join("xtask/src/fonts/mod.rs"), b"changed").unwrap();
    let other = freetype.key(&root).unwrap();
    let mislabelled = freetype.pack_path(&root, &other);
    fs::create_dir_all(mislabelled.parent().unwrap()).unwrap();
    fs::copy(&pack, &mislabelled).unwrap();
    assert_eq!(freetype.prepare(&root, make).unwrap(), "made");
    assert!(!mislabelled.exists());
    assert!(freetype.holds(&root, &other));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_miss_builds_from_nothing_and_only_a_clean_tree_is_packed() {
    let root = crate::walk::scratch("reference-clean");
    repository(&root);
    let freetype = build("freetype").unwrap();
    let dir = root.join(freetype.directory);
    fs::create_dir_all(dir.join("freetype-2.15.0")).unwrap();
    fs::write(dir.join("freetype-2.15.0/config.mk"), b"stale").unwrap();
    fs::write(dir.join("configure-argv.json"), b"[]").unwrap();
    fs::write(dir.join("freetype-2.15.0.tar.xz"), b"pinned").unwrap();
    let make = || {
        assert!(!dir.join("freetype-2.15.0").exists());
        assert!(!dir.join("configure-argv.json").exists());
        assert!(dir.join("freetype-2.15.0.tar.xz").is_file());
        fs::create_dir_all(dir.join("install/lib")).unwrap();
        fs::write(dir.join("install/lib/libfreetype.a"), b"archive").unwrap();
        fs::write(dir.join("build.json"), b"{}").unwrap();
        Ok("made".to_owned())
    };
    freetype.prepare(&root, make).unwrap();
    let key = freetype.key(&root).unwrap();
    fs::write(dir.join("install/lib/libfreetype.a"), b"changed").unwrap();
    assert!(freetype.pack(&root, &key).is_err());
    fs::write(dir.join("install/lib/libfreetype.a"), b"archive").unwrap();
    assert!(freetype.pack(&root, &key).is_ok());
    fs::remove_dir_all(root).unwrap();
}
