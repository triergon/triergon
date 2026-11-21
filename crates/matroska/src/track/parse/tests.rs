use super::*;
use crate::ebml_kit::{ABC, content_encoding, element, track, track_entry, uint, zlib_encoding};
use crate::packet::batch::PacketBatchJob;

#[test]
fn raw_tracks_job_builds_configuration_with_bounded_work_and_original_source_views() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let entry = track_entry(
        1,
        1,
        &[
            element(0x536e, b"authored name"),
            element(0x63a2, b"private"),
            element(0x22b59d, b"fr-CA"),
        ]
        .concat(),
    );
    let raw = element(0xae, &entry);
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job =
        TracksJob::new(source.clone(), None, Limits::default(), 1024, &pool, 1, 1).unwrap();
    let mut result = None;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("raw Tracks: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    let track = &result.tracks()[0];
    assert_eq!(track.source_metadata.as_ref(), entry.as_slice());
    assert!(track.codec_private.shares_storage_with(&source));
    assert_eq!(track.name.as_str(), "authored name");
    assert_eq!(track.effective_language(), "fr-CA");
    let at = track.name.as_ptr() as usize;
    let start = source.as_ptr() as usize;
    assert!(at >= start && at + track.name.len() <= start + source.len());
    let alias = result.clone();
    drop((source, result, job));
    assert_eq!(alias.tracks()[0].codec_private.as_ref(), b"private");
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn raw_tracks_job_keeps_late_duplicate_and_decoded_private_limits_atomic() {
    let extra = [zlib_encoding(2), element(0x63a2, &ABC)].concat();
    for (raw, metadata_left, message) in [
        (
            [track(1, 1, &[]), track(1, 1, &[])].concat(),
            4096,
            "duplicate track number",
        ),
        (track(1, 1, &extra), 0, "decoded private metadata"),
    ] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let left = if metadata_left == 0 {
            raw.len() + 2
        } else {
            metadata_left
        };
        let mut job = TracksJob::new(source, None, Limits::default(), left, &pool, 1, 1).unwrap();
        let mut error = None;
        for _ in 0..10_000 {
            match job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => (),
                Err(value) => {
                    error = Some(value);
                    break;
                }
                other => panic!("invalid Tracks published: {other:?}"),
            }
        }
        // The private decoder may reject its output cap before the final
        // retained-capacity check; both are explicit permanent policy failures.
        if message == "decoded private metadata" {
            assert!(matches!(error, Some(Error::Policy(_))));
        } else {
            assert!(matches!(error,Some(Error::Invalid(value)) if value==message));
        }
    }
}

#[test]
fn raw_tracks_job_preserves_mixed_encoding_order_and_all_hdr_source_floats() {
    let extra = [
        element(
            0x6d80,
            &[content_encoding(1, 0, &[]), content_encoding(0, 3, b"P")].concat(),
        ),
        element(0x63a2, &ABC),
        element(
            0xe0,
            &[
                uint(0xb0, 3840),
                uint(0xba, 2160),
                element(
                    0x55b0,
                    &[
                        element(0x55ba, &[]),
                        uint(0x55bc, 1000),
                        element(
                            0x55d0,
                            &[
                                element(0x55d1, &0.708_f64.to_be_bytes()),
                                element(0x55d9, &1000_f64.to_be_bytes()),
                                element(0x55da, &0.005_f64.to_be_bytes()),
                            ]
                            .concat(),
                        ),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    ]
    .concat();
    let raw = track(1, 1, &extra);
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job = TracksJob::new(source, None, Limits::default(), 4096, &pool, 1, 1).unwrap();
    let mut config = None;
    for _ in 0..10_000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                config = Some(value);
                break;
            }
            other => panic!("mixed typed configuration: {other:?}"),
        }
    }
    let config = config.unwrap();
    let track = &config.tracks()[0];
    assert_eq!(track.codec_private.as_ref(), b"Pabc");
    assert_eq!((track.width, track.height), (3840, 2160));
    let colour = track.colour.unwrap();
    assert_eq!(colour.transfer_characteristics, 2);
    assert_eq!(colour.max_cll, Some(1000));
    let mastering = colour.mastering.unwrap();
    assert_eq!(mastering.chromaticity[0], Some(0.708));
    assert_eq!(mastering.chromaticity[1], None);
    assert_eq!(mastering.luminance_max, Some(1000.0));
    assert_eq!(mastering.luminance_min, Some(0.005));
    let source =
        SharedBytes::copy_from_slice(&pool, &[&[0x81, 0, 0, 0x80][..], &ABC].concat()).unwrap();
    let mut packets =
        PacketBatchJob::new(source, true, 0, 1_000_000, config, Limits::default(), &pool).unwrap();
    let mut payload = None;
    for _ in 0..10_000 {
        match packets
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                payload = Some(value.packet.data);
                break;
            }
            other => panic!("mixed restored packet: {other:?}"),
        }
    }
    assert_eq!(payload.unwrap().as_ref(), b"Pabc");
}
