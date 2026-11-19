use super::*;
#[test]
fn first_difference_names_plane_and_position() {
    let (w, h) = (4, 2);
    let a = vec![0u8; w * h * 3 / 2];
    let mut b = a.clone();
    b[w * h + 1] = 1;
    assert_eq!(
        json::to_python(&Value::Object(first_diff(&a, &b, w, h, 1, false)), None),
        r#"{"plane": "U", "x": 1, "y": 0}"#
    );
    let mut c = vec![0u8; w * h * 3];
    c[2 * (w * h + 2) + 1] = 9;
    assert_eq!(
        json::to_python(
            &Value::Object(first_diff(&vec![0; w * h * 3], &c, w, h, 2, false)),
            None
        ),
        r#"{"plane": "V", "x": 0, "y": 0}"#
    );
}

#[test]
fn ctb_coordinates_use_luma_units_and_short_samples_stay_bounded() {
    let (w, h, depth) = (130, 70, 2);
    let a = vec![0; w * h * 3];
    let mut b = a.clone();
    let offset = (w * h + 32 * (w / 2) + 64) * depth;
    b[offset + 1] = 1;
    let position =
        |a, b| json::to_python(&Value::Object(first_diff(a, b, w, h, depth, true)), None);
    let uv = r#"{"plane": "U", "x": 64, "y": 32, "ctb64": [2, 1]}"#;
    assert_eq!(position(&a, &b), uv);
    assert_eq!(position(&a, &b[..offset + 1]), uv);
    assert_eq!(position(&a[..offset + 1], &b), uv);
    assert_eq!(position(&a[..offset], &a[..offset]), "{}");
}

#[cfg(unix)]
mod child_processes {
    use super::*;
    use crate::process::{OUT, TempDir};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn program(dir: &Path, name: &str, script: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path.to_str().unwrap().to_owned()
    }

    fn planar_row(ours: &str, reference: &str) -> Value {
        let tmp = TempDir::new(None).unwrap();
        let probe = program(
            &tmp.0,
            "probe",
            r#"echo '{"streams":[{"width":2,"height":2,"pix_fmt":"yuv420p"}]}'"#,
        );
        decoder(
            &probe,
            &program(&tmp.0, "reference", reference),
            &program(&tmp.0, "ours", ours),
            "hevc",
            "file.bit",
        )
        .unwrap()
    }

    // The side-output worker opens its output path just as codec-worker does.
    fn rgba_row(ours: &str, reference: &str) -> Value {
        let producer = |script: &str| {
            Frames::spawn(
                &[
                    "sh".into(),
                    "-c".into(),
                    format!("exec > \"$0\"\n{script}"),
                    OUT.into(),
                ],
                6,
                true,
            )
            .unwrap()
        };
        output(producer(ours), producer(reference)).unwrap()
    }

    #[test]
    fn exact_output_requires_two_full_successful_streams() {
        for (row, ref_rc, ref_partial, counts) in [
            (
                planar_row as fn(&str, &str) -> Value,
                "ffmpeg_rc",
                "ffmpeg_partial_bytes",
                ["frames_ours", "frames_ref", "frames_equal"],
            ),
            (
                rgba_row as fn(&str, &str) -> Value,
                "reference_rc",
                "reference_partial_bytes",
                ["pictures_ours", "pictures_ref", "pictures_equal"],
            ),
        ] {
            let frame = "printf abcdef";
            for (ours, reference, status, codes, tails, frames) in [
                (frame, frame, "pass", [0, 0], [0, 0], [1, 1, 1]),
                (
                    "printf abcdef; exit 3",
                    frame,
                    "failed-exit",
                    [3, 0],
                    [0, 0],
                    [1, 1, 1],
                ),
                (
                    frame,
                    "printf abcdef; exit 3",
                    "failed-exit",
                    [0, 3],
                    [0, 0],
                    [1, 1, 1],
                ),
                (
                    "printf abcdefX",
                    frame,
                    "partial-frame",
                    [0, 0],
                    [1, 0],
                    [1, 1, 1],
                ),
                (
                    frame,
                    "printf abcdefXY",
                    "partial-frame",
                    [0, 0],
                    [0, 2],
                    [1, 1, 1],
                ),
                (
                    "printf X",
                    "printf abcdefabcdefXY; exit 3",
                    "partial-frame",
                    [0, 3],
                    [1, 2],
                    [0, 2, 0],
                ),
                (
                    "printf abcdefabcdef",
                    frame,
                    "fail",
                    [0, 0],
                    [0, 0],
                    [2, 1, 1],
                ),
                (
                    frame,
                    "printf abcdefabcdef",
                    "fail",
                    [0, 0],
                    [0, 0],
                    [1, 2, 1],
                ),
            ] {
                let result = row(ours, reference);
                assert_eq!(
                    result.get("status").and_then(Value::as_str),
                    Some(status),
                    "{result:?}"
                );
                let numbers = |key: &str| result.get(key).and_then(python::int).unwrap();
                assert_eq!(["ours_rc", ref_rc].map(numbers), codes);
                assert_eq!(["ours_partial_bytes", ref_partial].map(numbers), tails);
                assert_eq!(counts.map(numbers), frames);
            }
        }
    }

    #[test]
    fn reports_first_picture_and_sample_for_each_output_format() {
        let candidate = r#"printf abcdefabcdXYabcdXu
echo '{"frame":1,"poc":27}' >&2"#;
        let reference = "printf abcdefabcdefabcdef";
        let planar = planar_row(candidate, reference);
        assert_eq!(
            planar.get("first_diff"),
            Some(
                &json::parse(r#"{"frame":1,"plane":"U","x":0,"y":0,"ctb64":[0,0],"poc":27}"#)
                    .unwrap()
            )
        );
        let rgba = rgba_row(candidate, reference);
        assert_eq!(
            rgba.get("first_diff"),
            Some(
                &json::parse(r#"{"frame":1,"x":1,"y":0,"channel":"R","differing_bytes":2}"#)
                    .unwrap()
            )
        );
        assert_eq!(rgba.get("differing_pictures"), Some(&Value::int(2)));
        assert_eq!(rgba.get("max_abs"), Some(&Value::int(15)));
    }

    #[test]
    fn missing_geometry_requires_observed_reference_refusal() {
        let tmp = TempDir::new(None).unwrap();
        let probe = program(&tmp.0, "probe", r#"echo '{"streams":[]}'"#);
        let ours = program(&tmp.0, "ours", "exit 1");
        let reference = program(&tmp.0, "reference", "exit 2");
        let run = || decoder(&probe, &reference, &ours, "hevc", "file.bit").unwrap();
        let refused = run();
        assert_eq!(refused.get("status"), Some(&Value::str("ffmpeg-refused")));
        for (field, value) in [
            ("ffprobe_rc", 0),
            ("ours_rc", 1),
            ("ffmpeg_rc", 2),
            ("ours_partial_bytes", 0),
            ("ffmpeg_partial_bytes", 0),
        ] {
            assert_eq!(refused.get(field), Some(&Value::int(value)));
        }

        program(&tmp.0, "reference", "printf X; exit 2");
        let bytes = run();
        assert_eq!(bytes.get("status"), Some(&Value::str("unframed-output")));
        assert_eq!(bytes.get("ffmpeg_bytes"), Some(&Value::int(1)));
        assert_eq!(bytes.get("ffmpeg_partial_bytes"), Some(&Value::Null));
    }

    #[test]
    fn declared_candidate_refusal_cannot_hide_a_late_reference_failure() {
        let refuse = r#"echo '{"error":"unsupported HEVC feature: unequal luma/chroma bit depth"}' >&2; exit 1"#;
        let frame = "printf abcdef";
        let late_exit = "printf abcdef; exit 2";
        let tmp = TempDir::new(None).unwrap();
        let run = tmp.0.join("run.jsonl");
        for (planar, rgba, accepted) in [
            (frame, frame, true),
            (late_exit, frame, false),
            (frame, late_exit, false),
            (":", frame, false),
            (frame, "printf abcdefX", false),
            (frame, ":", false),
        ] {
            let decoded = planar_row(refuse, planar);
            assert_eq!(
                decoded.get("ours_errors").unwrap().as_array().unwrap()[0].get("error"),
                Some(&Value::str(
                    "unsupported HEVC feature: unequal luma/chroma bit depth"
                ))
            );
            let row = Value::object([
                ("lane", Value::str("hevc")),
                ("name", Value::str("prefix.bit")),
                ("decoder", decoded),
                ("output", rgba_row("exit 1", rgba)),
            ]);
            fs::write(&run, json::to_python(&row, None)).unwrap();
            let result = crate::codecs::summary::summarize(
                run.to_str().unwrap(),
                &tmp.0.join("summary.json"),
                true,
            );
            assert_eq!(result.is_ok(), accepted, "{row:?}: {result:?}");
        }
    }

    #[test]
    fn refusals_remain_distinct_from_truncated_or_failed_runs() {
        let unsupported =
            r#"echo '{"error":"unsupported HEVC feature: unequal luma/chroma bit depth"}' >&2"#;
        let crash = format!("{unsupported}\nexit 101");
        assert_eq!(
            planar_row(&crash, "printf abcdef").get("status"),
            Some(&Value::str("failed-exit"))
        );
        let refuse = format!("{unsupported}\nexit 1");
        let status = |row: Value| row.get("status").cloned().unwrap();
        assert_eq!(
            status(planar_row(&refuse, "printf abcdef")),
            Value::str("refused-unsupported")
        );
        assert_eq!(
            status(planar_row(&refuse, "exit 2")),
            Value::str("ffmpeg-refused")
        );
        assert_eq!(
            status(planar_row(&refuse, "printf X; exit 2")),
            Value::str("partial-frame")
        );
        assert_eq!(
            status(planar_row(&refuse, "printf abcdef; exit 2")),
            Value::str("failed-exit")
        );
        assert_eq!(
            status(planar_row(&format!("printf X\n{refuse}"), "printf abcdef")),
            Value::str("partial-frame")
        );
        assert_eq!(
            status(planar_row(
                &format!("printf abcdef\n{unsupported}"),
                "printf abcdef"
            )),
            Value::str("decoder-error")
        );
        assert_eq!(
            status(planar_row("printf abcdef", "exit 2")),
            Value::str("failed-exit")
        );
        assert_eq!(status(planar_row(":", ":")), Value::str("fail"));
        assert_eq!(
            status(rgba_row(":", "exit 2")),
            Value::str("reference-produced-nothing")
        );
    }
}
