// Included only in the source-trace example's test module. Expectations
// cover three authored inputs and exact pinned-native rows. Source
// rows/rationals/raw boxes are checked independently of the native rows.
fn required_clock27_case(name: &str, extension: &str) {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clock-boundary27");
    let expected = fs::read_to_string(folder.join(format!("{name}.txt"))).unwrap();
    let rows = |prefix: &str| {
        expected
            .lines()
            .filter_map(|line| line.strip_prefix(prefix))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    let source_expected = rows("S ");
    let native_expected = rows("N ");
    assert_eq!(source_expected.len(), 6);
    assert!(!native_expected.is_empty());
    let mut native_nonfits = Vec::new();
    for (window, quantum) in [(4096, 4096), (1, 3)] {
        let mut output = Vec::new();
        run(
            &folder.join(format!("{name}.{extension}")),
            window,
            quantum,
            false,
            &mut output,
        )
        .unwrap_or_else(|error| panic!("source operation {name} {window}/{quantum}: {error:?}"));
        let trace = String::from_utf8(output).unwrap();
        let events = |event: &str| {
            trace
                .lines()
                .filter(|line| line.starts_with(&format!("{{\"event\":\"{event}\",")))
                .collect::<Vec<_>>()
        };
        let sources = events("sample");
        assert_eq!(
            sources.iter().map(|line| source_row(line)).collect::<Vec<_>>(),
            source_expected,
            "independent unique source facts {name} {window}/{quantum}"
        );
        for wanted in rows("R ") {
            let matching: Vec<_> = sources
                .iter()
                .filter(|line| field(line, "track_id") == wanted[0] && field(line, "sample") == wanted[1])
                .collect();
            assert_eq!(matching.len(), 1);
            for (field_name, offset) in [
                ("dts_seconds", 2),
                ("cts_seconds", 4),
                ("decode_delta_seconds", 6),
            ] {
                let exact = format!("\"{field_name}\":[{},{}]", wanted[offset], wanted[offset + 1]);
                assert!(
                    matching[0].contains(&exact),
                    "exact source rational {name} {window}/{quantum}: {exact}"
                );
            }
            assert_eq!(field(matching[0], "configuration_epoch"), "1");
            assert_eq!(field(matching[0], "source"), "1");
            assert_eq!(field(matching[0], "generation"), "1");
        }
        for (event, prefix, fields) in [
            ("raw_metadata", "M ", ["offset", "raw_hex"].as_slice()),
            ("description", "D ", ["track_id", "description", "raw_hex"].as_slice()),
            ("edit", "E ", ["track_id", "edit", "segment_duration", "media_time", "rate_bits"].as_slice()),
        ] {
            let actual: Vec<_> = events(event)
                .iter()
                .map(|line| fields.iter().map(|name| field(line, name)).collect::<Vec<_>>())
                .collect();
            assert_eq!(actual, rows(prefix), "raw source {event} {name} {window}/{quantum}");
        }
        let configurations = events("configuration");
        assert_eq!(configurations.len(), 1);
        assert_eq!(field(configurations[0], "track_id"), "1");
        assert_eq!(field(configurations[0], "before_sample"), "1");
        assert_eq!(field(configurations[0], "description"), "1");
        assert_eq!(field(configurations[0], "epoch"), "1");
        for wanted in rows("T ") {
            let streams = events("native_stream");
            assert_eq!(streams.len(), 1);
            assert_eq!(field(streams[0], "track_id"), wanted[0]);
            assert_eq!(field(streams[0], "zero_vendor"), wanted[1]);
            assert_eq!(field(streams[0], "codec"), "rawvideo");
            assert_eq!(field(streams[0], "extradata_hex"), "");
        }
        assert!(events("native_origin").is_empty(), "known source clock must stay known");
        assert!(trace.contains("\"event\":\"status\",\"status\":\"complete\""));
        let native_actual: Vec<_> = events("native_packet")
            .iter()
            .map(|line| {
                assert!(line.contains("\"side_data\":[]"));
                native_row(line)
            })
            .collect();
        // Native comparison is deliberately after every canonical assertion and
        // compares the actual ordered sequence verbatim, including null clocks,
        // initial discard flags, repeated source identities and terminal delta.
        if native_actual != native_expected {
            native_nonfits.push(format!(
                "{name} {window}/{quantum}: actual {native_actual:?}; exact pinned-native {native_expected:?}"
            ));
        }
        if !trace.contains("\"event\":\"native_end\",\"confirmed_eof\":true") {
            native_nonfits.push(format!(
                "{name} {window}/{quantum}: required native EOF absent; statuses {:?}",
                events("native_status")
            ));
        }
    }
    assert!(native_nonfits.is_empty(), "{}", native_nonfits.join("\n"));
}

#[test]
fn required_native_clock_legacy_tfdt_above_i64_preserves_source() {
    required_clock27_case("f-tfdt-above-i64", "mp4");
}

#[test]
fn required_native_clock_modern_tfdt_above_i64_preserves_source() {
    required_clock27_case("modern-iso6-tfdt-above-i64", "mp4");
}

#[test]
fn required_native_fractional_expanded_edits_preserve_source() {
    required_clock27_case("boundary-expanded-fractional", "mov");
}
