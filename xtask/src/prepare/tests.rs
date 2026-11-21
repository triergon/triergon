use super::*;

#[test]
fn a_release_is_fetched_from_its_address_then_its_mirrors() {
    let release = PinnedRelease {
        file: "x.tar.xz",
        directory: "x",
        url: "https://example.org/x.tar.xz",
        mirrors: &[fetch::Source::GitHubAsset {
            repo: "owner/x",
            id: 7,
        }],
        sha256: "0",
    };
    let sources = format!("{:?}", release.sources());
    assert_eq!(
        sources,
        r#"[Url("https://example.org/x.tar.xz"), GitHubAsset { repo: "owner/x", id: 7 }]"#
    );
}

#[test]
fn lists_unique_sorted_fate_names_only() {
    let listing = "make: noise\nfate-b\nfate-a.x_1\nfate-b\nfate-\nfate-bad name\n  fate-indented\nfate-c-d\n";
    assert_eq!(fate_list(listing), "fate-a.x_1\nfate-b\nfate-c-d\n");
}

#[test]
fn formats_profile_extends_the_matroska_configuration() {
    let prefix = Path::new("/p/install");
    let matroska = configure_argv(prefix, Profile::Matroska);
    let formats = configure_argv(prefix, Profile::Formats);
    assert_eq!(matroska[1], "--prefix=/p/install");
    assert_eq!(matroska.len() + 1, formats.len());
    assert!(formats.contains(
        &"--enable-demuxer=matroska,mov,mpegts,h264,hevc,srt,ass,webvtt,sup,vobsub".to_owned()
    ));
    assert_eq!(
        formats.last().unwrap(),
        "--enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata"
    );
}

#[test]
fn capture_profiles_differ_only_in_size_optimization() {
    let prefix = Path::new("/p/install");
    let speed = configure_argv(prefix, Profile::Capture);
    let small = configure_argv(prefix, Profile::CaptureSmall);
    assert!(speed.iter().any(|arg| arg == "--enable-static"));
    assert!(speed.iter().any(|arg| arg == "--disable-shared"));
    assert_eq!(
        speed,
        small
            .into_iter()
            .filter(|arg| arg != "--enable-small")
            .collect::<Vec<_>>()
    );
}

#[test]
fn verifies_the_unpacked_tree_against_the_archive() {
    let dir = crate::walk::scratch("tree");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/a.c"), b"int a;").unwrap();
    let mut header = vec![0u8; 512];
    header[..7].copy_from_slice(b"src/a.c");
    header[124..135].copy_from_slice(b"00000000006");
    header[156] = b'0';
    let mut tar = header;
    tar.extend(b"int a;");
    tar.resize(1024, 0);
    tar.extend([0; 1024]);
    let deflated = crate::zlib::compress(&tar, 6, crate::zlib::Strategy::Default).unwrap();
    let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    gz.extend_from_slice(&deflated[2..deflated.len() - 4]);
    gz.extend(crate::zlib::crc32(&tar).to_le_bytes());
    gz.extend((tar.len() as u32).to_le_bytes());
    assert_eq!(verify_tree(&gz, &dir).unwrap(), 1);
    fs::write(dir.join("src/a.c"), b"int b;").unwrap();
    assert_eq!(
        verify_tree(&gz, &dir).unwrap_err(),
        "Pinned source was changed: src/a.c"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn video_profile_adds_only_the_h264_and_hevc_decoders() {
    let prefix = Path::new("/p");
    let matroska = configure_argv(prefix, Profile::Matroska);
    let video = configure_argv(prefix, Profile::Video);
    let changed: Vec<_> = matroska
        .iter()
        .zip(&video)
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(matroska.len(), video.len());
    assert_eq!(
        changed,
        [(
            &"--enable-decoder=pcm_s16le".to_owned(),
            &"--enable-decoder=pcm_s16le,h264,hevc".to_owned()
        )]
    );
}
