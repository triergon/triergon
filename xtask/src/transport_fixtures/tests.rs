use super::*;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

#[test]
fn private_reference_archive_does_not_replace_the_result_used_for_pins() {
    let out = crate::walk::scratch("transport-reference-result");
    fs::create_dir_all(&out).unwrap();
    let done = worker::Finished {
            code: 0,
            stdout: format!(
                "{{\"operation\":\"{OPERATION}\",\"iterations\":1,\"packets\":2,\"bytes\":579,\"digest\":7,\"eof\":true}}\n"
            ).into_bytes(),
            stderr: b"[parser @ 0x1234] diagnostic\n".to_vec(),
        };
    let result = save_reference_result("sample", &out, &["worker".into()], &done).unwrap();
    assert_eq!(expected(&result).get("packets"), Some(&Value::int(2)));
    let raw = json::read(&out.join("sample.json")).unwrap();
    assert_eq!(raw.get("observation"), Some(&result));
    assert!(result.get("argv").is_none());
    assert_eq!(fs::read(out.join("sample.stdout")).unwrap(), done.stdout);
    assert_eq!(fs::read(out.join("sample.stderr")).unwrap(), done.stderr);
    fs::remove_dir_all(out).unwrap();
}

#[test]
fn original_inputs_arguments_and_all272_request_associations_are_unchanged() {
    let original = json::read(&root().join(ORIGINAL)).unwrap();
    let generated = all_cases(root()).unwrap();
    assert_eq!(generated.len(), 179);
    for (old, new) in cases(&original).unwrap().iter().zip(&generated) {
        assert!(new.bytes.is_none());
        for key in [
            "id",
            "profile",
            "file",
            "sha256",
            "rights",
            "arguments",
            "requests",
        ] {
            assert_eq!(old.get(key), new.row.get(key), "{key}");
        }
    }
    assert_eq!(
        cases(&original)
            .unwrap()
            .iter()
            .map(|v| v.get("requests").unwrap().as_array().unwrap().len())
            .sum::<usize>(),
        272
    );
}

#[test]
fn single_input_pins_stay_identical_when_sequence_reference_outputs_change() {
    let manifest = json::read(&root().join(format!("harness/{FEATURE}/corpus.json"))).unwrap();
    let rows = cases(&manifest).unwrap();
    let mut prior = rows[..141].to_vec();
    let mut sequences = 0;
    for row in &mut prior {
        if entities(row).len() > 1 {
            sequences += 1;
            assert!(row.get("reference_refusal").is_none());
            let Value::Object(fields) = row else {
                unreachable!()
            };
            for (key, value) in fields {
                match key.as_str() {
                    "single_input_reference_refusal" => *key = "reference_refusal".into(),
                    "expected" => {
                        *value = Value::object([
                            ("packets", Value::int(0)),
                            ("bytes", Value::int(0)),
                            ("digest", Value::int(0)),
                        ]);
                    }
                    "reference_result_sha256" => {
                        *value = Value::str(
                            "2345439b3fcd5eeec57a4a399ca8f944d7172120478247cbeb53500ff6eee257",
                        );
                    }
                    _ => (),
                }
            }
        }
    }
    assert_eq!(sequences, 3);
    let prior = Value::Array(prior);
    assert_eq!(
        sha256::hex(crate::harness::manifest::canonical(&prior).as_bytes()),
        "70a05f8e72322c9b4966bc0751397f7cc06a7088c6819800cd22d35f1901ad6e"
    );
    for (row, (suffix, packets)) in rows[141..143].iter().zip([("eof", 2), ("lookahead", 3)]) {
        assert_eq!(
            text(row, "id").unwrap(),
            format!("au-dvb-ac3-aac-interleaved-{suffix}")
        );
        assert_eq!(
            row.get("expected").unwrap().get("packets"),
            Some(&Value::int(packets))
        );
    }
}

#[test]
fn interleaved_audio_preserves_source_order_clocks_and_complete_frames() {
    fn payload(packet: &[u8]) -> &[u8] {
        let at = 4 + if packet[3] & 0x20 != 0 {
            1 + usize::from(packet[4])
        } else {
            0
        };
        &packet[at..]
    }
    let ac3 = audio(root(), true).unwrap();
    let aac = audio(root(), false).unwrap();
    for second in [false, true] {
        let case = interleaved_audio_case(&ac3, &aac, second).unwrap();
        assert_eq!(
            case.row.get("arguments"),
            Some(&Value::strings(["188", "7", "--dvb"]))
        );
        let raw = case.bytes.unwrap();
        let pmt = payload(raw.chunks_exact(188).nth(1).unwrap());
        assert_eq!(&pmt[4..6], &PROGRAM.to_be_bytes());
        assert_eq!(
            &pmt[13..pmt.len() - 4],
            &[6, 0xe1, 0, 0xf0, 3, 0x6a, 1, 0, 15, 0xe1, 1, 0xf0, 0]
        );
        let mut pes: Vec<(u16, Vec<u8>)> = Vec::new();
        for packet in raw.chunks_exact(188).skip(2) {
            let pid = (u16::from(packet[1] & 31) << 8) | u16::from(packet[2]);
            if packet[1] & 0x40 != 0 {
                pes.push((pid, Vec::new()));
            }
            let (last_pid, bytes) = pes.last_mut().unwrap();
            assert_eq!(*last_pid, pid);
            bytes.extend_from_slice(payload(packet));
        }
        let mut expected = vec![(&ac3, PID, 90_000), (&aac, PID + 1, 6_000_000)];
        if second {
            expected.push((&ac3, PID, 92_880));
        }
        assert_eq!(pes.len(), expected.len());
        for ((pid, bytes), (codec, expected_pid, pts)) in pes.iter().zip(expected) {
            assert_eq!(*pid, expected_pid);
            assert_eq!(&bytes[..4], &[0, 0, 1, codec.stream_id]);
            assert_eq!(
                usize::from(u16::from_be_bytes([bytes[4], bytes[5]])) + 6,
                bytes.len()
            );
            assert_eq!(&bytes[6..9], &[0x80, 0x80, 5]);
            let time = &bytes[9..14];
            let raw_pts = (u64::from(time[0] & 0x0e) << 29)
                | (u64::from(time[1]) << 22)
                | (u64::from(time[2] & 0xfe) << 14)
                | (u64::from(time[3]) << 7)
                | u64::from(time[4] >> 1);
            assert_eq!(raw_pts, pts);
            assert_eq!(&bytes[14..], &codec.bytes[..codec.first]);
        }
    }
}

#[test]
fn coded_controls_keep_complete_source_bytes_and_distinct_pes_boundaries() {
    for codec in [
        video(root(), false).unwrap(),
        video(root(), true).unwrap(),
        audio(root(), false).unwrap(),
        audio(root(), true).unwrap(),
    ] {
        assert!(codec.first > codec.header);
        let one = &codec.bytes[..codec.first];
        let whole =
            authored_case(&codec, "whole", &[part(one, Some(90_000), None)], false).unwrap();
        let split = authored_case(
            &codec,
            "split",
            &[
                part(&one[..2], Some(90_000), None),
                part(&one[2..], None, None),
            ],
            false,
        )
        .unwrap();
        assert_ne!(whole.bytes, split.bytes);
        let m2ts = authored_case(&codec, "m2ts", &[part(one, Some(90_000), None)], true)
            .unwrap()
            .bytes
            .unwrap();
        assert_eq!(
            m2ts.chunks_exact(192)
                .flat_map(|p| p[4..].iter().copied())
                .collect::<Vec<_>>(),
            whole.bytes.unwrap()
        );
    }
}

#[test]
fn malformed_reference_results_do_not_gain_fabricated_work() {
    for observation in [
        Value::Null,
        Value::object([("status", Value::str("timeout"))]),
        Value::object([("exit", Value::int(2))]),
    ] {
        assert_eq!(expected(&observation), Value::Null);
    }
    let observation = Value::object([
        ("exit", Value::int(0)),
        (
            "data",
            Value::object([
                ("operation", Value::str(OPERATION)),
                ("iterations", Value::int(1)),
                ("packets", Value::int(0)),
                ("bytes", Value::int(0)),
                ("digest", Value::int(0)),
                ("eof", Value::Bool(false)),
            ]),
        ),
    ]);
    assert_eq!(expected(&observation), Value::Null);
    assert_eq!(
        observation.get("data").unwrap().get("eof"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn wrap_control_encodes_all33_source_clock_bits() {
    let authored = all_cases(root()).unwrap();
    let case = authored
        .iter()
        .find(|c| text(&c.row, "id").unwrap() == "au-avc-clock-wrap")
        .unwrap();
    let timestamps: Vec<_> = case
        .bytes
        .as_ref()
        .unwrap()
        .chunks_exact(188)
        .filter(|p| p[1] & 0x40 != 0 && p[1] & 31 == 1 && p[2] == 0)
        .map(|p| {
            let payload = 4 + if p[3] & 0x20 != 0 {
                1 + usize::from(p[4])
            } else {
                0
            };
            p[payload + 9..payload + 14].to_vec()
        })
        .collect();
    assert_eq!(
        timestamps,
        [
            vec![0x2f, 0xff, 0xff, 0xf8, 0x31],
            vec![0x21, 0, 1, 0x0f, 0xa1]
        ]
    );
}

#[test]
fn prefix_loading_uses_the_shared_report_and_keeps_all_required_profiles() {
    let feature = root().join("harness").join(FEATURE);
    let au = crate::harness::report::PrefixFeature::load(
        root(),
        &feature.join("contract.json"),
        &feature.join("corpus.json"),
        None,
    )
    .unwrap();
    assert_eq!(au.prefix, "ts-au-");
    assert_eq!(au.candidate, "transport-worker");
    assert_eq!(au.reference.as_deref(), Some("ffmpeg-formats-native"));
    assert_eq!(au.cases.len(), 179);
    let contract = json::read(&au.contract).unwrap();
    assert_eq!(
        contract.get("required_profiles"),
        Some(&Value::strings(
            au.cases.iter().map(|c| text(c, "profile").unwrap())
        ))
    );
    assert!(contract.get("open_cases").is_none());
    let old = crate::harness::report::PrefixFeature::load(
        root(),
        &root().join("harness/features/mpegts-pes/contract.json"),
        &root().join(ORIGINAL),
        None,
    )
    .unwrap();
    assert_eq!(old.prefix, "ts-");
    assert_eq!(old.candidate, "transport-worker");
    assert_eq!(old.cases.len(), 97);
}

#[test]
fn reference_pins_prove_multiple_aus_per_pes_and_keep_raw_logs_private() {
    let manifest = json::read(&root().join(format!("harness/{FEATURE}/corpus.json"))).unwrap();
    let rows = cases(&manifest).unwrap();
    for codec in ["avc", "hevc", "adts-aac-lc", "dvb-ac3"] {
        for (suffix, count) in [
            ("one", 1),
            ("multiple-per-pes", 2),
            ("header-across-pes", 1),
            ("body-across-pes", 1),
            ("final-au-at-eof", 2),
            ("m2ts192", 2),
        ] {
            let id = format!("au-{codec}-{suffix}");
            let row = rows.iter().find(|r| text(r, "id").unwrap() == id).unwrap();
            assert_eq!(
                row.get("expected").unwrap().get("packets"),
                Some(&Value::int(count)),
                "{id}"
            );
        }
    }
    for id in [
        "au-atsc-ac3-multiple-per-pes",
        "au-atsc-eac3-multiple-per-pes",
    ] {
        let row = rows.iter().find(|r| text(r, "id").unwrap() == id).unwrap();
        assert_eq!(
            row.get("arguments"),
            Some(&Value::strings(["188", "7", "--atsc"]))
        );
        assert_eq!(
            row.get("expected").unwrap().get("packets"),
            Some(&Value::int(2))
        );
    }
    for (suffix, count) in [
        ("two-opaque-pes", 1),
        ("opaque-then-coded", 2),
        ("coded-then-opaque", 2),
        ("opaque-prefix-in-pes", 2),
        ("opaque-suffix-in-pes", 2),
        ("recover-pat-pointer", 1),
        ("recover-pmt-pointer", 1),
    ] {
        let id = format!("au-adts-aac-lc-{suffix}");
        let row = rows.iter().find(|r| text(r, "id").unwrap() == id).unwrap();
        assert_eq!(
            row.get("expected").unwrap().get("packets"),
            Some(&Value::int(count))
        );
    }
    for row in rows {
        assert!(row.get("reference_observation").is_none());
        assert!(row.get("stdout").is_none() && row.get("stderr").is_none());
        let digest = text(row, "reference_result_sha256").unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    let opaque = rows
        .iter()
        .find(|r| text(r, "id").unwrap() == "ts188-basic")
        .unwrap();
    assert!(opaque.get("construction").is_none());
}

#[test]
fn stable_hash_excludes_log_addresses_and_keeps_completion_facts() {
    let mut observation = Value::object([
        ("status", Value::str("ok")),
        ("exit", Value::int(0)),
        ("stderr", Value::str("[h264 @ 0x1234] missing picture")),
        (
            "data",
            Value::object([
                ("operation", Value::str(OPERATION)),
                ("iterations", Value::int(1)),
                ("packets", Value::int(0)),
                ("bytes", Value::int(0)),
                ("digest", Value::int(0)),
                ("eof", Value::Bool(true)),
            ]),
        ),
    ]);
    let first = observation_hash(&observation);
    put(
        &mut observation,
        "stderr",
        Value::str("[h264 @ 0x9876] missing picture"),
    )
    .unwrap();
    assert_eq!(observation_hash(&observation), first);
    let mut data = observation.get("data").unwrap().clone();
    put(&mut data, "eof", Value::Bool(false)).unwrap();
    put(&mut observation, "data", data).unwrap();
    assert_ne!(observation_hash(&observation), first);
}

#[test]
fn program_carrier_preserves_packets_and_checks_sync_before_writing() {
    let dir = crate::walk::scratch("program-carrier");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join("input.ts");
    let dst = dir.join("output.m2ts");
    let mut input = vec![0x47; 376];
    input[187] = 0xaf;
    fs::write(&src, &input).unwrap();
    program_m2ts(&src, &dst).unwrap();
    let output = fs::read(&dst).unwrap();
    assert_eq!(&output[..4], &[0; 4]);
    assert_eq!(&output[4..192], &input[..188]);
    assert_eq!(&output[192..196], &2700u32.to_be_bytes());
    assert_eq!(&output[196..], &input[188..]);
    input[188] = 0;
    for invalid in [&input[..0], &input[..187], input.as_slice()] {
        fs::write(&src, invalid).unwrap();
        assert!(program_m2ts(&src, &dst).is_err());
        assert_eq!(fs::read(&dst).unwrap(), output);
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn authored_dvb_ac3_descriptor_rejects_missing_flags_byte() {
    let codec = audio(root(), true).unwrap();
    let mut ts = Transport::new();
    assert!(ts.tables(&codec, &[0x6a, 0]).is_err());
    assert!(ts.bytes.is_empty());
    ts.tables(&codec, &[0x6a, 1, 0]).unwrap();
    assert_eq!(ts.bytes.len(), 2 * 188);
}

#[test]
fn raw_recording_requires_a_fresh_directory_outside_the_repository() {
    let out = crate::walk::scratch("transport-private-observations");
    let repo = out.join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert!(private_directory(&repo, &repo.join("raw")).is_err());
    assert!(!repo.join("raw").exists());
    let private = out.join("raw");
    private_directory(&repo, &private).unwrap();
    assert!(private_directory(&repo, &private).is_err());
    fs::remove_dir_all(out).unwrap();
}

#[test]
fn pes_input_pin_separates_reference_folds_from_source_associations() {
    let manifest = json::read(&root().join(ORIGINAL)).unwrap();
    let mutate = |key, value| {
        let mut modified = manifest.clone();
        let Value::Object(fields) = &mut modified else {
            unreachable!()
        };
        let Some((_, Value::Array(rows))) = fields.iter_mut().find(|(key, _)| key == "cases")
        else {
            unreachable!()
        };
        put(&mut rows[0], key, value).unwrap();
        original_inputs(modified)
    };
    assert!(mutate("expected", Value::Null).is_ok());
    assert!(mutate("reference_refusal", Value::str("different PES result")).is_ok());
    assert!(mutate("sha256", Value::str("different source pin")).is_err());
    assert!(
        mutate(
            "arguments",
            Value::Array(vec![Value::str("192"), Value::str("1")])
        )
        .is_err()
    );
    let mut requests = cases(&manifest).unwrap()[0]
        .get("requests")
        .unwrap()
        .as_array()
        .unwrap()
        .to_vec();
    put(&mut requests[0], "ordinal", Value::int(272)).unwrap();
    assert!(mutate("requests", Value::Array(requests)).is_err());
}

#[test]
fn regenerating_the_au_corpus_preserves_every_pin_and_file_hash() {
    let out = crate::walk::scratch("transport-fixtures");
    let paths = Paths {
        root: root().to_path_buf(),
        out: out.clone(),
    };
    generate(&paths).unwrap();
    for name in ["contract.json", "corpus.json"] {
        let file = format!("harness/{FEATURE}/{name}");
        assert_eq!(
            fs::read(root().join(&file)).unwrap(),
            fs::read(out.join(&file)).unwrap()
        );
    }
    let manifest = json::read(&out.join(format!("harness/{FEATURE}/corpus.json"))).unwrap();
    for case in &cases(&manifest).unwrap()[97..] {
        let file = Path::new(text(case, "file").unwrap());
        let base = if file.starts_with(".research") {
            out.as_path()
        } else {
            root()
        };
        assert_eq!(
            sha256::hex(&fs::read(base.join(file)).unwrap()),
            text(case, "sha256").unwrap()
        );
    }
    fs::remove_dir_all(out).unwrap();
}
