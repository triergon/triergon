//! The immutable-source state-history shortcut must be indistinguishable from
//! the ordinary scan: identical receipts (including scanned bytes), packets and
//! errors for every seek fixture, target and truncation.
use crate::BlockingSource;
use crate::reader::blocking::*;
use crate::storage::PayloadOwnership;
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_core::{Budget, OwnedInputBytes};

const FIXTURES: [&[u8]; 8] = [
    include_bytes!("../../../../tests/fixtures/seek/cues-end.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-relative.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-front.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-preroll.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-delay-preroll.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-sparse.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-unknown-cluster.mkv"),
    include_bytes!("../../../../tests/fixtures/seek/cues-missing.mkv"),
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

#[track_caller]
fn compare_sources(
    raw: &[u8],
    limits: Limits,
    policy: PayloadOwnership,
    track: usize,
    target: i64,
    playback: bool,
    case: std::fmt::Arguments<'_>,
) {
    let pool = Budget::new(limits.live_storage_bytes).unwrap();
    let input = OwnedInputBytes::try_from_vec(&pool, raw.to_vec()).unwrap();
    let owned = BlockingReader::open_source_in(
        BlockingSource::owned(input).with_payload_ownership(policy),
        limits,
        &pool,
    );
    let ordinary = BlockingReader::open_source(
        BlockingSource::from(Cursor::new(raw.to_vec())).with_payload_ownership(policy),
        limits,
    );
    assert_eq!(
        session(owned, track, target, playback),
        session(ordinary, track, target, playback),
        "{case}"
    );
}

fn hits() -> usize {
    crate::seek::epoch::OWNED_PREFIX_HITS.load(std::sync::atomic::Ordering::Relaxed)
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
                    compare_sources(
                        raw,
                        Limits::default(),
                        PayloadOwnership::SharedBounded,
                        track,
                        target,
                        playback,
                        format_args!("target {target} track {track} playback {playback}"),
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
            compare_sources(
                raw,
                Limits::default(),
                policy,
                0,
                7_500_000_000,
                true,
                format_args!("default limits"),
            );
            for elements in (1..140)
                .step_by(3)
                .chain([40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50])
            {
                let limits = Limits {
                    elements,
                    ..Limits::default()
                };
                compare_sources(
                    raw,
                    limits,
                    policy,
                    0,
                    7_500_000_000,
                    true,
                    format_args!("elements {elements}"),
                );
            }
            for scan in (0..60_000).step_by(997).chain(44_340..44_420) {
                let limits = Limits {
                    scan_bytes: scan,
                    ..Limits::default()
                };
                compare_sources(
                    raw,
                    limits,
                    policy,
                    0,
                    7_500_000_000,
                    true,
                    format_args!("scan {scan}"),
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
            compare_sources(
                &changed,
                Limits::default(),
                PayloadOwnership::SharedBounded,
                0,
                7_500_000_000,
                true,
                format_args!("at {at} id {id:x?}"),
            );
        }
    }
}

#[test]
fn owned_prefix_shortcut_matches_the_ordinary_scan_on_truncated_and_damaged_input() {
    for raw in FIXTURES {
        for cut in (300..raw.len()).step_by(1777) {
            let short = &raw[..cut];
            compare_sources(
                short,
                Limits::default(),
                PayloadOwnership::SharedBounded,
                0,
                7_500_000_000,
                true,
                format_args!("cut {cut}"),
            );
        }
        for at in (300..raw.len()).step_by(2311) {
            let mut damaged = raw.to_vec();
            damaged[at] ^= 0xa5;
            compare_sources(
                &damaged,
                Limits::default(),
                PayloadOwnership::SharedBounded,
                0,
                7_500_000_000,
                true,
                format_args!("damage {at}"),
            );
        }
    }
}
