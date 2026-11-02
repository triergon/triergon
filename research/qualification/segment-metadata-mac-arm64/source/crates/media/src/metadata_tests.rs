use super::*;
fn e(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut result: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (bytes.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    result.extend(((1_u64 << (7 * width)) | bytes.len() as u64).to_be_bytes()[8 - width..].iter());
    result.extend(bytes);
    result
}
fn u(id: u64, n: u64) -> Vec<u8> {
    e(id, &n.to_be_bytes())
}
fn file(uid: u64, bytes: &[u8]) -> Vec<u8> {
    e(
        0x61a7,
        &[
            u(0x46ae, uid),
            e(0x466e, b"font.ttf"),
            e(0x4660, b"application/x-truetype-font"),
            e(0x465c, bytes),
        ]
        .concat(),
    )
}
fn parse(kind: u64, bytes: Vec<u8>) -> Result<Document> {
    Document::parse(kind, bytes, 4096, 32, 16 * 1024 * 1024, 8 * 1024 * 1024)
}
#[test]
fn attachments_share_one_owned_source_after_reader_owner_is_gone() {
    let d = Arc::new(parse(ATTACHMENTS, [file(1, b"font"), file(2, b"more")].concat()).unwrap());
    let files: Vec<_> = d.attachments().collect();
    let retained = d.retained_bytes();
    let clone = files[0].clone();
    drop(d);
    assert!(files[0].shares_storage_with(&files[1]));
    assert!(clone.shares_storage_with(&files[0]));
    assert_eq!(clone.data().unwrap(), b"font");
    assert_eq!(clone.uid().unwrap(), 1);
    assert_eq!(files[1].filename().unwrap(), "font.ttf");
    assert_eq!(
        files[1].media_type().unwrap(),
        "application/x-truetype-font"
    );
    assert!(retained < 4096);
}
#[test]
fn structural_attachment_node_and_depth_limits_are_independent() {
    let payload = file(1, &[0; 1024]);
    assert!(matches!(
        Document::parse(
            ATTACHMENTS,
            payload.clone(),
            4096,
            32,
            16 * 1024 * 1024,
            1023
        ),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        Document::parse(ATTACHMENTS, payload.clone(), 2, 32, 16 * 1024 * 1024, 1024),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        Document::parse(
            ATTACHMENTS,
            payload.clone(),
            4096,
            0,
            16 * 1024 * 1024,
            1024
        ),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        Document::parse(ATTACHMENTS, payload.clone(), 4096, 32, 1024, 1024),
        Err(Error::Limit(_))
    ));
    let mut c = Catalogue::default();
    let limits = crate::Limits::default();
    assert!(
        c.add(ATTACHMENTS, payload.clone(), 0, None, limits)
            .is_err()
    );
    assert!(c.documents().is_empty());
    assert!(c.add(ATTACHMENTS, payload, 512, None, limits).is_ok());
    assert_eq!(c.attachment_bytes(), 1024);
}
#[test]
fn invalid_cardinality_uids_utf8_binary_and_lengths_fail() {
    for payload in [
        Vec::new(),
        e(0x61a7, &[]),
        [file(1, b"a"), file(1, b"b")].concat(),
        e(
            0x61a7,
            &[
                u(0x46ae, 0),
                e(0x466e, b"x"),
                e(0x4660, b"font"),
                e(0x465c, b""),
            ]
            .concat(),
        ),
        e(
            0x61a7,
            &[
                u(0x46ae, 1),
                e(0x466e, b"\xff"),
                e(0x4660, b"font"),
                e(0x465c, b""),
            ]
            .concat(),
        ),
        vec![0x61, 0xa7, 0xff],
        vec![0x61, 0xa7, 0x82, 0x46],
    ] {
        assert!(parse(ATTACHMENTS, payload).is_err());
    }
    let tag = e(
        0x7373,
        &e(
            0x67c8,
            &[e(0x45a3, b"X"), e(0x4487, b"text"), e(0x4485, b"bin")].concat(),
        ),
    );
    assert!(parse(TAGS, tag).is_err());
    assert!(
        parse(
            TAGS,
            e(
                0x7373,
                &e(0x67c8, &[e(0x45a3, b"X"), u(0x4484, 2)].concat())
            )
        )
        .is_err()
    );
    assert!(
        parse(
            TAGS,
            e(
                0x7373,
                &e(
                    0x67c8,
                    &[e(0x45a3, b"X"), e(0x447a, b"en\0hidden")].concat()
                )
            )
        )
        .is_err()
    );
}
#[test]
fn complete_nested_source_and_defaults_remain_available() {
    let tag = e(
        0x7373,
        &[
            e(0x63c0, &e(0x68ca, &[])),
            e(
                0x67c8,
                &[
                    e(0x45a3, b"GROUP"),
                    e(0x4484, &[]),
                    e(
                        0x67c8,
                        &[e(0x45a3, b"BINARY"), e(0x4485, &[0, 255])].concat(),
                    ),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let d = parse(TAGS, tag.clone()).unwrap();
    assert_eq!(d.raw(), tag);
    let t = d.root().field(0x7373).unwrap();
    assert_eq!(t.field(0x63c0).unwrap().number(0x68ca).unwrap(), Some(50));
    let simple = t.field(0x67c8).unwrap();
    assert_eq!(simple.string(0x45a3).unwrap(), Some("GROUP"));
    assert_eq!(simple.number(0x4484).unwrap(), Some(1));
    assert_eq!(
        simple.field(0x67c8).unwrap().field(0x4485).unwrap().raw(),
        [0, 255]
    );
}
#[test]
fn ordered_leaf_and_nested_chapter_ranges_obey_rfc9559() {
    let atom = |id, start, end: Option<u64>, extra: Vec<u8>| {
        e(
            0xb6,
            &[
                u(0x73c4, id),
                u(0x91, start),
                end.map_or(Vec::new(), |v| u(0x92, v)),
                extra,
            ]
            .concat(),
        )
    };
    let leaf = atom(2, 10, Some(20), Vec::new());
    let parent = atom(1, 0, None, leaf);
    assert!(parse(CHAPTERS, e(0x45b9, &[u(0x45dd, 1), parent].concat())).is_ok());
    assert!(
        parse(
            CHAPTERS,
            e(
                0x45b9,
                &[u(0x45dd, 1), atom(1, 0, None, Vec::new())].concat()
            )
        )
        .is_err()
    );
    assert!(parse(CHAPTERS, e(0x45b9, &atom(1, 20, Some(10), Vec::new()))).is_err());
    assert!(
        parse(
            CHAPTERS,
            e(0x45b9, &atom(1, 10, None, atom(2, 0, None, Vec::new())))
        )
        .is_err()
    );
}

#[test]
fn cumulative_arena_limits_and_absolute_depth_remain_bounded() {
    let tag = e(
        0x7373,
        &e(0x67c8, &[e(0x45a3, b"X"), e(0x4487, b"Y")].concat()),
    );
    let mut catalogue = Catalogue::default();
    let limits = crate::Limits {
        metadata_nodes: 9,
        ..crate::Limits::default()
    };
    catalogue
        .add(TAGS, tag.clone(), 1024, None, limits)
        .unwrap();
    assert!(matches!(
        catalogue.add(TAGS, tag, 1024, None, limits),
        Err(Error::Limit(_))
    ));
    assert_eq!(catalogue.documents().len(), 1);
    let mut nested = e(0x67c8, &e(0x45a3, b"LEAF"));
    for _ in 0..100 {
        nested = e(0x67c8, &[e(0x45a3, b"PARENT"), nested].concat());
    }
    assert!(matches!(
        Document::parse(
            TAGS,
            e(0x7373, &nested),
            4096,
            usize::MAX,
            16 * 1024 * 1024,
            8 * 1024 * 1024
        ),
        Err(Error::Limit("metadata nesting"))
    ));
}

#[test]
fn info_preserves_signed_dates_and_exact_float_values() {
    let raw = [
        e(0x4461, &(-1001_i64).to_be_bytes()),
        e(0x4489, &0.1_f32.to_be_bytes()),
        e(0x7ba9, "Schloß".as_bytes()),
    ]
    .concat();
    let doc = Document::parse(INFO, raw.clone(), 4096, 32, 16 * 1024 * 1024, 0).unwrap();
    let info = doc.info().unwrap();
    assert_eq!(info.date_utc_ns().unwrap(), Some(-1001));
    assert_eq!(info.duration_ticks().unwrap(), Some(f64::from(0.1_f32)));
    assert_eq!(info.title().unwrap(), Some("Schloß"));
    assert_eq!(info.timestamp_scale_ns().unwrap(), 1_000_000);
    assert_eq!(doc.raw(), raw);
    for data in [
        e(0x4461, &[1, 2, 3]),
        e(0x4489, &f64::NAN.to_be_bytes()),
        e(0x4489, &(-1_f64).to_be_bytes()),
    ] {
        assert!(Document::parse(INFO, data, 4096, 32, 16 * 1024 * 1024, 0).is_err());
    }
}
