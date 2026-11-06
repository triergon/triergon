#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, SourceLocation, Time, WorkBudget};
use triergon_subtitles::context::{
    Attachment, Builder, Context, Header, Identity, Limits, Metadata, Status,
};
use triergon_subtitles::text::{Encoding, Format, Input, Timing};
#[path = "context-fixtures/oracles.rs"]
mod oracles;

fn bytes(pool: &Budget, source: &[u8]) -> Bytes {
    Bytes::copy_from_slice(pool, source).unwrap()
}
fn header(pool: &Budget) -> Header {
    Header {
        identity: Identity {
            source: SourceId(53),
            generation: 7,
            configuration_epoch: 11,
            track_number: 5,
            track_uid: 0xfedc_ba98_7654_3210,
        },
        codec_id: bytes(pool, b"S_TEXT/ASS"),
        codec_private: Some(bytes(pool, oracles::ASS)),
        name: Some(bytes(pool, oracles::TRACK_NAME)),
        language: Some(bytes(pool, b"zho")),
        language_bcp47: Some(bytes(pool, b"zh-Hant-TW")),
        charset_hint: Some(bytes(pool, b"consumer hint / Big5")),
        position: Some(SourceLocation {
            source: SourceId(53),
            offset: 111,
        }),
    }
}
fn attachment(pool: &Budget) -> Attachment {
    Attachment {
        uid: 0x1234_5678_9abc_def0,
        name: bytes(pool, oracles::FONT_NAME),
        media_type: bytes(pool, b"application/octet-stream"),
        description: Some(bytes(pool, oracles::DESCRIPTION)),
        data: bytes(pool, oracles::FONT),
        position: Some(SourceLocation {
            source: SourceId(53),
            offset: 4097,
        }),
    }
}
fn metadata(pool: &Budget, value: &[u8]) -> Metadata {
    Metadata {
        key: bytes(pool, b"title"),
        value: bytes(pool, value),
        position: None,
    }
}
fn begin(pool: &Budget, limits: Limits) -> Builder {
    let mut builder = Builder::new(pool, limits);
    builder.provide_header(&mut Some(header(pool))).unwrap();
    builder
}
fn accept(builder: &mut Builder, quantum: u64) {
    for _ in 0..32 {
        match builder
            .poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
        {
            Status::NeedInput => return,
            Status::Yield => {}
            other => panic!("pending record: {other:?}"),
        }
    }
    panic!("record did not complete")
}
fn complete(builder: &mut Builder, quantum: u64) -> Context {
    builder.finish().unwrap();
    for _ in 0..32 {
        match builder
            .poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
        {
            Status::Ready => return builder.take_output().unwrap(),
            Status::Yield => {}
            other => panic!("context completion: {other:?}"),
        }
    }
    panic!("context did not complete")
}
fn packet(pool: &Budget) -> Input {
    Input {
        format: Format::OpaqueAss,
        encoding: Encoding::Opaque,
        payload: bytes(pool, oracles::PACKET),
        codec_id: None,
        codec_private: None,
        charset_label: None,
        addition: None,
        timing: Timing::exact(Some(Time::new(1, 3).unwrap()), None),
        source_id: SourceId(53),
        generation: 7,
        sequence: 29,
        configuration_epoch: 11,
        container_position: Some(SourceLocation {
            source: SourceId(53),
            offset: 9999,
        }),
    }
}

#[test]
fn complete_source_context_is_identical_at_work_one_seven_and_4096() {
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(1 << 20).unwrap();
        let baseline = pool.used();
        let original = header(&pool);
        let font = attachment(&pool);
        let mut builder = Builder::new(&pool, Limits::default());
        builder.provide_header(&mut Some(original.clone())).unwrap();
        builder.provide_attachment(&mut Some(font.clone())).unwrap();
        accept(&mut builder, quantum);
        builder
            .provide_metadata(&mut Some(metadata(&pool, b"Original title")))
            .unwrap();
        let result = complete(&mut builder, quantum);
        let out = result.header();
        assert_eq!(out.identity, original.identity);
        assert_eq!(out.codec_id.as_ref(), b"S_TEXT/ASS");
        assert_eq!(out.codec_private.as_ref().unwrap().as_ref(), oracles::ASS);
        assert_eq!(out.name.as_ref().unwrap().as_ref(), oracles::TRACK_NAME);
        assert_eq!(out.language.as_ref().unwrap().as_ref(), b"zho");
        assert_eq!(out.language_bcp47.as_ref().unwrap().as_ref(), b"zh-Hant-TW");
        assert_eq!(
            out.charset_hint.as_ref().unwrap().as_ref(),
            b"consumer hint / Big5"
        );
        assert_eq!(out.position, original.position);
        let got = &result.attachments()[0];
        assert_eq!(got.uid, font.uid);
        assert_eq!(got.name.as_ref(), oracles::FONT_NAME);
        assert_eq!(got.media_type.as_ref(), b"application/octet-stream");
        assert_eq!(got.data.as_ref(), oracles::FONT);
        assert_eq!(
            got.description.as_ref().unwrap().as_ref(),
            oracles::DESCRIPTION
        );
        assert_eq!(got.position, font.position);
        assert!(got.data.shares_storage_with(&font.data));
        assert!(
            out.codec_private
                .as_ref()
                .unwrap()
                .shares_storage_with(original.codec_private.as_ref().unwrap())
        );
        assert_eq!(result.metadata()[0].key.as_ref(), b"title");
        assert_eq!(result.metadata()[0].value.as_ref(), b"Original title");
        drop(result);
        drop(builder);
        drop(original);
        drop(font);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn absent_and_present_empty_configuration_and_hints_remain_distinct() {
    for present in [false, true] {
        let pool = Budget::new(1 << 20).unwrap();
        let mut h = header(&pool);
        let empty = present.then(|| bytes(&pool, b""));
        h.codec_private = empty.clone();
        h.name = empty.clone();
        h.language = empty.clone();
        h.language_bcp47 = empty.clone();
        h.charset_hint = empty;
        let mut builder = Builder::new(&pool, Limits::default());
        builder.provide_header(&mut Some(h)).unwrap();
        let out = complete(&mut builder, 1);
        for value in [
            &out.header().codec_private,
            &out.header().name,
            &out.header().language,
            &out.header().language_bcp47,
            &out.header().charset_hint,
        ] {
            assert_eq!(value.is_some(), present);
            assert!(value.as_ref().is_none_or(Bytes::is_empty));
        }
    }
}
#[test]
fn both_language_sources_are_carried_without_selection_or_default_synthesis() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut h = header(&pool);
    h.language = None;
    h.language_bcp47 = None;
    let mut builder = Builder::new(&pool, Limits::default());
    builder.provide_header(&mut Some(h)).unwrap();
    let out = complete(&mut builder, 7);
    assert!(out.header().language.is_none());
    assert!(out.header().language_bcp47.is_none());
}
#[test]
fn metadata_duplicate_keys_empty_values_and_order_survive() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut builder = begin(&pool, Limits::default());
    for value in [&b"first"[..], b"", b"second"] {
        builder
            .provide_metadata(&mut Some(metadata(&pool, value)))
            .unwrap();
        accept(&mut builder, 1);
    }
    let out = complete(&mut builder, 7);
    assert_eq!(out.metadata().len(), 3);
    for (value, expected) in out.metadata().iter().zip([&b"first"[..], b"", b"second"]) {
        assert_eq!(value.key.as_ref(), b"title");
        assert_eq!(value.value.as_ref(), expected);
    }
}
#[test]
fn unknown_attachment_and_raw_mime_are_preserved_in_source_order() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut builder = begin(&pool, Limits::default());
    let mut first = attachment(&pool);
    first.media_type = bytes(&pool, b"Font/TTF; source spelling");
    builder.provide_attachment(&mut Some(first)).unwrap();
    accept(&mut builder, 1);
    let mut second = attachment(&pool);
    second.uid = 3;
    second.name = bytes(&pool, b"opaque.data");
    second.data = bytes(&pool, oracles::UNKNOWN);
    second.media_type = bytes(&pool, b"application/x-unknown");
    builder.provide_attachment(&mut Some(second)).unwrap();
    let out = complete(&mut builder, 1);
    assert_eq!(out.attachments().len(), 2);
    assert_eq!(
        out.attachments()[0].media_type.as_ref(),
        b"Font/TTF; source spelling"
    );
    assert_eq!(out.attachments()[1].data.as_ref(), oracles::UNKNOWN);
    assert_eq!(out.attachments()[1].uid, 3);
}
#[test]
fn zero_copy_tiny_views_keep_their_complete_backing_charge() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut builder = begin(&pool, Limits::default());
    let root = bytes(&pool, &[0x5a; 8192]);
    let capacity = root.retained_storage_bytes();
    let mut font = attachment(&pool);
    font.data = root.slice(31..32).unwrap();
    builder.provide_attachment(&mut Some(font)).unwrap();
    let out = complete(&mut builder, 1);
    drop(root);
    let data = &out.attachments()[0].data;
    assert_eq!(data.as_ref(), b"Z");
    assert_eq!(data.retained_storage_bytes(), capacity);
    assert!(pool.used() >= capacity);
}
#[test]
fn attachment_and_config_alone_survive_every_original_root_and_context_drop() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let mut builder = begin(&pool, Limits::default());
    builder
        .provide_attachment(&mut Some(attachment(&pool)))
        .unwrap();
    let out = complete(&mut builder, 1);
    let font = out.attachments()[0].data.clone();
    let config = out.header().codec_private.clone().unwrap();
    drop(out);
    drop(builder);
    assert_eq!(font.as_ref(), oracles::FONT);
    assert_eq!(config.as_ref(), oracles::ASS);
    assert_eq!(
        pool.used(),
        baseline + font.retained_storage_bytes() + config.retained_storage_bytes()
    );
    drop(font);
    assert_eq!(pool.used(), baseline + config.retained_storage_bytes());
    drop(config);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn packet_identity_binding_preserves_exact_time_and_original_payload() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut builder = begin(&pool, Limits::default());
    let out = complete(&mut builder, 1);
    let input = packet(&pool);
    out.check_packet(&input, 5).unwrap();
    assert_eq!(input.payload.as_ref(), oracles::PACKET);
    assert_eq!(input.timing.start(), Some(Time::new(1, 3).unwrap()));
    assert!(input.timing.duration().is_none());
    assert_eq!(input.sequence, 29);
}
#[test]
fn binding_rejects_wrong_source() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let out = complete(&mut b, 1);
    let mut p = packet(&pool);
    p.source_id = SourceId(54);
    assert_eq!(out.check_packet(&p, 5), Err(Error::SourceMismatch));
}
#[test]
fn binding_rejects_wrong_parser_generation() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let out = complete(&mut b, 1);
    let mut p = packet(&pool);
    p.generation = 8;
    assert_eq!(
        out.check_packet(&p, 5),
        Err(Error::StaleGeneration {
            expected: 7,
            received: 8
        })
    );
}
#[test]
fn binding_rejects_wrong_configuration_epoch() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let out = complete(&mut b, 1);
    let mut p = packet(&pool);
    p.configuration_epoch = 12;
    assert!(matches!(out.check_packet(&p, 5), Err(Error::Invalid(_))));
}
#[test]
fn binding_rejects_other_track_number() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let out = complete(&mut b, 1);
    assert!(matches!(
        out.check_packet(&packet(&pool), 6),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn new_context_does_not_inherit_prior_attachments_or_metadata() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    b.provide_attachment(&mut Some(attachment(&pool))).unwrap();
    accept(&mut b, 1);
    b.provide_metadata(&mut Some(metadata(&pool, b"old")))
        .unwrap();
    let old = complete(&mut b, 1);
    let mut next = header(&pool);
    next.identity.configuration_epoch = 12;
    b.provide_header(&mut Some(next)).unwrap();
    let new = complete(&mut b, 1);
    assert_eq!(old.attachments().len(), 1);
    assert_eq!(old.metadata().len(), 1);
    assert!(new.attachments().is_empty());
    assert!(new.metadata().is_empty());
    assert_eq!(new.header().identity.configuration_epoch, 12);
    assert_eq!(old.header().identity.configuration_epoch, 11);
}
#[test]
fn no_header_or_unfinished_context_never_implies_end() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = Builder::new(&pool, Limits::default());
    assert_eq!(
        b.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Status::NeedInput
    );
    assert!(b.finish().is_err());
    b.provide_header(&mut Some(header(&pool))).unwrap();
    assert_eq!(
        b.poll(&mut WorkBudget::new(9), &AtomicBool::new(false))
            .unwrap(),
        Status::NeedInput
    );
    assert!(b.take_output().is_none());
}
#[test]
fn zero_work_does_not_accept_pending_record_or_publish_output() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    b.provide_attachment(&mut Some(attachment(&pool))).unwrap();
    b.finish().unwrap();
    let before = pool.used();
    let mut work = WorkBudget::new(0);
    assert_eq!(
        b.poll(&mut work, &AtomicBool::new(false)).unwrap(),
        Status::Yield
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(pool.used(), before);
    assert!(b.take_output().is_none());
    let recovered = b.recover();
    assert!(recovered.attachments.is_empty());
    assert!(recovered.pending_attachment.is_some());
    assert!(recovered.header.is_some());
}
#[test]
fn one_unit_per_record_and_publication_does_not_scan_attachment_payload() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let mut a = attachment(&pool);
    a.data = bytes(&pool, &[0x31; 16384]);
    b.provide_attachment(&mut Some(a)).unwrap();
    b.finish().unwrap();
    let mut first = WorkBudget::new(1);
    assert_eq!(
        b.poll(&mut first, &AtomicBool::new(false)).unwrap(),
        Status::Yield
    );
    assert_eq!(first.spent(), 1);
    let mut second = WorkBudget::new(1);
    assert_eq!(
        b.poll(&mut second, &AtomicBool::new(false)).unwrap(),
        Status::Ready
    );
    assert_eq!(second.spent(), 1);
    assert_eq!(b.take_output().unwrap().attachments()[0].data.len(), 16384);
}
#[test]
fn cancellation_precedes_work_and_recovers_all_committed_and_pending_owners() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let mut b = begin(&pool, Limits::default());
    b.provide_metadata(&mut Some(metadata(&pool, b"kept")))
        .unwrap();
    accept(&mut b, 1);
    b.provide_attachment(&mut Some(attachment(&pool))).unwrap();
    let mut work = WorkBudget::new(4096);
    assert_eq!(
        b.poll(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(work.spent(), 0);
    let recovered = b.recover();
    assert!(recovered.header.is_some());
    assert_eq!(recovered.metadata.as_slice()[0].value.as_ref(), b"kept");
    assert_eq!(
        recovered.pending_attachment.as_ref().unwrap().data.as_ref(),
        oracles::FONT
    );
    drop(recovered);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn allocation_pressure_retains_pending_record_and_retry_is_exact() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    b.provide_attachment(&mut Some(attachment(&pool))).unwrap();
    b.finish().unwrap();
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        b.poll(&mut WorkBudget::new(7), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert!(b.take_output().is_none());
    drop(pressure);
    let out = complete(&mut b, 1);
    assert_eq!(out.attachments().len(), 1);
    assert_eq!(out.attachments()[0].data.as_ref(), oracles::FONT);
}
#[test]
fn output_pressure_never_overwrites_or_spends_work() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    b.finish().unwrap();
    assert_eq!(
        b.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        Status::Ready
    );
    for _ in 0..3 {
        let mut work = WorkBudget::new(7);
        assert_eq!(
            b.poll(&mut work, &AtomicBool::new(false)).unwrap(),
            Status::NeedOutput
        );
        assert_eq!(work.spent(), 0);
    }
    let mut offered = Some(header(&pool));
    assert!(b.provide_header(&mut offered).is_err());
    assert!(offered.is_some());
    assert_eq!(b.take_output().unwrap().header().identity.track_number, 5);
    assert_eq!(
        b.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        Status::End
    );
}
#[test]
fn pending_attachment_excludes_another_record_without_consuming_it() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    b.provide_attachment(&mut Some(attachment(&pool))).unwrap();
    let mut offered = Some(metadata(&pool, b"not taken"));
    assert!(b.provide_metadata(&mut offered).is_err());
    assert_eq!(offered.unwrap().value.as_ref(), b"not taken");
}
#[test]
fn finish_disallows_further_offers_without_dropping_them() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    b.finish().unwrap();
    let mut offered = Some(attachment(&pool));
    assert!(b.provide_attachment(&mut offered).is_err());
    assert!(offered.is_some());
}
#[test]
fn foreign_header_pool_is_rejected_before_owner_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    let mut b = Builder::new(&pool, Limits::default());
    let mut offered = Some(header(&other));
    assert_eq!(b.provide_header(&mut offered), Err(Error::ForeignPool));
    assert!(offered.is_some());
}
#[test]
fn foreign_attachment_pool_is_rejected_before_owner_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let mut offered = Some(attachment(&other));
    assert_eq!(b.provide_attachment(&mut offered), Err(Error::ForeignPool));
    assert!(offered.is_some());
}
#[test]
fn foreign_metadata_pool_is_rejected_before_owner_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    let mut b = begin(&pool, Limits::default());
    let mut offered = Some(metadata(&other, b"foreign"));
    assert_eq!(b.provide_metadata(&mut offered), Err(Error::ForeignPool));
    assert!(offered.is_some());
}
#[test]
fn configuration_limit_rejects_before_header_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    let limits = Limits {
        max_config_bytes: oracles::ASS.len() - 1,
        ..Limits::default()
    };
    let mut b = Builder::new(&pool, limits);
    let mut offered = Some(header(&pool));
    assert!(matches!(
        b.provide_header(&mut offered),
        Err(Error::Limit { .. })
    ));
    assert_eq!(
        offered.unwrap().codec_private.unwrap().as_ref(),
        oracles::ASS
    );
}
#[test]
fn label_limit_rejects_before_header_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    let limits = Limits {
        max_label_bytes: 2,
        ..Limits::default()
    };
    let mut b = Builder::new(&pool, limits);
    let mut offered = Some(header(&pool));
    assert!(matches!(
        b.provide_header(&mut offered),
        Err(Error::Limit { .. })
    ));
    assert!(offered.is_some());
}
#[test]
fn attachment_byte_limit_rejects_before_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(
        &pool,
        Limits {
            max_attachment_bytes: oracles::FONT.len() - 1,
            ..Limits::default()
        },
    );
    let mut offered = Some(attachment(&pool));
    assert!(matches!(
        b.provide_attachment(&mut offered),
        Err(Error::Limit { .. })
    ));
    assert!(offered.is_some());
}
#[test]
fn total_visible_limit_rejects_a_record_without_losing_header() {
    let pool = Budget::new(1 << 20).unwrap();
    let h = header(&pool);
    let n = h.codec_id.len()
        + h.codec_private.as_ref().unwrap().len()
        + h.name.as_ref().unwrap().len()
        + h.language.as_ref().unwrap().len()
        + h.language_bcp47.as_ref().unwrap().len()
        + h.charset_hint.as_ref().unwrap().len();
    let mut b = Builder::new(
        &pool,
        Limits {
            max_visible_bytes: n,
            ..Limits::default()
        },
    );
    b.provide_header(&mut Some(h)).unwrap();
    let mut offered = Some(metadata(&pool, b"x"));
    assert!(matches!(
        b.provide_metadata(&mut offered),
        Err(Error::Limit { .. })
    ));
    assert!(offered.is_some());
    assert_eq!(complete(&mut b, 1).visible_bytes(), n);
}
#[test]
fn attachment_count_limit_is_not_an_unsupported_format_skip() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(
        &pool,
        Limits {
            max_attachments: 0,
            ..Limits::default()
        },
    );
    let mut offered = Some(attachment(&pool));
    assert_eq!(
        b.provide_attachment(&mut offered),
        Err(Error::Limit {
            requested: 1,
            maximum: 0
        })
    );
    assert!(offered.is_some());
}
#[test]
fn metadata_count_limit_preserves_the_pending_offer() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut b = begin(
        &pool,
        Limits {
            max_metadata: 0,
            ..Limits::default()
        },
    );
    let mut offered = Some(metadata(&pool, b"x"));
    assert_eq!(
        b.provide_metadata(&mut offered),
        Err(Error::Limit {
            requested: 1,
            maximum: 0
        })
    );
    assert!(offered.is_some());
}
#[test]
fn zero_matroska_uid_and_track_number_are_rejected_without_transfer() {
    let pool = Budget::new(1 << 20).unwrap();
    for number in [true, false] {
        let mut h = header(&pool);
        if number {
            h.identity.track_number = 0;
        } else {
            h.identity.track_uid = 0;
        }
        let mut offered = Some(h);
        let mut b = Builder::new(&pool, Limits::default());
        assert!(matches!(
            b.provide_header(&mut offered),
            Err(Error::Invalid(_))
        ));
        assert!(offered.is_some());
    }
    let mut b = begin(&pool, Limits::default());
    let mut a = attachment(&pool);
    a.uid = 0;
    let mut offered = Some(a);
    assert!(matches!(
        b.provide_attachment(&mut offered),
        Err(Error::Invalid(_))
    ));
    assert!(offered.is_some());
}
#[test]
fn source_locations_must_belong_to_the_context_source() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut h = header(&pool);
    h.position = Some(SourceLocation {
        source: SourceId(54),
        offset: 1,
    });
    let mut offered = Some(h);
    let mut b = Builder::new(&pool, Limits::default());
    assert_eq!(b.provide_header(&mut offered), Err(Error::SourceMismatch));
    assert!(offered.is_some());
    b.provide_header(&mut Some(header(&pool))).unwrap();
    let mut a = attachment(&pool);
    a.position = Some(SourceLocation {
        source: SourceId(54),
        offset: 1,
    });
    let mut offered = Some(a);
    assert_eq!(
        b.provide_attachment(&mut offered),
        Err(Error::SourceMismatch)
    );
    assert!(offered.is_some());
}
#[test]
fn opaque_bytes_are_never_recoded_trimmed_or_nul_terminated() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut h = header(&pool);
    h.charset_hint = Some(bytes(&pool, b"\xff\x00hint "));
    let mut b = Builder::new(&pool, Limits::default());
    b.provide_header(&mut Some(h)).unwrap();
    let out = complete(&mut b, 1);
    assert_eq!(
        out.header().charset_hint.as_ref().unwrap().as_ref(),
        b"\xff\x00hint "
    );
}
#[test]
fn physical_inline_state_has_explicit_ceilings() {
    assert!(size_of::<Header>() <= 512);
    assert!(size_of::<Attachment>() <= 256);
    assert!(size_of::<Metadata>() <= 128);
    assert!(size_of::<Context>() <= 768);
    assert!(size_of::<Builder>() <= 2048);
}
