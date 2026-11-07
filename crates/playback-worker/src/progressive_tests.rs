use super::*;

// These tests reuse the repository's JSON reader; its writer helpers are unused here.
#[allow(dead_code)]
#[path = "../../../xtask/src/json.rs"]
mod json;
#[path = "../../../xtask/src/sha256.rs"]
mod sha256;

fn cases() -> Vec<json::Value> {
    let manifest = json::parse_strict(include_str!(
        "../../../harness/features/progressive-ts188/controls.json"
    ))
    .unwrap();
    manifest.get("cases").unwrap().as_array().unwrap().to_vec()
}

fn text<'a>(value: &'a json::Value, key: &str) -> &'a str {
    value.get(key).unwrap().as_str().unwrap()
}

fn number(value: &json::Value, key: &str) -> u64 {
    let json::Value::Number(value) = value.get(key).unwrap() else {
        panic!("integer required")
    };
    u64::try_from(value.as_i128().unwrap()).unwrap()
}

fn path(row: &json::Value) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../harness/features/progressive-ts188/inputs")
        .join(text(row, "file"))
}

#[test]
fn all_five_complete_traces_and_work_match_the_original_observations() {
    let cases = cases();
    assert_eq!(cases.len(), 5);
    for row in &cases {
        let arguments = row.get("arguments").unwrap().as_array().unwrap();
        let has_dts = arguments[0].as_str().unwrap() == "pts-dts";
        let streaming = arguments[1].as_str().unwrap() == "before-eof";
        let mut trace = Vec::new();
        let work = operation(&path(row), has_dts, Some(&mut trace), streaming).unwrap();
        let expected = row.get("expected").unwrap();
        assert_eq!(
            work.packets,
            number(expected, "packets"),
            "{} packets",
            text(row, "id")
        );
        assert_eq!(
            work.bytes,
            number(expected, "bytes"),
            "{} bytes",
            text(row, "id")
        );
        assert_eq!(
            work.digest,
            number(expected, "digest"),
            "{} digest",
            text(row, "id")
        );
        // The archived worker prints this exact footer after the complete trace.
        writeln!(trace,
            "{{\"schema\":1,\"operation\":\"{OP}\",\"iterations\":1,\"packets\":{},\"bytes\":{},\"digest\":{},\"elapsed_ns\":0,\"eof\":true}}",
            work.packets, work.bytes, work.digest
        ).unwrap();
        assert_eq!(
            sha256::hex(&trace),
            text(
                row.get("outcomes").unwrap().get("candidate").unwrap(),
                "stdout_sha256"
            ),
            "{} literal trace",
            text(row, "id")
        );
    }
}

#[test]
fn original_fifty_session_work_and_fresh_sessions_preserve_the_fold() {
    let timed = json::parse_strict(include_str!(
        "../../../harness/features/progressive-ts188/corpus.json"
    ))
    .unwrap();
    for row in timed.get("cases").unwrap().as_array().unwrap() {
        let expected = row.get("expected").unwrap();
        let fifty = row.get("expected_fifty").unwrap();
        let mut digest = 0u64;
        for _ in 0..50 {
            digest = digest
                .wrapping_mul(31)
                .wrapping_add(number(expected, "digest"));
        }
        assert_eq!(
            digest,
            number(fifty, "digest"),
            "{} fifty-session fold",
            text(row, "id")
        );
        assert_eq!(50 * number(expected, "packets"), number(fifty, "packets"));
        assert_eq!(50 * number(expected, "bytes"), number(fifty, "bytes"));
    }
    for row in cases() {
        let expected = row.get("expected").unwrap();
        let args = row.get("arguments").unwrap().as_array().unwrap();
        for _ in 0..2 {
            let work = operation(
                &path(&row),
                args[0].as_str().unwrap() == "pts-dts",
                None,
                args[1].as_str().unwrap() == "before-eof",
            )
            .unwrap();
            assert_eq!(
                work.digest,
                number(expected, "digest"),
                "{} fresh session",
                text(&row, "id")
            );
        }
    }
}
