//! Authored format-job regressions behind the additive typed reader.
mod cue_reference_closure;
mod packet_side_data;
use super::*;
use crate::{incremental::ParseStep, storage::SharedBytes};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, WorkBudget,
};

fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];

#[test]
fn lace_job_resumes_descriptors_and_exposes_checked_source_ranges() {
    for (mode, raw, expected) in [
        (1, vec![1, 1, 4, 5, 6], vec![vec![4], vec![5, 6]]),
        (2, vec![1, 4, 5, 6, 7], vec![vec![4, 5], vec![6, 7]]),
        (
            3,
            vec![2, 0x81, 0xc0, 4, 5, 6, 7, 8, 9],
            vec![vec![4], vec![5, 6], vec![7, 8, 9]],
        ),
    ] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = lace::LaceJob::new(source.clone(), 0..raw.len(), mode).unwrap();
        let mut frames = None;
        for _ in 0..1000 {
            let mut work = WorkBudget::new(1);
            match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
                ParseStep::Yield => (),
                ParseStep::Event(value) => {
                    frames = Some(value);
                    break;
                }
                _ => panic!("bounded known lace"),
            }
            assert!(work.spent() <= 1);
        }
        let frames = frames.unwrap();
        assert_eq!(frames.len(), expected.len());
        for (range, bytes) in frames.ranges().zip(expected) {
            assert_eq!(&source[range], bytes.as_slice());
        }
    }
}

#[test]
fn source_range_job_uses_the_existing_core_cursor_and_preserves_generation() {
    let raw = element(0x1c53bb6b, &element(0xbb, &[]));
    let pool = Budget::new(64 * 1024).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(7),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        1,
    )
    .unwrap();
    let generation = input.generation();
    let mut job = range::RangeLeafJob::begin(&mut input, 123, 0x1c53bb6b, None, 1024).unwrap();
    let old = input.token();
    let mut result = None;
    for _ in 0..1000 {
        match job
            .step(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = (request.offset - 123) as usize;
                assert!(
                    at < raw.len(),
                    "a finite complete leaf must not require source EOF"
                );
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                input
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("complete one-byte ranged leaf"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.data.as_ref(), element(0xbb, &[]).as_slice());
    assert_eq!(input.generation(), generation);
    let new = input.request_range(0).unwrap();
    assert_ne!(old, new.token);
    assert_eq!(input.generation(), generation);
    drop(input);
    assert_eq!(result.data.as_ref(), element(0xbb, &[]).as_slice());
}

#[test]
fn stored_transform_has_exact_capacity_and_retains_owned_alias() {
    use crate::zlib::typed_restore::{RestoreJob, RestorePlan};
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &ABC).unwrap();
    let mut job = RestoreJob::new(
        source,
        RestorePlan::zlib(),
        crate::Limits::default(),
        33,
        &pool,
    )
    .unwrap();
    let mut result = None;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            _ => panic!("valid bounded stored stream"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    assert_eq!(result.as_ref(), b"abc");
    assert_eq!(result.retained_capacity(), 3);
    // A separate retained alias must remain charged after the job is dropped.
    let alias = result.clone();
    let before = pool.used();
    drop(job);
    assert!(pool.used() <= before);
    assert_eq!(alias.as_ref(), b"abc");
}

#[test]
fn info_projection_reports_only_source_values_and_preserves_unknown_fields_as_raw_bytes() {
    let raw = [
        element(0x2ad7b1, &[0x0f, 0x42, 0x40]),
        element(0xec, b"opaque"),
    ]
    .concat();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job = info::InfoJob::new(source.clone());
    let mut info = None;
    for _ in 0..1000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                info = Some(value);
                break;
            }
            _ => panic!("complete Info"),
        }
    }
    let info = info.unwrap();
    assert_eq!(info.timestamp_scale_ns, 1_000_000);
    assert!(info.source.shares_storage_with(&source));
    assert_eq!(info.source.as_ref(), raw.as_slice());
}

fn uint(id: u64, value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}
fn track_entry(number: u64, kind: u64, extra: &[u8]) -> Vec<u8> {
    [
        uint(0xd7, number),
        uint(0x73c5, number),
        uint(0x83, kind),
        element(0x86, b"V_AUTHORED/OPAQUE"),
        extra.to_vec(),
    ]
    .concat()
}
fn encoding(scope: u64) -> Vec<u8> {
    element(
        0x6d80,
        &element(
            0x6240,
            &[uint(0x5032, scope), element(0x5034, &uint(0x4254, 0))].concat(),
        ),
    )
}
// Only this fixture seed uses the frozen blocking parser. Packet-job tests must
// not rely on the new raw Tracks builder to provide their independent oracle.
fn seed_configuration(pool: &Budget, extra: &[u8]) -> configuration::Configuration {
    seed_configuration_tracks(pool, &element(0xae, &track_entry(1, 1, extra)))
}
fn seed_configuration_tracks(pool: &Budget, entries: &[u8]) -> configuration::Configuration {
    use crate::matroska::Demuxer;
    let raw = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            0x18538067,
            &[element(0x1654ae6b, entries), element(0x1f43b675, &[])].concat(),
        ),
    ]
    .concat();
    let reader =
        Demuxer::open_in(std::io::Cursor::new(raw), crate::Limits::default(), pool).unwrap();
    let mut tracks = BudgetVec::new(pool);
    for track in reader.tracks() {
        tracks.try_push(track.clone()).unwrap();
    }
    let mut seal = configuration::ConfigurationJob::from_validated_vec(tracks, pool, 7, 3);
    match seal
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::Event(value) => value,
        _ => panic!("seal"),
    }
}
fn fixed_stored_block(bad_last_crc: bool) -> Vec<u8> {
    let mut raw = vec![0x81, 0, 0, 0x84, 1];
    raw.extend(ABC);
    raw.extend(ABC);
    if bad_last_crc {
        *raw.last_mut().unwrap() ^= 1;
    }
    raw
}

#[test]
fn bounded_text_keeps_terminator_utf8_ascii_error_precedence() {
    use crate::storage::text_job::TextJob;
    for (raw, ascii, message) in [
        (vec![0xff, 0, 1], true, "metadata string terminator"),
        (vec![0xc2, 0, 1], false, "metadata string terminator"),
        (vec![0xff], true, "UTF-8 metadata"),
        (vec![0xc2], false, "UTF-8 metadata"),
        (vec![1], true, "ASCII metadata"),
    ] {
        let pool = Budget::new(4096).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = TextJob::new(source, 0..raw.len(), ascii).unwrap();
        let mut error = None;
        for _ in 0..100 {
            let mut work = WorkBudget::new(1);
            match job.step(&mut work, &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => (),
                Err(value) => {
                    error = Some(value);
                    break;
                }
                _ => panic!("invalid source text published"),
            }
            assert!(work.spent() <= 1);
        }
        assert!(matches!(error,Some(crate::Error::Invalid(value)) if value==message));
    }
}

#[test]
fn bounded_text_keeps_the_exact_unicode_source_span_after_owner_drop() {
    use crate::storage::text_job::TextJob;
    let pool = Budget::new(4096).unwrap();
    let baseline = pool.used();
    let raw = "Aé😀\0\0".as_bytes();
    let source = SharedBytes::copy_from_slice(&pool, raw).unwrap();
    let pointer = source.as_ptr();
    let mut job = TextJob::new(source, 0..raw.len(), false).unwrap();
    let mut result = None;
    for _ in 0..100 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("bounded text"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.range, 0.."Aé😀".len());
    let alias = result.text.clone();
    drop((job, result));
    assert_eq!(alias.as_str(), "Aé😀");
    assert_eq!(alias.as_ptr(), pointer);
    assert!(pool.used() > baseline);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn packet_batch_rejects_a_bad_last_crc_without_publishing_any_lace() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &fixed_stored_block(true)).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let mut failed = false;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)) {
            Ok(ParseStep::Yield) => (),
            Err(crate::Error::Invalid("Adler-32 mismatch")) => {
                failed = true;
                break;
            }
            other => panic!("no lace may escape rejected batch: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert!(failed);
    assert!(matches!(
        job.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(crate::Error::Invalid(_))
    ));
}

#[test]
fn packet_batch_preserves_exact_laced_capacity_and_old_configuration_lifetime() {
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let config = seed_configuration(&pool, &encoding(1));
    let next = config.rebind(8, 4);
    let limits = crate::Limits {
        block_bytes: 33,
        ..crate::Limits::default()
    };
    let source = SharedBytes::copy_from_slice(&pool, &fixed_stored_block(false)).unwrap();
    let mut job =
        packet::PacketBatchJob::new(source, true, 2, 1_000_000, config, limits, &pool).unwrap();
    let mut packets = Vec::new();
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => packets.push(value),
            ParseStep::Finished => break,
            other => panic!("valid batch: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert_eq!(packets.len(), 2);
    for packet in &packets {
        assert_eq!(packet.packet.data.as_ref(), b"abc");
        assert_eq!(packet.packet.data.storage.retained_capacity(), 3);
        assert_eq!(packet.configuration.generation, 7);
        assert_eq!(packet.configuration.revision, 3);
    }
    assert_eq!(packets[0].packet.pts_ns, Some(2_000_000));
    assert_eq!(packets[1].packet.pts_ns, None);
    assert_eq!(next.generation, 8);
    drop((next, job));
    assert!(pool.used() > baseline);
    assert_eq!(packets[1].packet.data.as_ref(), b"abc");
    drop(packets);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn nonlaced_stored_packet_keeps_borrowed_block_owner_and_its_capacity() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &encoding(1));
    let source =
        SharedBytes::copy_from_slice(&pool, &[&[0x81, 0, 0, 0x80][..], &ABC].concat()).unwrap();
    let original = source.clone();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let mut packet = None;
    for _ in 0..10_000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                packet = Some(value);
                break;
            }
            other => panic!("stored frame: {other:?}"),
        }
    }
    let packet = packet.unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"abc");
    assert!(packet.packet.data.storage.shares_storage_with(&original));
    assert_eq!(
        packet.packet.data.storage.retained_capacity(),
        original.retained_capacity()
    );
}

#[test]
fn packet_batch_keeps_pending_output_across_pressure_and_checks_cancel_before_publish() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &fixed_stored_block(false)).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut saw_pressure = false;
    for _ in 0..1000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => {
                saw_pressure = true;
                break;
            }
            other => panic!("reserved pool: {other:?}"),
        }
    }
    assert!(saw_pressure);
    drop(blocker);
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert!(matches!(
        job.step(&mut WorkBudget::new(1000), &AtomicBool::new(true)),
        Err(crate::Error::Cancelled)
    ));
    assert!(matches!(
        job.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(crate::Error::Invalid(_))
    ));
}

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
    let mut job = tracks::TracksJob::new(
        source.clone(),
        None,
        crate::Limits::default(),
        1024,
        &pool,
        1,
        1,
    )
    .unwrap();
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
    let extra = [encoding(2), element(0x63a2, &ABC)].concat();
    for (raw, metadata_left, message) in [
        (
            [
                element(0xae, &track_entry(1, 1, &[])),
                element(0xae, &track_entry(1, 1, &[])),
            ]
            .concat(),
            4096,
            "duplicate track number",
        ),
        (
            element(0xae, &track_entry(1, 1, &extra)),
            0,
            "decoded private metadata",
        ),
    ] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let left = if metadata_left == 0 {
            raw.len() + 2
        } else {
            metadata_left
        };
        let mut job =
            tracks::TracksJob::new(source, None, crate::Limits::default(), left, &pool, 1, 1)
                .unwrap();
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
        // The frozen private decoder may reject its output cap before the final
        // retained-capacity check; both are explicit permanent policy failures.
        if message == "decoded private metadata" {
            assert!(matches!(error, Some(crate::Error::Limit(_))));
        } else {
            assert!(matches!(error,Some(crate::Error::Invalid(value)) if value==message));
        }
    }
}

#[test]
fn raw_tracks_job_preserves_mixed_encoding_order_and_all_hdr_source_floats() {
    let enc = |order, algorithm, settings: &[u8]| {
        element(
            0x6240,
            &[
                uint(0x5031, order),
                uint(0x5032, 3),
                element(
                    0x5034,
                    &[uint(0x4254, algorithm), element(0x4255, settings)].concat(),
                ),
            ]
            .concat(),
        )
    };
    let extra = [
        element(0x6d80, &[enc(1, 0, &[]), enc(0, 3, b"P")].concat()),
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
    let raw = element(0xae, &track_entry(1, 1, &extra));
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job =
        tracks::TracksJob::new(source, None, crate::Limits::default(), 4096, &pool, 1, 1).unwrap();
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
    let mut packets = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
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

fn cluster_anchor(
    pool: &Budget,
    tail: &[u8],
) -> (
    crate::incremental::ElementReader,
    crate::incremental::ClusterCheckpoint,
    Vec<u8>,
) {
    use crate::incremental::{ElementEvent, ElementReader};
    let raw = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, tail),
    ]
    .concat();
    let mut reader = ElementReader::new(
        pool,
        SourceId(11),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        crate::Limits::default(),
        1,
    )
    .unwrap();
    let mut anchor = None;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(pool, &raw[at..at + 1]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            ParseStep::Yield => (),
            ParseStep::Event(ElementEvent::Start(header)) if header.id == 0x1f43b675 => {
                anchor = reader.cluster_checkpoint();
                break;
            }
            ParseStep::Event(_) => (),
            other => panic!("anchor source: {other:?}"),
        }
    }
    (reader, anchor.unwrap(), raw)
}

#[test]
fn async_cue_validation_requests_ranges_checks_all_laces_and_repositions_before_receipt() {
    use crate::{cues::Cue, incremental::ElementEvent};
    let pool = Budget::new(128 * 1024).unwrap();
    let configuration = seed_configuration(&pool, &encoding(1));
    let block = element(0xa3, &fixed_stored_block(false));
    let timestamp = uint(0xe7, 2);
    let (mut reader, anchor, raw) = cluster_anchor(
        &pool,
        &element(0x1f43b675, &[timestamp.clone(), block].concat()),
    );
    let generation = reader.generation();
    let cue = Cue {
        ticks: 2,
        track: 1,
        cluster: anchor.offset(),
        relative: Some(timestamp.len() as u64),
        block: Some(1),
        codec_state: 0,
    };
    let configuration = configuration.rebind(generation, 3);
    let mut job = seek_validate::CueValidationJob::new(
        crate::matroska::typed::seek_validate::CueTarget {
            cue,
            track: 0,
            scale: 1_000_000,
            anchor,
            whole_cluster: false,
        },
        configuration,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let mut result = None;
    let mut request_tokens = Vec::new();
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        match job
            .step(&mut reader, &mut work, &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                request_tokens.push(request.token.clone());
                let at = request.offset as usize;
                assert!(at < raw.len());
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("cue validator: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    assert_eq!(result.actual_ns, 2_000_000);
    assert!(result.scanned_bytes > 0);
    assert_eq!(reader.generation(), generation);
    assert!(
        request_tokens
            .iter()
            .all(|token| token.generation() == generation)
    );
    assert_eq!(reader.position(), result.location.block_offset());
    // A replay range has a fresh core request token before a receipt can escape.
    assert_ne!(reader.request().token, *request_tokens.last().unwrap());
    let mut replay = None;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Yield => (),
            ParseStep::Event(ElementEvent::Leaf { header, data }) => {
                replay = Some((header, data));
                break;
            }
            other => panic!("replayed block: {other:?}"),
        }
    }
    let (header, data) = replay.unwrap();
    assert_eq!(header.id, 0xa3);
    assert_eq!(data.as_ref(), fixed_stored_block(false).as_slice());
}

#[test]
fn async_cue_validation_cannot_turn_bad_last_crc_or_wrong_source_position_into_landing() {
    use crate::cues::Cue;
    for bad_crc in [false, true] {
        let pool = Budget::new(128 * 1024).unwrap();
        let configuration = seed_configuration(&pool, &encoding(1));
        let timestamp = uint(0xe7, 2);
        let (mut reader, anchor, raw) = cluster_anchor(
            &pool,
            &element(
                0x1f43b675,
                &[
                    timestamp.clone(),
                    element(0xa3, &fixed_stored_block(bad_crc)),
                ]
                .concat(),
            ),
        );
        let cue = Cue {
            ticks: 2,
            track: 1,
            cluster: anchor.offset(),
            relative: Some(timestamp.len() as u64 + 1),
            block: Some(1),
            codec_state: 0,
        };
        let mut job = seek_validate::CueValidationJob::new(
            crate::matroska::typed::seek_validate::CueTarget {
                cue,
                track: 0,
                scale: 1_000_000,
                anchor,
                whole_cluster: true,
            },
            configuration.rebind(reader.generation(), 3),
            crate::Limits::default(),
            &pool,
        )
        .unwrap();
        let mut failed = false;
        for _ in 0..100_000 {
            match job.step(
                &mut reader,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            ) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::NeedInput(request)) => {
                    let at = request.offset as usize;
                    if at == raw.len() {
                        reader.finish(request.source, request.token).unwrap();
                    } else {
                        let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                        reader
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    bytes,
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Err(crate::Error::Invalid(message)) => {
                    assert_eq!(
                        message,
                        if bad_crc {
                            "Adler-32 mismatch"
                        } else {
                            "cue position disagrees with referenced block"
                        }
                    );
                    failed = true;
                    break;
                }
                other => panic!("unverified cue must not produce a landing: {other:?}"),
            }
        }
        assert!(failed);
    }
}

#[test]
fn seek_plan_uses_all_track_preroll_negative_floor_and_keeps_extreme_unselected_ticks() {
    use crate::cues::Cue;
    let pool = Budget::new(64 * 1024).unwrap();
    let video = track_entry(1, 1, &uint(0x56aa, 2));
    let audio = track_entry(2, 2, &uint(0x56bb, 10));
    let raw = [element(0xae, &video), element(0xae, &audio)].concat();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut tracks =
        tracks::TracksJob::new(source, None, crate::Limits::default(), 4096, &pool, 1, 1).unwrap();
    let mut config = None;
    for _ in 0..10_000 {
        match tracks
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                config = Some(value);
                break;
            }
            other => panic!("config {other:?}"),
        }
    }
    let cue = |ticks, track, cluster| Cue {
        ticks,
        track,
        cluster,
        relative: None,
        block: None,
        codec_state: 0,
    };
    let cues = [cue(0, 1, 100), cue(u64::MAX, 1, 200), cue(u64::MAX, 2, 300)];
    let mut plan = seek::SeekPlanJob::new(config.unwrap(), 0, 0, true, 3, 100).unwrap();
    let mut result = None;
    for _ in 0..1000 {
        match plan
            .step(&cues, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("plan {other:?}"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.preroll_ns, 10);
    assert_eq!(result.discard_until_ns, 0);
    assert!(matches!(
        result.landing,
        seek::LandingPlan::Cue {
            cue: crate::cues::Cue { ticks: 0, .. },
            whole_cluster: true,
            scan_after_validation: None
        }
    ));
}

#[test]
fn cues_job_waits_for_late_time_keeps_every_source_position_and_does_not_sort_silently() {
    let point = |ticks| {
        element(
            0xbb,
            &[
                element(
                    0xb7,
                    &[uint(0xf7, 1), uint(0xf1, 10), uint(0xf0, 0)].concat(),
                ),
                element(
                    0xb7,
                    &[uint(0xf7, 2), uint(0xf1, 20), uint(0x5378, 1)].concat(),
                ),
                uint(0xb3, ticks),
            ]
            .concat(),
        )
    };
    let raw = [point(u64::MAX), point(0)].concat();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let alias = source.clone();
    let mut job = cues::CuesJob::new(source, 100, Some(1000), 4, &pool).unwrap();
    let mut result = None;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("Cues transaction: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    let entries = result.entries.as_slice();
    assert_eq!(entries.len(), 4);
    assert_eq!(
        (entries[0].ticks, entries[1].ticks, entries[2].ticks),
        (u64::MAX, u64::MAX, 0)
    );
    assert_eq!((entries[0].cluster, entries[1].cluster), (110, 120));
    assert_eq!(entries[0].relative, Some(0));
    assert_eq!(entries[1].block, Some(1));
    assert!(result.source.shares_storage_with(&alias));
    drop((job, alias));
    assert_eq!(result.source.as_ref(), raw.as_slice());
}

#[test]
fn blocking_feeder_retries_freeze_without_reading_again_and_observes_known_eof() {
    use std::io::Read;
    struct OnceBlocked {
        pool: Budget,
        blocker: Option<triergon_core::Reservation>,
        reads: usize,
        at: usize,
    }
    impl Read for OnceBlocked {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.at == 3 {
                return Ok(0);
            }
            let count = out.len().min(3 - self.at);
            out[..count].copy_from_slice(&b"abc"[self.at..self.at + count]);
            self.at += count;
            if self.blocker.is_none() {
                self.blocker = Some(self.pool.reserve(self.pool.available()).unwrap());
            }
            Ok(count)
        }
    }
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(31),
        SourceCapabilities {
            length: Some(3),
            random_access: false,
        },
        3,
    )
    .unwrap();
    let mut feeder = blocking::InputFeeder::new(
        OnceBlocked {
            pool: pool.clone(),
            blocker: None,
            reads: 0,
            at: 0,
        },
        3,
    )
    .unwrap();
    assert!(matches!(
        feeder
            .feed_sequential(&mut input, &AtomicBool::new(false))
            .unwrap(),
        blocking::FeedStep::Backpressure { .. }
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    drop(feeder.reader_mut().blocker.take());
    assert!(matches!(
        feeder
            .feed_sequential(&mut input, &AtomicBool::new(false))
            .unwrap(),
        blocking::FeedStep::Provided
    ));
    assert_eq!(feeder.reader_mut().reads, 1);
    let mut bytes = [0; 3];
    assert!(matches!(
        input
            .read_into(&mut bytes, &mut WorkBudget::new(3), &AtomicBool::new(false))
            .unwrap(),
        triergon_core::Step::Ready(3)
    ));
    assert_eq!(&bytes, b"abc");
    // Reaching declared length is starvation until an actual Read confirms EOF.
    assert!(matches!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        triergon_core::Step::NeedInput(_)
    ));
    assert!(matches!(
        feeder
            .feed_sequential(&mut input, &AtomicBool::new(false))
            .unwrap(),
        blocking::FeedStep::End
    ));
    assert_eq!(feeder.reader_mut().reads, 2);
    assert!(matches!(
        input
            .read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        triergon_core::Step::End(_)
    ));
}

#[test]
fn blocking_feeder_keeps_transport_failure_distinct_from_explicit_end() {
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("authored transport failure"))
        }
    }
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(32),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        4,
    )
    .unwrap();
    let mut feeder = blocking::InputFeeder::new(Broken, 4).unwrap();
    assert!(matches!(
        feeder.feed_sequential(&mut input, &AtomicBool::new(false)),
        Err(crate::Error::Io(_))
    ));
    assert!(matches!(
        input.read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(triergon_core::Error::Transport)
    ));
    let request = input.request();
    assert!(matches!(
        input.finish(request.source, request.token),
        Err(triergon_core::Error::Transport)
    ));
}

#[test]
fn packet_batch_retries_pending_allocation_without_losing_either_lace() {
    let pool = Budget::new(64 * 1024).unwrap();
    let config = seed_configuration(&pool, &encoding(1));
    let source = SharedBytes::copy_from_slice(&pool, &fixed_stored_block(false)).unwrap();
    let mut job = packet::PacketBatchJob::new(
        source,
        true,
        0,
        1_000_000,
        config,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..1000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            other => panic!("retained caller pressure: {other:?}"),
        }
    }
    assert!(blocked);
    drop(blocker);
    let mut packets = Vec::new();
    let mut finished = false;
    for _ in 0..10_000 {
        match job
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => packets.push(value),
            ParseStep::Finished => {
                finished = true;
                break;
            }
            other => panic!("resumed batch: {other:?}"),
        }
    }
    assert!(finished);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].packet.data.as_ref(), b"abc");
    assert_eq!(packets[1].packet.data.as_ref(), b"abc");
}

#[test]
fn finite_child_cursor_rejects_even_an_empty_span_outside_its_owner() {
    let mut cursor = syntax::ChildCursor::new(4..4);
    assert!(matches!(
        cursor.step(b"abc", &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(crate::Error::Truncated)
    ));
}

#[test]
fn block_group_waits_for_trailing_timing_and_accepts_empty_codec_state_before_publication() {
    let block = element(0xa1, &[0x81, 0, 5, 0x04, 1, b'A', b'B']);
    for has_state in [false, true] {
        let pool = Budget::new(64 * 1024).unwrap();
        let configuration = seed_configuration(&pool, &[]);
        let initial_revision = configuration.revision();
        let initial_generation = configuration.generation();
        let mut raw = [
            block.clone(),
            uint(0x9b, 4),
            element(0xfb, &[0xff]),
            element(0x75a2, &[0xff]),
        ]
        .concat();
        if has_state {
            raw.extend(element(0xa4, &[]));
        }
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = packet::PacketBatchJob::new(
            source,
            false,
            10,
            1_000_000,
            configuration.clone(),
            crate::Limits::default(),
            &pool,
        )
        .unwrap();
        let mut packets = Vec::new();
        let mut changed = None;
        let mut finished = false;
        for _ in 0..10_000 {
            if let Some(value) = job.take_configuration_change() {
                assert!(
                    packets.is_empty(),
                    "state configuration must precede every lace"
                );
                assert!(
                    changed.is_none(),
                    "one written state changes the batch revision once"
                );
                changed = Some(value);
            }
            match job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(packet)) => packets.push(packet),
                Ok(ParseStep::Finished) => {
                    finished = true;
                    break;
                }
                other => panic!("trailing group fields: {other:?}"),
            }
        }
        assert!(finished);
        // Every original timing/duration/key/padding assertion now applies to
        // BOTH the ordinary group and the accepted empty-state transaction.
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].packet.pts_ns, Some(15_000_000));
        assert_eq!(packets[1].packet.pts_ns, Some(17_000_000));
        assert_eq!(packets[0].packet.duration_ns, Some(2_000_000));
        assert!(
            packets
                .iter()
                .all(|p| !p.packet.keyframe && p.packet.discard_padding_ns == -1)
        );
        assert_eq!(changed.is_some(), has_state);
        assert!(
            configuration.codec_state(0).is_none(),
            "the initial snapshot stays immutable"
        );
        for packet in &packets {
            assert_eq!(packet.configuration.generation(), initial_generation);
            assert_eq!(
                packet.configuration.revision(),
                initial_revision + u64::from(has_state)
            );
            let group = packet.group.as_ref().unwrap();
            assert_eq!(group.raw_body(), raw.as_slice());
            assert_eq!(group.codec_state().is_some(), has_state);
            assert_eq!(packet.configuration.codec_state(0).is_some(), has_state);
            let initialization = packet.configuration.initialization(0).unwrap();
            if has_state {
                let state = group.codec_state().unwrap();
                assert!(state.data().is_empty());
                assert_eq!(state.raw(), element(0xa4, &[]).as_slice());
                assert_eq!(
                    state.position().unwrap(),
                    None,
                    "a low-level job has no invented absolute origin"
                );
                assert!(
                    packet
                        .configuration
                        .codec_state(0)
                        .unwrap()
                        .data()
                        .is_empty()
                );
                assert!(initialization.data.is_empty());
                assert_eq!(
                    initialization.origin,
                    crate::typed::InitializationOrigin::CodecState(None)
                );
                assert_eq!(
                    changed.as_ref().unwrap().revision(),
                    packet.configuration.revision()
                );
            } else {
                assert_eq!(
                    initialization.origin,
                    crate::typed::InitializationOrigin::InitialTrackEntry
                );
            }
        }
    }
}

#[test]
fn range_job_rejects_stale_generation_and_a_transport_failure_even_at_its_pending_freeze() {
    use triergon_core::SourceFailure;
    let raw = element(0x1c53bb6b, &element(0xbb, &[]));
    let pool = Budget::new(4096).unwrap();
    let mut input = InputCursor::new(
        &pool,
        SourceId(41),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        1,
    )
    .unwrap();
    let mut stale = range::RangeLeafJob::begin(&mut input, 0, 0x1c53bb6b, None, 1024).unwrap();
    input.reset(0).unwrap();
    assert!(matches!(
        stale.step(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(crate::Error::Core(
            triergon_core::Error::StaleGeneration { .. }
        ))
    ));
    drop(stale);
    let mut job = range::RangeLeafJob::begin(&mut input, 0, 0x1c53bb6b, None, 1024).unwrap();
    let mut blockers = Vec::new();
    let mut last_supplied = false;
    let mut blocked = false;
    for _ in 0..1000 {
        match job
            .step(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => {
                if last_supplied && pool.available() != 0 {
                    blockers.push(pool.reserve(pool.available()).unwrap());
                }
            }
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                assert!(at < raw.len());
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                input
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
                if at + 1 == raw.len() {
                    last_supplied = true;
                    blockers.push(pool.reserve(pool.available()).unwrap());
                }
            }
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            other => panic!("pending freeze: {other:?}"),
        }
    }
    assert!(blocked);
    let request = input.request();
    input
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    drop(blockers);
    assert!(matches!(
        job.step(
            &mut input,
            &mut WorkBudget::new(1000),
            &AtomicBool::new(false)
        ),
        Err(crate::Error::Core(triergon_core::Error::Transport))
    ));
}

#[test]
fn playback_preroll_plan_validates_video_but_replays_the_earlier_interleaved_audio() {
    use crate::{cues::Cue, incremental::ElementEvent};
    let pool = Budget::new(128 * 1024).unwrap();
    let entries = [
        element(0xae, &track_entry(1, 1, &[])),
        element(0xae, &track_entry(2, 2, &uint(0x56bb, 2_000_000))),
    ]
    .concat();
    let configuration = seed_configuration_tracks(&pool, &entries);
    let timestamp = uint(0xe7, 2);
    let audio = element(0xa3, &[0x82, 0xff, 0xff, 0x80, b'A']);
    let video = element(0xa3, &[0x81, 0, 0, 0x80, b'V']);
    let (mut reader, anchor, raw) = cluster_anchor(
        &pool,
        &element(
            0x1f43b675,
            &[timestamp.clone(), audio.clone(), video].concat(),
        ),
    );
    let configuration = configuration.rebind(reader.generation(), 3);
    let cues = [Cue {
        ticks: 2,
        track: 1,
        cluster: anchor.offset(),
        relative: Some((timestamp.len() + audio.len()) as u64),
        block: Some(2),
        codec_state: 0,
    }];
    let mut plan = seek::SeekPlanJob::new(
        configuration.clone(),
        0,
        2_000_000,
        true,
        1_000_000,
        anchor.offset(),
    )
    .unwrap();
    let mut result = None;
    for _ in 0..1000 {
        match plan
            .step(&cues, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("preroll plan: {other:?}"),
        }
    }
    let result = result.unwrap();
    assert_eq!(result.preroll_ns, 2_000_000);
    assert_eq!(result.discard_until_ns, 2_000_000);
    let seek::LandingPlan::Cue {
        cue,
        whole_cluster,
        scan_after_validation: None,
    } = result.landing
    else {
        panic!("single-cue preroll")
    };
    assert!(whole_cluster);
    let mut job = seek_validate::CueValidationJob::new(
        crate::matroska::typed::seek_validate::CueTarget {
            cue,
            track: 0,
            scale: 1_000_000,
            anchor,
            whole_cluster,
        },
        configuration,
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let mut landing = None;
    for _ in 0..100_000 {
        match job
            .step(
                &mut reader,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                assert!(at < raw.len());
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(value) => {
                landing = Some(value);
                break;
            }
            other => panic!("preroll validation: {other:?}"),
        }
    }
    let landing = landing.unwrap();
    assert_eq!(landing.actual_ns, 2_000_000);
    assert_eq!(landing.replay_cluster_time, None);
    assert_eq!(reader.position(), cue.cluster);
    let mut first_block = None;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::NeedInput(request) => {
                let at = request.offset as usize;
                let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            ParseStep::Event(ElementEvent::Leaf { header, data }) if header.id == 0xa3 => {
                first_block = Some(data);
                break;
            }
            ParseStep::Event(_) => (),
            other => panic!("whole-Cluster replay: {other:?}"),
        }
    }
    assert_eq!(
        first_block.unwrap().as_ref(),
        &[0x82, 0xff, 0xff, 0x80, b'A']
    );
}

#[test]
fn typed_document_preserves_the_validated_preorder_source_catalogue_under_work_one() {
    use crate::metadata::{CHAPTERS, Document, typed_document::DocumentJob};
    let chapter = |uid, start, end| {
        element(
            0xb6,
            &[
                uint(0x73c4, uid),
                uint(0x91, start),
                uint(0x92, end),
                element(0x80, &element(0x85, "épisode".as_bytes())),
                element(0x4abc, b"opaque"),
            ]
            .concat(),
        )
    };
    let raw = element(
        0x45b9,
        &[uint(0x45bc, 99), chapter(1, 0, 20), chapter(2, 21, 40)].concat(),
    );
    let expected = Document::parse(CHAPTERS, raw.clone(), 4096, 32, 64 * 1024, 4096).unwrap();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let mut job = DocumentJob::new(
        CHAPTERS,
        source,
        crate::metadata::typed_document::DocumentLimits {
            node_limit: 4096,
            depth_limit: 32,
            storage_limit: 64 * 1024,
            attachment_limit: 4096,
        },
        Some(123),
        &pool,
    )
    .unwrap();
    let mut result = None;
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("typed document: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    assert_eq!(result.source_offset(), Some(123));
    assert_eq!(result.raw(), raw.as_slice());
    assert_eq!(result.node_count(), expected.node_count());
    let values = |document: &Document| {
        document
            .elements()
            .map(|element| {
                (
                    element.id(),
                    element.raw().to_vec(),
                    element.parent().map(|p| p.id()),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(values(&result), values(&expected));
    let alias = result.clone();
    drop((result, job));
    assert_eq!(alias.kind(), CHAPTERS);
    assert!(pool.used() > baseline);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn typed_document_rejects_uid_reuse_ordered_missing_end_and_nested_start_range_atomically() {
    use crate::metadata::{CHAPTERS, typed_document::DocumentJob};
    let atom = |uid, start, end: &[u8], nested: &[u8]| {
        element(
            0xb6,
            &[
                uint(0x73c4, uid),
                uint(0x91, start),
                end.to_vec(),
                nested.to_vec(),
            ]
            .concat(),
        )
    };
    let invalids = [
        (
            element(
                0x45b9,
                &[atom(1, 0, &[], &[]), atom(1, 1, &[], &[])].concat(),
            ),
            "duplicate metadata UID",
        ),
        (
            element(0x45b9, &[uint(0x45dd, 1), atom(1, 0, &[], &[])].concat()),
            "missing metadata field",
        ),
        (
            element(
                0x45b9,
                &atom(1, 10, &uint(0x92, 20), &atom(2, 9, &uint(0x92, 11), &[])),
            ),
            "nested chapter time range",
        ),
    ];
    for (raw, message) in invalids {
        let pool = Budget::new(128 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = DocumentJob::new(
            CHAPTERS,
            source,
            crate::metadata::typed_document::DocumentLimits {
                node_limit: 4096,
                depth_limit: 32,
                storage_limit: 64 * 1024,
                attachment_limit: 4096,
            },
            None,
            &pool,
        )
        .unwrap();
        let mut error = None;
        for _ in 0..100_000 {
            match job.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => (),
                Err(value) => {
                    error = Some(value);
                    break;
                }
                other => panic!("invalid catalogue published: {other:?}"),
            }
        }
        assert!(matches!(error,Some(crate::Error::Invalid(value)) if value==message));
    }
}

#[test]
fn typed_catalogue_keeps_cumulative_node_limits_and_prior_document_after_rejected_commit() {
    use crate::metadata::{Catalogue, INFO, typed_document::CatalogueJob};
    let pool = Budget::new(128 * 1024).unwrap();
    let mut catalogue = Catalogue::new(&pool);
    let limits = crate::Limits {
        metadata_nodes: 3,
        ..crate::Limits::default()
    };
    let raw = uint(0x2ad7b1, 1_000_000);
    for pass in 0..2 {
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = CatalogueJob::new(INFO, source, 4096, Some(pass), limits, &pool).unwrap();
        let mut accepted = false;
        let mut rejected = false;
        for _ in 0..100_000 {
            match job.step(
                &mut catalogue,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            ) {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(value)) => {
                    assert_eq!(value.structural_bytes, raw.len());
                    accepted = true;
                    break;
                }
                Err(crate::Error::Limit("metadata nodes")) => {
                    rejected = true;
                    break;
                }
                other => panic!("catalogue transaction: {other:?}"),
            }
        }
        assert_eq!(accepted, pass == 0);
        assert_eq!(rejected, pass == 1);
        assert_eq!(catalogue.documents().len(), 1);
        assert_eq!(catalogue.documents()[0].source_offset(), Some(0));
    }
}

mod cue_tie_tests;

mod profiled_resource_tests;

mod packet_layout_tests;

mod seek_optimization;

mod packet_handoff_tests;

mod publication_move_tests;

mod text_vector_tests;

mod chapter_walk_tests;

mod seek_subtitle_recovery_tests;

mod ready_slot_tests;
