//! The immutable-source state-history shortcut must be indistinguishable from
//! the ordinary scan: identical receipts (including scanned bytes), packets and
//! errors for every seek fixture, target and truncation.
use super::*;
use crate::storage::PayloadOwnership;
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_core::{Budget, OwnedInputBytes};

const FIXTURES: [&[u8]; 8] = [
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-end.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-relative.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-front.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-preroll.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-delay-preroll.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-sparse.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-unknown-cluster.mkv"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/seek/cues-missing.mkv"
    )),
];

fn session<R: Read>(
    mut reader: Result<BlockingReader<R>>,
    track: usize,
    target: i64,
    playback: bool,
) -> Vec<String> {
    let cancel = AtomicBool::new(false);
    let mut rows = Vec::new();
    let Ok(demux) = reader.as_mut() else {
        return vec![format!("open {:?}", reader.err())];
    };
    let receipt = if playback {
        demux.seek_for_playback(track, target, &cancel)
    } else {
        demux.seek_previous(track, target, &cancel)
    };
    rows.push(format!("seek {receipt:?}"));
    if receipt.is_ok() {
        loop {
            match demux.next_packet(&cancel) {
                Ok(Some(p)) => rows.push(format!(
                    "packet {} {:?} {:?} {} {} {:?}",
                    p.track,
                    p.pts_ns,
                    p.duration_ns,
                    p.keyframe,
                    p.discard_padding_ns,
                    p.data.as_ref()
                )),
                Ok(None) => break,
                Err(error) => {
                    rows.push(format!("error {error:?}"));
                    break;
                }
            }
        }
    }
    rows
}

fn owned(raw: &[u8], track: usize, target: i64, playback: bool) -> Vec<String> {
    let limits = Limits::default();
    let pool = Budget::new(limits.live_storage_bytes).unwrap();
    let source = OwnedInputBytes::try_from_vec(&pool, raw.to_vec()).unwrap();
    session(
        BlockingReader::open_owned_seekable_in(
            source,
            limits,
            &pool,
            PayloadOwnership::SharedBounded,
        ),
        track,
        target,
        playback,
    )
}

fn ordinary(raw: &[u8], track: usize, target: i64, playback: bool) -> Vec<String> {
    session(
        BlockingReader::open_seekable_with_payload_ownership(
            Cursor::new(raw.to_vec()),
            Limits::default(),
            PayloadOwnership::SharedBounded,
        ),
        track,
        target,
        playback,
    )
}

fn owned_with(
    raw: &[u8],
    limits: Limits,
    policy: PayloadOwnership,
    target: i64,
    playback: bool,
) -> Vec<String> {
    let pool = Budget::new(limits.live_storage_bytes).unwrap();
    let source = OwnedInputBytes::try_from_vec(&pool, raw.to_vec()).unwrap();
    session(
        BlockingReader::open_owned_seekable_in(source, limits, &pool, policy),
        0,
        target,
        playback,
    )
}

fn ordinary_with(
    raw: &[u8],
    limits: Limits,
    policy: PayloadOwnership,
    target: i64,
    playback: bool,
) -> Vec<String> {
    session(
        BlockingReader::open_seekable_with_payload_ownership(
            Cursor::new(raw.to_vec()),
            limits,
            policy,
        ),
        0,
        target,
        playback,
    )
}

fn hits() -> usize {
    crate::matroska::typed::epoch::OWNED_PREFIX_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn owned_prefix_shortcut_matches_the_ordinary_scan_for_every_seek() {
    let before = hits();
    for raw in FIXTURES {
        for target in [
            0,
            250_000_000,
            1_000_000_000,
            3_000_000_000,
            7_500_000_000,
            12_000_000_000,
            15_000_000_000,
            40_000_000_000,
        ] {
            for track in [0, 1] {
                for playback in [false, true] {
                    assert_eq!(
                        owned(raw, track, target, playback),
                        ordinary(raw, track, target, playback),
                        "target {target} track {track} playback {playback}"
                    );
                }
            }
        }
    }
    assert!(
        hits() > before,
        "the shortcut must actually replace some ordinary scans"
    );
}

#[test]
fn owned_prefix_shortcut_matches_at_limit_boundaries_and_both_policies() {
    for raw in &FIXTURES[..2] {
        for policy in [
            PayloadOwnership::OwnedExact,
            PayloadOwnership::SharedBounded,
        ] {
            let base = owned_with(raw, Limits::default(), policy, 7_500_000_000, true);
            assert_eq!(
                base,
                ordinary_with(raw, Limits::default(), policy, 7_500_000_000, true)
            );
            for elements in (1..140)
                .step_by(3)
                .chain([40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50])
            {
                let limits = Limits {
                    elements,
                    ..Limits::default()
                };
                assert_eq!(
                    owned_with(raw, limits, policy, 7_500_000_000, true),
                    ordinary_with(raw, limits, policy, 7_500_000_000, true),
                    "elements {elements}"
                );
            }
            for scan in (0..60_000).step_by(997).chain(44_340..44_420) {
                let limits = Limits {
                    scan_bytes: scan,
                    ..Limits::default()
                };
                assert_eq!(
                    owned_with(raw, limits, policy, 7_500_000_000, true),
                    ordinary_with(raw, limits, policy, 7_500_000_000, true),
                    "scan {scan}"
                );
            }
        }
    }
}

#[test]
fn owned_prefix_shortcut_declines_structural_changes_like_the_ordinary_scan() {
    let raw = FIXTURES[0];
    // Rewrite one prefix element ID at a time (Cluster, Timestamp, SimpleBlock
    // headers) into Void, CRC-32, BlockGroup, Tags and an unknown ID.
    let mut starts = Vec::new();
    let mut at = 246;
    while at < 44_597 {
        starts.push(at);
        at += 41;
    }
    for &at in &starts {
        for id in [[0xec_u8], [0xbf], [0xa0], [0xa1], [0x80]] {
            let mut changed = raw.to_vec();
            changed[at] = id[0];
            assert_eq!(
                owned(&changed, 0, 7_500_000_000, true),
                ordinary(&changed, 0, 7_500_000_000, true),
                "at {at} id {id:x?}"
            );
        }
    }
}

#[test]
fn owned_prefix_shortcut_matches_the_ordinary_scan_on_truncated_and_damaged_input() {
    for raw in FIXTURES {
        for cut in (300..raw.len()).step_by(1777) {
            let short = &raw[..cut];
            assert_eq!(
                owned(short, 0, 7_500_000_000, true),
                ordinary(short, 0, 7_500_000_000, true),
                "cut {cut}"
            );
        }
        for at in (300..raw.len()).step_by(2311) {
            let mut damaged = raw.to_vec();
            damaged[at] ^= 0xa5;
            assert_eq!(
                owned(&damaged, 0, 7_500_000_000, true),
                ordinary(&damaged, 0, 7_500_000_000, true),
                "damage {at}"
            );
        }
    }
}
