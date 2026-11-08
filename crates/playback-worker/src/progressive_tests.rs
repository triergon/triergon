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
fn fields_preserve_numeric_trace_and_failed_write_folds() {
    struct LimitedWriter {
        limit: usize,
        bytes: Vec<u8>,
    }
    impl Write for LimitedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !bytes.is_empty() && self.bytes.len() == self.limit {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let count = bytes.len().min(self.limit - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let seed = 17u64;
    let words = [u64::from('v' as u32), u64::MAX, 2];
    let expected = |words: &[u64]| {
        words.iter().fold(seed, |digest, &word| {
            digest.wrapping_mul(31).wrapping_add(word)
        })
    };
    let mut numeric = RetainedStream {
        digest: seed,
        ..RetainedStream::default()
    };
    numeric.fields('v', &[-1, 2], &mut None).unwrap();
    let mut traced = RetainedStream {
        digest: seed,
        ..RetainedStream::default()
    };
    let mut trace = Vec::new();
    traced.fields('v', &[-1, 2], &mut Some(&mut trace)).unwrap();
    assert_eq!(trace, b"v -1 2 ");
    assert_eq!(numeric.digest, expected(&words));
    assert_eq!(traced.digest, numeric.digest);

    for (limit, mixed) in [(0, 1), (1, 2), (4, 3), (6, 3)] {
        let mut stream = RetainedStream {
            digest: seed,
            ..RetainedStream::default()
        };
        let mut writer = LimitedWriter {
            limit,
            bytes: Vec::new(),
        };
        let error = stream
            .fields('v', &[-1, 2], &mut Some(&mut writer))
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(writer.bytes, &trace[..limit]);
        assert_eq!(stream.digest, expected(&words[..mixed]));
    }
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
