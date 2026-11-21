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
