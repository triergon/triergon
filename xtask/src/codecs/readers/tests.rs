use super::*;

fn packet(pts: i128) -> Fields {
    [None, None, Some(pts), Some(pts), Some(1536), None, None]
}

#[test]
fn projection_accumulates_rounded_durations_where_inexact() {
    // AC-3 at 44.1 kHz: 1536 samples are 3134.69 ticks of 1/90000.
    let mut packets = vec![packet(0), packet(1536), packet(3072)];
    project(&mut packets, (1, 44100), (1, 90000));
    let times: Vec<_> = packets.iter().map(|p| (p[PTS], p[DURATION])).collect();
    assert_eq!(
        times,
        [
            (Some(0), Some(3134)),
            (Some(3134), Some(3134)),
            (Some(6268), Some(3134))
        ]
    );
    // 1152 and 1536 samples at 44.1 kHz are exactly 368640 and 491520 ticks of 1/14112000.
    let mut packets = vec![packet(-1152), packet(0)];
    project(&mut packets, (1, 44100), (1, 14_112_000));
    assert_eq!(packets[0][PTS], Some(-368_640));
    assert_eq!(packets[1][DURATION], Some(491_520));
}

#[test]
fn alignment_pairs_unequal_keys_before_leaving_packets_unpaired() {
    let k = |v: &[i128]| v.iter().map(|&x| [Some(x), Some(1)]).collect::<Vec<_>>();
    // A leading reference packet, then equal positions.
    assert_eq!(
        align(&k(&[46, 880]), &k(&[0, 46, 880]), false),
        [(None, Some(0)), (Some(0), Some(1)), (Some(1), Some(2))]
    );
    // Shifted positions pair in order.
    assert_eq!(
        align(&k(&[10, 20]), &k(&[5, 15]), false),
        [(Some(0), Some(0)), (Some(1), Some(1))]
    );
    // Four reader packets one reference packet holds.
    assert_eq!(
        align(&k(&[0, 9, 10, 11, 12, 20]), &k(&[0, 9, 20]), false),
        [
            (Some(0), Some(0)),
            (Some(1), Some(1)),
            (Some(2), None),
            (Some(3), None),
            (Some(4), None),
            (Some(5), Some(2))
        ]
    );
}

fn reading(fields: Vec<Fields>, compat: Option<Vec<Fields>>) -> Candidate {
    let packets = fields
        .iter()
        .enumerate()
        .map(|(i, f)| Emitted {
            native: *f,
            compat: compat.as_ref().map(|c| c[i]),
            time_base: (1, 48000),
            first_of_track: i == 0,
            ..Emitted::default()
        })
        .collect();
    Candidate {
        tracks: vec![0],
        stream_of_track: vec![(0, 0)],
        streams: vec![(0, packets)],
        error: None,
        receipt: None,
    }
}

fn reference_of(fields: Vec<Fields>) -> Reference {
    Reference {
        tracks: vec![0],
        streams: vec![(0, fields)],
        time_bases: vec![(0, (1, 48000))],
        ..Reference::default()
    }
}

fn format_rules() -> FormatRules {
    let all = vec![SIZE, PTS, DTS, DURATION, TRIM_START, TRIM_END];
    FormatRules {
        streams: Streams::PositionInLink,
        media: vec!["audio".to_owned()],
        align_by: SIZE,
        apart: false,
        native: ViewRules {
            compare: all.clone(),
            duration_excludes_padding: true,
        },
        compat: Some(ViewRules {
            compare: all,
            duration_excludes_padding: false,
        }),
    }
}

#[test]
fn native_differences_the_compat_view_reproduces_are_set_apart() {
    // The last Opus packet: 960 samples, 936 of them padding; FFmpeg reports 24.
    let ffmpeg = vec![[
        Some(9),
        None,
        Some(0),
        Some(0),
        Some(24),
        Some(0),
        Some(936),
    ]];
    let native = vec![[Some(9), None, Some(0), Some(0), Some(960), None, Some(936)]];
    let ours = reading(native, Some(ffmpeg.clone()));
    let theirs = reference_of(ffmpeg);
    let rules = format_rules();
    let comparison = Comparison::new(&ours, &theirs, &rules);
    assert!(
        comparison
            .differences(View::Native, &rules.native)
            .is_empty()
    );
    assert!(
        comparison
            .differences(View::Compat, rules.compat.as_ref().unwrap())
            .is_empty()
    );
    // A late native time the compat view gets right.
    let late = vec![[Some(9), None, Some(5), Some(5), Some(960), None, Some(936)]];
    let ffmpeg = vec![[
        Some(9),
        None,
        Some(0),
        Some(0),
        Some(24),
        Some(0),
        Some(936),
    ]];
    let ours = reading(late, Some(ffmpeg.clone()));
    let theirs = reference_of(ffmpeg);
    let comparison = Comparison::new(&ours, &theirs, &rules);
    let native = comparison.differences(View::Native, &rules.native);
    assert_eq!(
        json::canonical(&Value::Array(native.iter().map(Difference::row).collect())),
        r#"[[0,0,0,"pts",5,0],[0,0,0,"dts",5,0]]"#
    );
    assert!(
        comparison
            .differences(View::Compat, rules.compat.as_ref().unwrap())
            .is_empty()
    );
}

#[test]
fn scopes_cover_only_their_fields_packets_and_values() {
    let rules = format_rules();
    let ours = reading(
        vec![
            [Some(1), None, Some(0), Some(0), Some(10), None, None],
            [Some(2), None, Some(10), Some(10), Some(-4), None, None],
        ],
        None,
    );
    let theirs = reference_of(vec![
        [Some(1), None, Some(0), Some(0), Some(10), None, None],
        [
            Some(2),
            None,
            Some(10),
            Some(10),
            Some((1 << 32) - 4),
            None,
            None,
        ],
    ]);
    let comparison = Comparison::new(&ours, &theirs, &rules);
    let found = comparison.differences(View::Native, &rules.native);
    assert_eq!(found.len(), 1);
    let scope = |fields: &[&str], packets, reference| Scope {
        views: vec![View::Native],
        fields: fields.iter().map(|f| (*f).to_owned()).collect(),
        packets,
        reference,
    };
    let covers = |s: &Scope| comparison.covers(s, View::Native, &found[0], &found);
    assert!(covers(&scope(
        &["duration"],
        Packets::LastOfStream,
        Relation::Unsigned32
    )));
    assert!(!covers(&scope(
        &["pts"],
        Packets::LastOfStream,
        Relation::Unsigned32
    )));
    assert!(!covers(&scope(
        &["duration"],
        Packets::FirstOfTrack,
        Relation::Any
    )));
    assert!(!covers(&scope(
        &["duration"],
        Packets::Any,
        Relation::ReferenceAbsent
    )));
    let mut compat_only = scope(&["duration"], Packets::Any, Relation::Any);
    compat_only.views = vec![View::Compat];
    assert!(!covers(&compat_only));
}

#[test]
fn resplit_and_shifted_times_cover_one_source_cut_differently() {
    // Bytes 0..30 are one reader packet and two reference packets of 10 samples
    // each; the next frame is paired, timed one reference packet later.
    let packet = |position, size, pts| -> Fields {
        [
            Some(size),
            Some(position),
            Some(pts),
            Some(pts),
            Some(10),
            None,
            None,
        ]
    };
    let mut rules = format_rules();
    rules.align_by = POSITION;
    rules.native.compare = vec![SIZE, POSITION, PTS];
    let ours = reading(vec![packet(0, 30, 0), packet(30, 5, 10)], None);
    let theirs = reference_of(vec![
        packet(0, 12, 0),
        packet(12, 18, 10),
        packet(30, 5, 20),
    ]);
    let comparison = Comparison::new(&ours, &theirs, &rules);
    let found = comparison.differences(View::Native, &rules.native);
    let rows: Vec<Value> = found.iter().map(Difference::row).collect();
    assert_eq!(
        json::canonical(&Value::Array(rows)),
        r#"[[0,0,0,"size",30,12],[0,null,1,"unmatched",null,18],[0,1,2,"pts",10,20]]"#
    );
    let scope = |fields: &[&str], packets, reference| Scope {
        views: vec![View::Native],
        fields: fields.iter().map(|f| (*f).to_owned()).collect(),
        packets,
        reference,
    };
    let resplit = scope(&["size", "unmatched"], Packets::Resplit, Relation::Any);
    let shifted = scope(&["pts"], Packets::Any, Relation::PlusUnmatchedDurations);
    let covered = |s: &Scope| {
        found
            .iter()
            .map(|d| comparison.covers(s, View::Native, d, &found))
            .collect::<Vec<_>>()
    };
    assert_eq!(covered(&resplit), [true, true, false]);
    assert_eq!(covered(&shifted), [false, false, true]);
}

#[test]
fn reviewed_lists_must_hash_to_their_digest_and_name_known_dispositions() {
    let rules = r#"{"formats": {"ogg": {"streams": "position-in-link", "align_by": "size",
            "views": {"native": {"compare": ["pts"]}}}},
            "dispositions": [{"id": "a", "reason": "why", "clause": "test clause", "scope": {"views": ["native"],
            "fields": ["pts"], "packets": "any"}}]}"#;
    let rows = r#"[[0, 0, 0, "pts", 1, 2]]"#;
    let digest = list_sha256(json::parse(rows).unwrap().as_array().unwrap());
    let differences = |id: &str, digest: &str| {
        format!(
            r#"{{"schema": "{DIFFERENCES_SCHEMA}", "cases": [{{"name": "n", "sha256": "s",
                "view": "native", "dispositions": ["{id}"], "difference_sha256": "{digest}",
                "differences": [[0, 0, 0, "pts", 1, 2, "{id}"]]}}]}}"#
        )
    };
    let parse = |d: String| Rules::parse(&json::parse(rules).unwrap(), &json::parse(&d).unwrap());
    assert!(parse(differences("a", &digest)).is_ok());
    assert!(parse(differences("a", "0")).is_err());
    assert!(parse(differences("b", &digest)).is_err());
    assert!(
        Rules::parse(
            &json::parse(&rules.replace(r#""packets": "any""#, r#""packets": "some""#)).unwrap(),
            &json::parse(&differences("a", &digest)).unwrap()
        )
        .is_err()
    );
}

#[test]
fn the_repository_rules_load() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let rules = Rules::load(&root).unwrap();
    for lane in [
        "reader-ogg",
        "reader-flac",
        "reader-mp3",
        "reader-aac",
        "reader-ac3",
        "reader-eac3",
    ] {
        rules.format(format(lane).unwrap()).unwrap();
        assert!(suffixes(lane).is_some());
    }
    let ogg = rules.format("ogg").unwrap();
    assert!(ogg.compat.is_some());
    assert_eq!(ogg.media, ["audio", "video"]);
    assert_eq!(rules.format("flac").unwrap().media, ["audio"]);
    let seeks = SeekCase::load(&root.join(SEEKS)).unwrap();
    assert_eq!(seeks.len(), SEEK_CASES);
    let manifest = json::read(&root.join(MANIFEST)).unwrap();
    let vectors = manifest.get("vectors").and_then(Value::as_array).unwrap();
    let ogg_vectors = vectors
        .iter()
        .filter(|v| v.get("lane").and_then(Value::as_str) == Some("reader-ogg"))
        .count();
    assert_eq!(ogg_vectors, OGG_CASES);
    assert_eq!(vectors.len(), READER_CASES);
    let mut ogg_seeks = 0;
    for seek in &seeks {
        let lane = vectors
            .iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some(seek.input.as_str()))
            .and_then(|v| v.get("lane"))
            .and_then(Value::as_str);
        assert!(
            matches!(
                lane,
                Some("reader-ogg" | "reader-flac" | "reader-mp3" | "reader-aac" | "reader-ac3")
            ),
            "{}",
            seek.name
        );
        ogg_seeks += usize::from(lane == Some("reader-ogg"));
    }
    assert_eq!(ogg_seeks, 26);
    assert_eq!(seeks.iter().filter(|case| case.open.is_some()).count(), 33);
}

/// The committed manifests' case counts, raised with every case added.
const READER_CASES: usize = 318;
const OGG_CASES: usize = 45;
const SEEK_CASES: usize = 92;
const MANIFEST: &str = "harness/codecs/reader-manifest.json";
const SEEKS: &str = "harness/codecs/reader-seeks.json";

#[test]
fn seek_cases_name_decimal_targets_and_their_reference_target() {
    let case = |policy: &str, seconds: &str, reference: &str| {
        json::parse(&format!(
            r#"{{"schema": "{SEEKS_SCHEMA}", "contract": "c", "cases": [{{"name": "n",
                "input": "a.opus", "track": 0, "seconds": "{seconds}", "policy": "{policy}",
                "reference_seconds": "{reference}"}}]}}"#
        ))
        .unwrap()
    };
    assert!(SeekCase::parse(&case("exact", "1.5", "1.5")).is_ok());
    assert!(SeekCase::parse(&case("preroll", "1.5", "1.42")).is_ok());
    assert!(SeekCase::parse(&case("exact", "1.5", "1.42")).is_err());
    assert!(SeekCase::parse(&case("exact", "-1", "-1")).is_err());
    assert!(SeekCase::parse(&case("nearest", "1", "1")).is_err());
}

#[test]
fn a_seek_must_land_at_or_before_its_target() {
    let seek = SeekCase {
        name: "n".into(),
        input: "a.opus".into(),
        track: 0,
        seconds: "1.5".into(),
        policy: "exact".into(),
        reference_seconds: "1.5".into(),
        open: None,
    };
    // Packets of 48 kHz ticks; the receipt in seconds.
    let landed = |landing: &str, first: i128| {
        let mut ours = reading(
            vec![[Some(9), Some(0), Some(first), None, None, None, None]],
            None,
        );
        ours.receipt = Some(
            json::parse(&format!(
                r#"{{"event":"seek","requested":[3,2],"landing":{landing}}}"#
            ))
            .unwrap(),
        );
        landing_check(&ours, &seek)
    };
    assert!(landed("[1,1]", 48_000).is_ok());
    assert!(landed("[3,2]", 72_000).is_ok());
    // The receipt or the first packet past the target.
    assert!(landed("[2,1]", 48_000).is_err());
    assert!(landed("[1,1]", 72_001).is_err());
    assert!(landed("[1,0]", 48_000).is_err());
    // A chain: the seek to track 0 delivers track 1, the same stream in the second
    // link, timed from the start of the file.
    let chained = |first: i128| {
        let mut ours = reading(
            vec![[Some(9), Some(0), Some(first), None, None, None, None]],
            None,
        );
        ours.stream_of_track = vec![(0, 0), (1, 0)];
        ours.receipt = Some(json::parse(r#"{"requested":[3,2],"landing":[1,1]}"#).unwrap());
        ours
    };
    assert!(landing_check(&chained(48_000), &seek).is_ok());
    assert!(landing_check(&chained(72_001), &seek).is_err());
    // A track of no compared stream, and a stream that delivers nothing.
    let mut unmapped = chained(48_000);
    unmapped.stream_of_track = vec![(1, 0)];
    assert!(landing_check(&unmapped, &seek).is_err());
    let mut silent = chained(48_000);
    silent.streams.clear();
    assert!(landing_check(&silent, &seek).is_err());
}

#[test]
fn ffmpeg_packets_before_the_reader_landing_are_counted_not_compared() {
    let packet = |position: i128, size: i128| {
        [
            Some(size),
            Some(position),
            Some(0),
            Some(0),
            Some(960),
            None,
            None,
        ]
    };
    let ours = reading(vec![packet(300, 7), packet(300, 8)], None);
    let mut theirs = reference_of(vec![
        packet(100, 5),
        packet(300, 6),
        packet(300, 7),
        packet(300, 8),
    ]);
    assert_eq!(before_landing(&ours, &mut theirs), [(0, 2)]);
    assert_eq!(theirs.streams[0].1.len(), 2);
    // FFmpeg landing later leaves the reader's extra packets to compare.
    let mut later = reference_of(vec![packet(300, 8)]);
    assert!(before_landing(&ours, &mut later).is_empty());
    // Packets starting after the reader's landing are not FFmpeg's earlier landing.
    let mut after = reference_of(vec![packet(400, 7), packet(400, 8)]);
    assert!(before_landing(&ours, &mut after).is_empty());
    // Packets apart in every key never pair.
    let k = |v: &[i128]| v.iter().map(|&x| [Some(x), Some(x)]).collect::<Vec<Key>>();
    assert_eq!(
        align(&k(&[1]), &k(&[2]), true),
        [(Some(0), None), (None, Some(0))]
    );
    assert_eq!(align(&k(&[1]), &k(&[2]), false), [(Some(0), Some(0))]);
    // Of equal sizes, the run that keeps as many packets as the reader's.
    let same = reading(vec![packet(300, 7), packet(300, 7)], None);
    let mut runs = reference_of(vec![packet(300, 7), packet(300, 7), packet(300, 7)]);
    assert_eq!(before_landing(&same, &mut runs), [(0, 1)]);
    // FFmpeg stopping early still lands on the reader's first packet.
    let mut short = reference_of(vec![packet(100, 5), packet(300, 7)]);
    assert_eq!(before_landing(&ours, &mut short), [(0, 1)]);
}
