#[path = "../../media/tests/support/ebml.rs"]
mod ebml;

#[test]
fn source_seek_protocol_folds_the_complete_suffix() {
    let path = std::env::temp_dir().join(format!("triergon-cued-aac-{}.mkv", std::process::id()));
    std::fs::write(&path, ebml::cued_aac().0).unwrap();
    let run = |mode: &str| {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_media-worker"))
            .args([mode, path.to_str().unwrap(), "1010000000"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    let trace = run("cued-aac-trace");
    assert_eq!(
        trace,
        "{\"landing_ns\":1000000000,\"packets\":[{\"track\":0,\"pts_ns\":1000000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"7365636f6e64\"},{\"track\":0,\"pts_ns\":1020000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"7468697264\"}],\"eof\":true}\n"
    );
    let fold = run("cued-aac-fold");
    assert_eq!(
        fold,
        "{\"packets\":2,\"bytes\":11,\"digest\":14086002072218715579,\"eof\":true}\n"
    );
    assert_eq!(
        run("cued-aac-memory"),
        format!("TRIERGON_BEGIN_V1\nTRIERGON_END_V1\n{fold}")
    );
    let run_fractional = |mode: &str| {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_media-worker"))
            .args([mode, path.to_str().unwrap(), "999600000"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    };
    assert_eq!(
        run_fractional("cued-aac-trace"),
        "{\"landing_ns\":0,\"packets\":[{\"track\":0,\"pts_ns\":0,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"6669727374\"},{\"track\":0,\"pts_ns\":1000000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"7365636f6e64\"},{\"track\":0,\"pts_ns\":1020000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"7468697264\"}],\"eof\":true}\n"
    );
    assert_eq!(
        run_fractional("cued-aac-fold"),
        "{\"packets\":3,\"bytes\":16,\"digest\":10656081498208427173,\"eof\":true}\n"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn coded_fixture_seek_folds_every_packet_through_eof() {
    let fixture =
        include_bytes!("../../../harness/features/matroska-cued-aac/inputs/coded-cues.mkv");
    assert_eq!(fixture.as_slice(), ebml::cued_aac_coded());
    let path = std::env::temp_dir().join(format!("triergon-coded-cues-{}.mkv", std::process::id()));
    std::fs::write(&path, fixture).unwrap();
    let run = |mode| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_media-worker"))
            .args([mode, path.to_str().unwrap(), "100000000"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(
        run("cued-aac-trace"),
        concat!(
            "{\"landing_ns\":93000000,\"packets\":[",
            "{\"track\":0,\"pts_ns\":93000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"2064000190000e\"},",
            "{\"track\":0,\"pts_ns\":116000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"2064000190000e\"},",
            "{\"track\":0,\"pts_ns\":139000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"2064000190000e\"},",
            "{\"track\":0,\"pts_ns\":163000000,\"dts_ns\":null,\"duration_ns\":null,\"payload\":\"2064000190000e\"}],\"eof\":true}\n"
        )
    );
    assert_eq!(
        run("cued-aac-fold"),
        "{\"packets\":4,\"bytes\":28,\"digest\":16219901096867284417,\"eof\":true}\n"
    );
    std::fs::remove_file(path).unwrap();
}

fn check_dolby_folds(fixture: &[u8], mode: &str, expected: [&str; 2]) {
    let path = std::env::temp_dir().join(format!("triergon-{mode}-{}.mkv", std::process::id()));
    std::fs::write(&path, fixture).unwrap();
    for (target, expected) in ["130000000", "127600000"].into_iter().zip(expected) {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_media-worker"))
            .args([mode, path.to_str().unwrap(), target])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8(result.stdout).unwrap(), expected);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn cued_ac3_folds_both_cue_landings_through_eof() {
    let fixture =
        include_bytes!("../../../harness/features/matroska-cued-ac3/inputs/coded-cues.mkv");
    assert_eq!(fixture.as_slice(), ebml::cued_ac3_coded());
    check_dolby_folds(
        fixture,
        "cued-ac3-fold",
        [
            "{\"packets\":4,\"bytes\":3072,\"digest\":4603375696001153562,\"eof\":true}\n",
            "{\"packets\":8,\"bytes\":6144,\"digest\":7084967468992935401,\"eof\":true}\n",
        ],
    );
}

#[test]
fn cued_eac3_folds_both_cue_landings_through_eof() {
    let fixture =
        include_bytes!("../../../harness/features/matroska-cued-eac3/inputs/coded-cues.mkv");
    assert_eq!(fixture.as_slice(), ebml::cued_eac3_coded());
    check_dolby_folds(
        fixture,
        "cued-eac3-fold",
        [
            "{\"packets\":4,\"bytes\":3072,\"digest\":18072471727007900406,\"eof\":true}\n",
            "{\"packets\":8,\"bytes\":6144,\"digest\":9717410312991101686,\"eof\":true}\n",
        ],
    );
}
