//! Original runtime guards. Baseline-compatible: no new optimization API used.
use super::*;
use crate::incremental::{ElementEvent, ElementReader};
use crate::matroska::typed::epoch::{EpochRestoreJob, EpochTarget};
use std::io::Cursor;

fn prefix_anchor(
    pool: &Budget,
    tail: &[u8],
) -> (
    ElementReader,
    crate::incremental::ClusterCheckpoint,
    Vec<u8>,
) {
    let raw = [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, tail),
    ]
    .concat();
    let mut reader = ElementReader::new(
        pool,
        SourceId(2011),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        crate::Limits::default(),
        4096,
    )
    .unwrap();
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(7), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => prefix_supply(&mut reader, pool, &raw, request),
            ParseStep::Event(ElementEvent::Start(header)) if header.id == 0x1f43b675 => {
                let anchor = reader.cluster_checkpoint().unwrap();
                return (reader, anchor, raw);
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("authored Cluster anchor: {other:?}"),
        }
    }
    panic!("Cluster anchor missing")
}
fn prefix_supply(
    reader: &mut ElementReader,
    pool: &Budget,
    raw: &[u8],
    request: triergon_core::InputRequest,
) {
    let at = request.offset as usize;
    if at == raw.len() {
        reader.finish(request.source, request.token).unwrap();
        return;
    }
    let end = (at + request.maximum.min(64)).min(raw.len());
    assert!(end > at);
    let bytes = Bytes::copy_from_slice(pool, &raw[at..end]).unwrap();
    reader
        .provide(&InputWindow::new(request.source, request.token, request.offset, bytes).unwrap())
        .unwrap();
}
fn epoch_target(pool: &Budget) -> (ElementReader, EpochTarget, Vec<u8>, u64) {
    let first_body = [
        uint(0xe7, 0),
        element(
            0xa3,
            &[vec![0x81, 0, 0, 0x80], vec![b'x'; 32 * 1024]].concat(),
        ),
    ]
    .concat();
    let first = element(0x1f43b675, &first_body);
    let second = element(
        0x1f43b675,
        &[uint(0xe7, 10), element(0xa3, &[0x81, 0, 0, 0x80, b'y'])].concat(),
    );
    let (mut reader, anchor, raw) = prefix_anchor(pool, &[first.clone(), second].concat());
    let target = anchor.offset() + first.len() as u64;
    reader
        .begin_internal_cluster_range(&anchor, target)
        .unwrap();
    let replay = loop {
        match reader
            .step(&mut WorkBudget::new(7), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => prefix_supply(&mut reader, pool, &raw, request),
            ParseStep::Event(ElementEvent::Leaf { header, .. }) if header.id == 0xa3 => {
                break reader.block_replay_location(header, Some(10)).unwrap();
            }
            ParseStep::Event(_) | ParseStep::Yield => (),
            other => panic!("authored replay location: {other:?}"),
        }
    };
    let target_context = EpochTarget {
        anchor,
        replay,
        whole_cluster: true,
        reference: None,
        segment_data: 0,
        remaining: 128 * 1024,
    };
    (reader, target_context, raw, target)
}
fn epoch_setup(pool: &Budget) -> (ElementReader, EpochRestoreJob, Vec<u8>, u64) {
    let (reader, target_context, raw, target) = epoch_target(pool);
    let configuration = seed_configuration(pool, &[]).rebind(reader.generation(), 3);
    let job = EpochRestoreJob::new(
        configuration,
        target_context,
        crate::Limits::default(),
        pool,
    )
    .unwrap();
    (reader, job, raw, target)
}

#[test]
fn seek_optimization_epoch_skips_unused_simpleblock_allocation_but_charges_all_source_bytes() {
    let pool = Budget::new(16 * 1024).unwrap();
    let baseline = pool.used();
    let (mut reader, mut job, raw, target) = epoch_setup(&pool);
    let first = job_boundary_start_for_test(&raw);
    let mut result = None;
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(1);
        match job
            .step(&mut reader, &mut work, &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => prefix_supply(&mut reader, &pool, &raw, request),
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            ParseStep::Backpressure { .. } => {
                panic!("irrelevant SimpleBlock body must not require a32KiB collected owner")
            }
            other => panic!("bounded source epoch: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.expect("bounded byte scan must finish");
    // Target Cluster header has ID4+size1. No payload byte is excluded from
    // the source-consumption counter even though its unused owner is omitted.
    assert_eq!(result.scanned_bytes, target - first + 5);
    assert_eq!(reader.position(), target);
    assert!(pool.peak() <= pool.limit());
    drop((reader, job, result));
    assert_eq!(pool.used(), baseline);
}
fn job_boundary_start_for_test(raw: &[u8]) -> u64 {
    // Independent fixture byte search, not a production seek/index helper.
    raw.windows(4)
        .position(|bytes| bytes == [0x1f, 0x43, 0xb6, 0x75])
        .unwrap() as u64
}

#[test]
fn seek_optimization_epoch_zero_work_and_cancel_preserve_cursor_and_sticky_failure() {
    let pool = Budget::new(16 * 1024).unwrap();
    let (mut reader, mut job, _, _) = epoch_setup(&pool);
    let position = reader.position();
    let cancel = AtomicBool::new(false);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        job.step(&mut reader, &mut zero, &cancel).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!((reader.position(), zero.spent()), (position, 0));
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        job.step(&mut reader, &mut WorkBudget::new(1), &cancel),
        Err(crate::Error::Cancelled)
    ));
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(
        job.step(&mut reader, &mut WorkBudget::new(1), &cancel)
            .is_err()
    );
    assert_eq!(reader.position(), position);
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for &byte in bytes {
        value ^= u32::from(byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(value & 1));
        }
    }
    !value
}
struct SeekFixture {
    raw: Vec<u8>,
    prefix: usize,
    target_prefix: usize,
    target_header: usize,
    all_clusters: usize,
}
fn checkpoint_file(indexed: bool, states: bool, bad_crc: bool, false_state: bool) -> SeekFixture {
    let video = element(0xae, &track_entry(1, 1, &element(0x63a2, b"video-initial")));
    let audio = element(0xae, &track_entry(2, 2, &element(0x63a2, b"audio-initial")));
    let tracks = element(0x1654ae6b, &[video, audio].concat());
    let simple = |track: u8, value: u8| element(0xa3, &[0x80 | track, 0, 0, 0x80, value]);
    let group = |track: u8, value: u8, state: &[u8], corrupt: bool| {
        let block = element(0xa1, &[0x80 | track, 0, 0, 0, value]);
        let body = [block.clone(), element(0xa4, state)].concat();
        let crc = crc32(&body) ^ u32::from(corrupt);
        let checksum = element(0xbf, &crc.to_le_bytes());
        let encoded_body = [checksum.clone(), body].concat();
        let encoded = element(0xa0, &encoded_body);
        let state_offset = encoded.len() - encoded_body.len() + checksum.len() + block.len();
        (encoded, state_offset)
    };
    let (video_first, video_state_offset) = if states {
        group(1, b'0', b"video-before", bad_crc)
    } else {
        (simple(1, b'0'), 0)
    };
    let audio_first = if states {
        group(2, b'a', b"audio-before", false).0
    } else {
        simple(2, b'a')
    };
    let first_body = [uint(0xe7, 0), video_first, audio_first].concat();
    let first = element(0x1f43b675, &first_body);
    let timestamp = uint(0xe7, 10);
    let before_rap = [timestamp.clone(), simple(2, b'b')].concat();
    let second_body = [before_rap.clone(), simple(1, b'1')].concat();
    let second = element(0x1f43b675, &second_body);
    let third_body = [
        uint(0xe7, 20),
        if states {
            group(1, b'2', b"video-after", false).0
        } else {
            simple(1, b'2')
        },
        if states {
            group(2, b'c', b"audio-after", false).0
        } else {
            simple(2, b'c')
        },
    ]
    .concat();
    let third = element(0x1f43b675, &third_body);
    let cues = |cluster, state| {
        element(
            0x1c53bb6b,
            &element(
                0xbb,
                &[
                    uint(0xb3, 10),
                    element(
                        0xb7,
                        &[
                            uint(0xf7, 1),
                            uint(0xf1, cluster),
                            uint(0xf0, before_rap.len() as u64),
                            uint(0x5378, 2),
                            uint(0xea, state),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        )
    };
    let index_size = if indexed { cues(0, 0).len() } else { 0 };
    let first_position = tracks.len() + index_size;
    let state_position =
        first_position + first.len() - first_body.len() + uint(0xe7, 0).len() + video_state_offset;
    let index = if indexed {
        cues(
            (first_position + first.len()) as u64,
            if states && !false_state {
                state_position as u64
            } else {
                0
            },
        )
    } else {
        Vec::new()
    };
    assert_eq!(index.len(), index_size);
    let body = [tracks, index, first.clone(), second.clone(), third.clone()].concat();
    SeekFixture {
        raw: [
            element(0x1a45dfa3, &element(0x4282, b"matroska")),
            element(0x18538067, &body),
        ]
        .concat(),
        prefix: first.len(),
        target_prefix: second.len(),
        target_header: second.len() - second_body.len(),
        all_clusters: first.len() + second.len() + third.len(),
    }
}

#[test]
fn seek_optimization_validated_unchanged_target_prefix_is_not_read_twice_for_epoch_proof() {
    let fixture = checkpoint_file(true, false, false, false);
    let expected = fixture.target_prefix + fixture.prefix + fixture.target_header;
    let mut reader = crate::typed::BlockingReader::open_seekable(
        Cursor::new(fixture.raw),
        crate::Limits::default(),
    )
    .unwrap();
    let receipt = reader
        .seek_for_playback(0, 10_000_000, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(
        receipt.scanned_bytes, expected as u64,
        "Cue target validated fully once; prefix epoch stops at proven unchanged Cluster boundary"
    );
    let audio = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let video = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(
        (audio.packet.track, audio.packet.data.as_ref()),
        (1, &b"b"[..])
    );
    assert_eq!(
        (video.packet.track, video.packet.data.as_ref()),
        (0, &b"1"[..])
    );
}

#[test]
fn seek_optimization_missing_index_reuses_all_track_configuration_at_validated_scan_boundary() {
    let fixture = checkpoint_file(false, true, false, false);
    let expected = fixture.all_clusters;
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = crate::typed::BlockingReader::open_seekable_in(
        Cursor::new(fixture.raw),
        crate::Limits::default(),
        &pool,
    )
    .unwrap();
    let receipt = reader
        .seek_for_playback(0, 10_000_000, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(
        receipt.scanned_bytes, expected as u64,
        "the full fallback scan already proved the actual all-track boundary; no second prefix pass"
    );
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"b");
    assert_eq!(
        packet.configuration.initialization(0).unwrap().data,
        b"video-before"
    );
    assert_eq!(
        packet.configuration.initialization(1).unwrap().data,
        b"audio-before"
    );
    drop(reader);
    assert_eq!(
        packet.configuration.initialization(0).unwrap().data,
        b"video-before"
    );
    drop(packet);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn seek_optimization_unchanged_target_proof_still_restores_prior_states_on_every_track() {
    let fixture = checkpoint_file(true, true, false, false);
    let mut reader = crate::typed::BlockingReader::open_seekable(
        Cursor::new(fixture.raw),
        crate::Limits::default(),
    )
    .unwrap();
    reader
        .seek_for_playback(0, 10_000_000, &AtomicBool::new(false))
        .unwrap();
    let packet = reader
        .next_typed_packet(&AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(
        packet.configuration.initialization(0).unwrap().data,
        b"video-before"
    );
    assert_eq!(
        packet.configuration.initialization(1).unwrap().data,
        b"audio-before"
    );
}

#[test]
fn seek_optimization_prefix_bad_crc_and_false_initial_state_never_publish_a_receipt() {
    for (bad_crc, false_state) in [(true, false), (false, true)] {
        let fixture = checkpoint_file(true, true, bad_crc, false_state);
        let mut reader = crate::typed::BlockingReader::open_seekable(
            Cursor::new(fixture.raw),
            crate::Limits::default(),
        )
        .unwrap();
        assert!(
            reader
                .seek_for_playback(0, 10_000_000, &AtomicBool::new(false))
                .is_err()
        );
    }
}

#[test]
fn seek_optimization_foreign_pool_configuration_is_rejected_before_range_io() {
    let pool = Budget::new(16 * 1024).unwrap();
    let (_, target, _, _) = epoch_target(&pool);
    let other = Budget::new(16 * 1024).unwrap();
    assert!(matches!(
        EpochRestoreJob::new(
            seed_configuration(&other, &[]),
            target,
            crate::Limits::default(),
            &pool
        ),
        Err(crate::Error::Core(triergon_core::Error::ForeignPool))
    ));
}

#[test]
fn seek_optimization_skipped_payload_keeps_the_full_remaining_scan_limit() {
    let pool = Budget::new(16 * 1024).unwrap();
    let (mut reader, mut target, raw, _) = epoch_target(&pool);
    target.remaining = 64;
    let configuration = seed_configuration(&pool, &[]).rebind(reader.generation(), 3);
    let mut job =
        EpochRestoreJob::new(configuration, target, crate::Limits::default(), &pool).unwrap();
    let mut error = None;
    for _ in 0..1000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut reader, &mut work, &AtomicBool::new(false)) {
            Ok(ParseStep::NeedInput(request)) => prefix_supply(&mut reader, &pool, &raw, request),
            Ok(ParseStep::Yield) => (),
            Err(value) => {
                error = Some(value);
                break;
            }
            other => panic!("scan limit must precede unused payload collection: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    assert!(matches!(
        error,
        Some(crate::Error::Limit("operation read bytes"))
    ));
}
