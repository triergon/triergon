mod common;
use common::{child, children, roots, table};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_isobmff::{
    ConfigurationLimits, SampleTable, TrackHeader,
    compat::mov::{
        encoded::{
            EncodedCreationSource, EncodedMetadataProfile, EncodedSelection, EncodedStream,
            EncodedStreamJob,
        },
        packets::CursorStatus,
    },
};

const WAVE: &[u8] = include_bytes!("fixtures/encoded-metadata-v4/qt-v1-wave.mov");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    length: u64,
}
impl Source {
    fn new(b: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let top = roots(&pool, b).unwrap();
        let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
        let track = children(movie, 0)
            .unwrap()
            .into_iter()
            .find(|x| x.header().kind == *b"trak")
            .unwrap();
        let mdia = child(&track, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = table(
            child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
            &pool,
            1,
        )
        .unwrap();
        Self {
            pool,
            header,
            table,
            length: b.len() as u64,
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(1911),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn job(&self, input: &InputCursor) -> Result<EncodedStreamJob<'_>, Error> {
        EncodedStreamJob::new_description_with_metadata(
            &self.header,
            &self.table,
            input,
            EncodedSelection {
                major_brand: Some(*b"qt  "),
                description: 1,
                limits: ConfigurationLimits::default(),
            },
            EncodedMetadataProfile,
        )
    }
}
fn finish(
    job: &mut EncodedStreamJob<'_>,
    input: &mut InputCursor,
    q: u64,
) -> Result<EncodedStream, Error> {
    for _ in 0..20000 {
        let mut work = WorkBudget::new(q);
        let status = job.poll(input, &mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= q);
        if status == CursorStatus::Ready {
            return job.take(input, &AtomicBool::new(false));
        }
    }
    panic!("bounded sound metadata progress");
}
#[test]
fn wave_v1_retains_words_and_full_backing_without_retyping_audio_v0() {
    let s = Source::new(WAVE);
    let mut i = s.input();
    let before = s.pool.used();
    let result = finish(&mut s.job(&i).unwrap(), &mut i, 1).unwrap();
    let source = result.receiver_source().unwrap();
    let sound = source.sound.as_ref().unwrap();
    assert_eq!(
        (
            sound.samples_per_packet,
            sound.bytes_per_packet,
            sound.bytes_per_frame,
            sound.bytes_per_sample
        ),
        (1024, 0, 0, 2)
    );
    assert_eq!(
        result.demux().extradata.as_ref(),
        [0x11, 0x88, 0x56, 0xe5, 0]
    );
    assert_eq!(result.demux().language, *b"eng");
    assert_eq!(source.language.raw_word, 0);
    assert_eq!(result.demux().handler_name.as_ref(), b"SoundHandler");
    assert!(source.handler.source_pascal);
    assert_eq!(result.demux().vendor_id, Some([0; 4]));
    assert_eq!(source.wave.as_ref().unwrap().header().kind, *b"wave");
    assert_eq!(result.configuration_box().header().kind, *b"esds");
    assert_eq!(
        result.source_description().raw().bytes().as_ref(),
        s.table.descriptions()[0].raw().bytes().as_ref()
    );
    assert_eq!(
        s.pool.used(),
        before,
        "metadata borrows charged owners rather than allocating a copy"
    );
}
#[test]
fn unknown_wave_child_is_retained_and_work_one_matches_bulk() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/qt-v1-unknown.mov"
    ));
    let mut a = s.input();
    let one = finish(&mut s.job(&a).unwrap(), &mut a, 1).unwrap();
    let mut b = s.input();
    let bulk = finish(&mut s.job(&b).unwrap(), &mut b, 4096).unwrap();
    let raw = one
        .receiver_source()
        .unwrap()
        .wave
        .as_ref()
        .unwrap()
        .bytes()
        .as_ref();
    assert!(raw.windows(8).any(|x| x == b"original"));
    assert_eq!(
        raw,
        bulk.receiver_source()
            .unwrap()
            .wave
            .as_ref()
            .unwrap()
            .bytes()
            .as_ref()
    );
    assert_eq!(
        one.demux().extradata.as_ref(),
        bulk.demux().extradata.as_ref()
    );
}
#[test]
fn zero_work_changes_no_backing_input_or_result() {
    let s = Source::new(WAVE);
    let mut i = s.input();
    let mut job = s.job(&i).unwrap();
    let used = s.pool.used();
    let position = i.position();
    let mut work = WorkBudget::new(0);
    assert_eq!(
        job.poll(&mut i, &mut work, &AtomicBool::new(false))
            .unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(work.spent(), 0);
    assert_eq!(i.position(), position);
    assert_eq!(s.pool.used(), used);
    assert_eq!(finish(&mut job, &mut i, 1).unwrap().demux().codec, "aac");
}
#[test]
fn cancellation_is_prior_to_work_and_sticky() {
    let s = Source::new(WAVE);
    let mut i = s.input();
    let mut j = s.job(&i).unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn transport_failure_is_not_empty_metadata() {
    let s = Source::new(WAVE);
    let mut i = s.input();
    let mut j = s.job(&i).unwrap();
    i.fail(i.source(), i.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn logical_reset_is_not_a_configuration_epoch() {
    let s = Source::new(WAVE);
    let mut i = s.input();
    let mut j = s.job(&i).unwrap();
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn foreign_pool_is_rejected_at_constructor() {
    let s = Source::new(WAVE);
    let pool = Budget::new(4096).unwrap();
    let i = InputCursor::new(
        &pool,
        SourceId(1911),
        SourceCapabilities {
            length: Some(s.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(s.job(&i), Err(Error::ForeignPool)));
}
#[test]
fn retained_config_keeps_original_source_charge_after_source_drop() {
    let s = Source::new(WAVE);
    let pool = s.pool.clone();
    let mut i = s.input();
    let result = finish(&mut s.job(&i).unwrap(), &mut i, 1).unwrap();
    let retained = result.demux().extradata.clone();
    let capacity = retained.retained_capacity();
    drop(result);
    drop(i);
    drop(s);
    assert!(pool.used() >= capacity);
    assert!(capacity > retained.len());
    drop(retained);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn outer_and_nested_extensions_share_one_total_limit() {
    let s = Source::new(WAVE);
    let mut i = s.input();
    let mut job = EncodedStreamJob::new_description_with_metadata(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(*b"qt  "),
            description: 1,
            limits: ConfigurationLimits {
                max_child_boxes: 4,
                ..ConfigurationLimits::default()
            },
        },
        EncodedMetadataProfile,
    )
    .unwrap();
    assert_eq!(
        finish(&mut job, &mut i, 1).unwrap_err(),
        Error::Limit {
            requested: 5,
            maximum: 4
        }
    );
    assert_eq!(s.table.descriptions()[0].raw().payload()[8..10], [0, 1]);
}
#[test]
fn empty_iso_handler_is_source_owned_but_receiver_tag_is_absent() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/creation-v0-1578905043.mov"
    ));
    let mut i = s.input();
    let mut j = EncodedStreamJob::new_description_with_metadata(
        &s.header,
        &s.table,
        &i,
        EncodedSelection {
            major_brand: Some(*b"mp42"),
            description: 1,
            limits: ConfigurationLimits::default(),
        },
        EncodedMetadataProfile,
    )
    .unwrap();
    let r = finish(&mut j, &mut i, 1).unwrap();
    let source = r.receiver_source().unwrap();
    assert!(source.handler.demux.is_none());
    assert!(!source.handler.source_pascal);
    assert_eq!(source.handler.raw().payload().len(), 25);
    assert_eq!(source.language.raw_word, 0);
    assert_eq!(r.demux().language, *b"eng");
    assert!(r.demux().handler_name.is_empty());
    assert_eq!(r.demux().vendor_id, None);
}
macro_rules! refused {
    ($name:ident,$file:literal) => {
        #[test]
        fn $name() {
            let s = Source::new(include_bytes!(concat!(
                "fixtures/encoded-metadata-v4/",
                $file,
                ".mov"
            )));
            let mut i = s.input();
            let error = finish(&mut s.job(&i).unwrap(), &mut i, 1).unwrap_err();
            assert!(
                matches!(
                    error,
                    Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated
                ),
                "{error:?}"
            );
        }
    };
}
refused!(duplicate_frma_is_not_first_wins, "qt-v1-duplicate-frma");
refused!(missing_frma_is_not_guessed_from_entry, "qt-v1-missing-frma");
refused!(conflicting_frma_is_not_relabelled, "qt-v1-wrong-frma");
refused!(
    missing_terminator_remains_explicit,
    "qt-v1-missing-terminator"
);
refused!(terminator_payload_is_rejected, "qt-v1-terminator-body");
refused!(
    child_after_terminator_is_not_silently_skipped,
    "qt-v1-after-terminator"
);
refused!(
    duplicate_esds_remains_a_second_fault,
    "qt-v1-duplicate-esds"
);
#[test]
fn uncompressed_sample_table_meaning_is_not_used_as_compressed_frames() {
    let s = Source::new(include_bytes!(
        "fixtures/encoded-metadata-v4/qt-v1-unframed-compression.mov"
    ));
    let i = s.input();
    assert!(matches!(
        s.job(&i),
        Err(Error::Unsupported(
            "encoded sound-v1 frame-table compression ID"
        ))
    ));
}
macro_rules! date {
    ($name:ident,$file:literal,$word:expr,$expected:expr) => {
        #[test]
        fn $name() {
            let s = Source::new(include_bytes!(concat!(
                "fixtures/encoded-metadata-v4/",
                $file,
                ".mov"
            )));
            let c = EncodedCreationSource::from_source(&s.header, EncodedMetadataProfile).unwrap();
            assert_eq!(c.source_seconds(), $word);
            assert_eq!(c.demux().map(|x| x.as_slice()), $expected);
            assert_eq!(c.raw().bytes().as_ref(), s.header.raw_mdhd.bytes().as_ref());
        }
    };
}
date!(zero_creation_has_no_tag, "creation-v0-0", 0, None);
date!(
    v0_low_word_is_explicit_receiver_unix_observation,
    "creation-v0-1578905043",
    1578905043,
    Some(b"2020-01-13T08:44:03.000000Z".as_slice())
);
date!(
    v0_below_epoch_boundary,
    "creation-v0-2082844799",
    2082844799,
    Some(b"2036-01-01T23:59:59.000000Z".as_slice())
);
date!(
    v0_exact_epoch_boundary,
    "creation-v0-2082844800",
    2082844800,
    Some(b"1970-01-01T00:00:00.000000Z".as_slice())
);
date!(
    v0_maximum_word,
    "creation-v0-4294967295",
    4294967295,
    Some(b"2040-02-06T06:28:15.000000Z".as_slice())
);
date!(
    v1_low_word_keeps_quicktime_epoch,
    "creation-v1-1",
    1,
    Some(b"1904-01-01T00:00:01.000000Z".as_slice())
);
date!(
    v1_one_before_epoch,
    "creation-v1-2082844799",
    2082844799,
    Some(b"1969-12-31T23:59:59.000000Z".as_slice())
);
date!(
    v1_microseconds_overflow_omits_only_receiver_tag,
    "creation-v1-9223372036854775807",
    9223372036854775807,
    None
);
date!(
    v1_sign_bit_keeps_unsigned_source_word,
    "creation-v1-9223372036854775808",
    9223372036854775808,
    None
);
