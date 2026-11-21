use super::*;
use crate::json::parse;

fn policy() -> Policy {
    Policy::load(&crate::root()).unwrap()
}

fn row(id: &str, upper: &str, heap: &str, rss: &str) -> Value {
    parse(&format!(
        r#"{{"id": "{id}", "timing": {{"status": "measured", "ratio": {{"upper_95": {upper}}}}},
            "memory": {{"status": "qualified", "heap_ratio_upper_95": {heap},
                        "total_rss_ratio_upper_95": {rss}}}}}"#
    ))
    .unwrap()
}

fn files(entries: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
    entries
        .iter()
        .map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec()))
        .collect()
}

#[test]
fn a_record_holds_the_largest_bounds_and_the_cases_that_hold() {
    let over = format!("{}", policy().runtime.ceiling * 2.0);
    let rows = [
        row("a", "0.5", "0.4", "0.7"),
        row("b", &over, "0.6", "0.8"),
        row("c", "0.9", "0.3", "Infinity"),
        parse(r#"{"id": "d", "outcome": "refusal"}"#).unwrap(),
        parse(r#"{"id": "e", "timing": {"status": "contaminated"}}"#).unwrap(),
    ];
    let record = Measured::of_rows(&rows, policy()).unwrap();
    assert_eq!(
        record.runtime,
        Some(Runtime {
            worst: Some((over.parse().unwrap(), "b".into())),
            passing: 2,
            cases: 4,
        })
    );
    assert_eq!(
        record.memory,
        Some(Memory {
            heap: Some(0.6),
            total_rss: Some(0.8),
            lower: 2,
            cases: 3,
        })
    );
    assert_eq!(Measured::parse(&record.to_value()), Ok(record));
    // A collection of one kind records that kind alone; one that measured nothing, none.
    let timed =
        [
            parse(r#"{"id": "a", "timing": {"status": "measured", "ratio": {"upper_95": 0.5}}}"#)
                .unwrap(),
        ];
    assert!(
        Measured::of_rows(&timed, policy())
            .unwrap()
            .memory
            .is_none()
    );
    assert_eq!(Measured::of_rows(&rows[3..4], policy()), None);
}

#[test]
fn records_join_by_their_largest_bounds_and_summed_counts() {
    let parse_record = |text: &str| Measured::parse(&parse(text).unwrap()).unwrap();
    let timing =
        parse_record(r#"{"runtime": {"upper": 0.9, "worst": "x", "passing": 3, "cases": 3}}"#);
    let other = parse_record(
        r#"{"runtime": {"upper": 0.95, "worst": "y", "passing": 1, "cases": 2},
            "memory": {"heap": 0.5, "total_rss": null, "lower": 0, "cases": 2}}"#,
    );
    let memory =
        parse_record(r#"{"memory": {"heap": 0.4, "total_rss": 0.8, "lower": 4, "cases": 4}}"#);
    let joined = Measured::join([&timing, &other, &memory]);
    assert_eq!(
        joined.to_value(),
        parse(
            r#"{"runtime": {"upper": 0.95, "worst": "y", "passing": 4, "cases": 5},
                "memory": {"heap": 0.5, "total_rss": 0.8, "lower": 4, "cases": 6}}"#
        )
        .unwrap()
    );
}

#[test]
fn a_malformed_record_is_refused() {
    for text in [
        "[]",
        r#"{"timing": {}}"#,
        r#"{"runtime": {"upper": 0.9, "worst": null, "passing": 1, "cases": 1}}"#,
        r#"{"runtime": {"upper": 0.9, "worst": "a", "passing": 2, "cases": 1}}"#,
        r#"{"runtime": {"upper": 0.9, "worst": "a", "passing": 1.0, "cases": 1}}"#,
        r#"{"memory": {"heap": "0.5", "total_rss": 0.5, "lower": 1, "cases": 1}}"#,
        r#"{"memory": {"heap": 0.5, "total_rss": 0.5, "lower": 2, "cases": 1}}"#,
    ] {
        assert!(Measured::parse(&parse(text).unwrap()).is_err(), "{text}");
    }
}

#[test]
fn each_command_is_read_from_the_files_it_writes() {
    assert_eq!(Source::of("gate test"), None);
    for (command, source) in [
        ("grade", Source::Report),
        ("report --benchmark", Source::Report),
        ("isobmff-seek --memory", Source::Report),
        ("external-vector --benchmark", Source::Report),
        ("codecs time", Source::CodecTimes),
        ("codecs memory", Source::CodecMemory),
        ("capture qualify", Source::Capture),
    ] {
        assert_eq!(Source::of(command), Some(source), "{command}");
    }
    let reads = |source: Source| {
        [
            "report.json",
            "summary.json",
            "aac-lc.json",
            "gate-result.json",
            "identity.json",
            "c01/result.json",
            "measurements/0000-timing.json",
        ]
        .into_iter()
        .filter(|path| source.reads(path))
        .collect::<Vec<_>>()
    };
    assert_eq!(reads(Source::Report), ["report.json"]);
    assert_eq!(reads(Source::CodecMemory), ["summary.json"]);
    assert_eq!(
        reads(Source::CodecTimes),
        ["report.json", "summary.json", "aac-lc.json"]
    );
    assert_eq!(reads(Source::Capture), ["c01/result.json"]);
}

#[test]
fn codec_and_capture_outputs_become_graded_rows() {
    let times = files(&[
        (
            "aac-lc.json",
            r#"{"id": "aac-lc", "operation": "audio-aac", "status": "measured", "ratio": {"upper_95": 0.8}}"#,
        ),
        (
            "h264.json",
            r#"{"id": "h264", "operation": "video-h264", "status": "invalid-timing"}"#,
        ),
        (
            "claims.json",
            r#"{"0123": {"id": "aac-lc", "status": "measured"}}"#,
        ),
    ]);
    let record = Measured::of_rows(&Source::CodecTimes.rows(&times).unwrap(), policy()).unwrap();
    assert_eq!(
        record.runtime,
        Some(Runtime {
            worst: Some((0.8, "aac-lc".into())),
            passing: 1,
            cases: 2,
        })
    );
    let memory = files(&[(
        "summary.json",
        r#"[{"id": "aac-lc", "memory_status": "qualified", "heap_ratio_upper_95": 0.7, "total_rss_ratio_upper_95": 0.6},
            {"id": "h264", "memory_status": "contaminated", "heap_ratio_upper_95": 0.2, "total_rss_ratio_upper_95": 0.3}]"#,
    )]);
    let record = Measured::of_rows(&Source::CodecMemory.rows(&memory).unwrap(), policy()).unwrap();
    assert_eq!(
        (record.runtime, record.memory),
        (
            None,
            Some(Memory {
                heap: Some(0.7),
                total_rss: Some(0.6),
                lower: 1,
                cases: 2,
            })
        )
    );
    let capture = files(&[
        (
            "c01/result.json",
            r#"{"runtime": {"ratio": {"median": 0.9, "upper_95": 0.95}},
                "memory": {"status": "qualified", "heap_ratio_upper_95": 0.5, "total_rss_ratio_upper_95": 0.9}}"#,
        ),
        ("c02/result.json", r#"{"runtime": {"status": "refused"}}"#),
    ]);
    let record = Measured::of_rows(&Source::Capture.rows(&capture).unwrap(), policy()).unwrap();
    assert_eq!(
        record.runtime,
        Some(Runtime {
            worst: Some((0.95, "c01".into())),
            passing: 1,
            cases: 2,
        })
    );
    assert_eq!(record.memory.map(|m| (m.lower, m.cases)), Some((1, 1)));
    let report = files(&[(
        "report.json",
        r#"{"case": {"id": "v", "timing": {"status": "measured", "ratio": {"upper_95": 0.4}}}}"#,
    )]);
    assert_eq!(
        Measured::of_rows(&Source::Report.rows(&report).unwrap(), policy())
            .and_then(|record| record.runtime)
            .map(|runtime| runtime.cases),
        Some(1)
    );
}
