#![forbid(unsafe_code)]
//! Source-reference regressions, independent of native equal-time winner policy.
use std::{
    io::{Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, InputWindow, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_matroska::{
    Error, Limits,
    incremental::ParseStep,
    typed::{BlockingReader, Event, PayloadOwnership, Reader},
};

fn raw(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cue-reference-next")
            .join(format!("{name}.mkv")),
    )
    .unwrap()
}
fn video() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cue-reference-next/source-avc-packet.bin"),
    )
    .unwrap()
}
struct Chunks {
    input: Cursor<Vec<u8>>,
    maximum: usize,
}
impl Read for Chunks {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = out.len().min(self.maximum);
        self.input.read(&mut out[..n])
    }
}
impl Seek for Chunks {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.input.seek(from)
    }
}
fn blocking(name: &str, policy: PayloadOwnership, maximum: usize) -> BlockingReader<Chunks> {
    BlockingReader::open_seekable_with_payload_ownership(
        Chunks {
            input: Cursor::new(raw(name)),
            maximum,
        },
        Limits::default(),
        policy,
    )
    .unwrap()
}
fn check_plain(name: &str, expected: usize) {
    let cancel = AtomicBool::new(false);
    for policy in [
        PayloadOwnership::OwnedExact,
        PayloadOwnership::SharedBounded,
    ] {
        for maximum in [1, 4096] {
            for eof in [false, true] {
                let mut r = blocking(name, policy, maximum);
                let first = r.next_typed_packet(&cancel).unwrap().unwrap();
                if eof {
                    while r.next_typed_packet(&cancel).unwrap().is_some() {}
                }
                let receipt = r.seek_previous(0, 1_500_000_000, &cancel).unwrap();
                assert_eq!(receipt.requested_ns, 1_500_000_000);
                assert_eq!(receipt.actual_ns, 1_000_000_000);
                assert_eq!(receipt.preroll_ns, 0);
                assert!(receipt.used_index);
                let mut packets = Vec::new();
                while let Some(p) = r.next_typed_packet(&cancel).unwrap() {
                    packets.push(p);
                }
                assert_eq!(packets.len(), expected, "{name} {maximum} eof={eof}");
                assert_eq!(packets[0].packet.pts_ns, Some(1_000_000_000));
                assert_eq!(packets.last().unwrap().packet.pts_ns, Some(2_000_000_000));
                for p in packets {
                    assert_eq!(p.packet.data.as_ref(), video());
                    assert_eq!(p.packet.dts_ns, None);
                    assert!(p.packet.keyframe);
                }
                assert_eq!(
                    first.packet.data.as_ref(),
                    video(),
                    "caller-owned old packet survives seek"
                );
            }
        }
    }
}

#[test]
fn explicit_later_simple_relative_position_is_not_the_earlier_same_time_rap() {
    check_plain("simple-relative-rap3", 4);
}
#[test]
fn explicit_later_simple_block_number_is_not_the_earlier_same_time_rap() {
    check_plain("simple-number-rap3", 4);
}
#[test]
fn consistent_relative_and_number_resolve_the_actual_group() {
    check_plain("group-both-rap3", 4);
}
#[test]
fn first_reference_controls_keep_the_existing_source_policy() {
    for name in [
        "simple-relative-rap1",
        "simple-number-rap1",
        "group-both-rap1",
    ] {
        check_plain(name, 4);
    }
}
#[test]
fn same_cluster_cue_permutations_preserve_each_selected_raw_reference() {
    // Existing source planner retains the last equal-time entry in this same
    // Cluster. This is a source policy check, not a normative/native tie rule.
    for kind in ["simple", "group"] {
        check_plain(&format!("{kind}-ties-forward"), 4);
        check_plain(&format!("{kind}-ties-reverse"), 4);
        check_plain(&format!("{kind}-ties-late-only"), 4);
    }
}
#[test]
fn referenced_final_lace_replays_the_whole_validated_batch() {
    check_plain("simple-final-rap-lacing-permitted", 5);
}
#[test]
fn codec_state_in_the_validated_prefix_is_kept_for_a_later_rap() {
    let cancel = AtomicBool::new(false);
    let mut r = blocking(
        "group-state-prefix-later-rap",
        PayloadOwnership::SharedBounded,
        7,
    );
    while r.next_typed_packet(&cancel).unwrap().is_some() {}
    let receipt = r.seek_previous(0, 1_000_000_000, &cancel).unwrap();
    assert_eq!(receipt.actual_ns, 1_000_000_000);
    let p = r.next_typed_packet(&cancel).unwrap().unwrap();
    let state = p.configuration.codec_state(0).unwrap();
    assert!(state.position().unwrap().segment_position > 0);
    assert_eq!(
        state.data(),
        p.configuration.tracks()[0].codec_private.as_ref()
    );
    assert_eq!(p.packet.dts_ns, None);
}
#[test]
fn contradictory_reference_fields_remain_a_strict_error() {
    let cancel = AtomicBool::new(false);
    let mut r = blocking("strict-contradictory", PayloadOwnership::OwnedExact, 4096);
    while r.next_typed_packet(&cancel).unwrap().is_some() {}
    assert!(matches!(
        r.seek_previous(0, 1_000_000_000, &cancel),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn non_boundary_and_outside_references_cannot_fabricate_a_landing() {
    let cancel = AtomicBool::new(false);
    for name in [
        "strict-inside-header",
        "strict-outside-cluster",
        "strict-number-outside",
    ] {
        let mut r = blocking(name, PayloadOwnership::SharedBounded, 17);
        while r.next_typed_packet(&cancel).unwrap().is_some() {}
        assert!(
            matches!(
                r.seek_previous(0, 1_000_000_000, &cancel),
                Err(Error::Invalid(_))
            ),
            "{name}"
        );
    }
}

fn drive(
    r: &mut Reader,
    source: &[u8],
    cap: usize,
    quantum: u64,
    events: &mut Vec<Event>,
) -> triergon_matroska::Result<()> {
    let cancel = AtomicBool::new(false);
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        let step = r.step(&mut work, &cancel)?;
        assert!(work.spent() <= quantum);
        match step {
            ParseStep::Yield => (),
            ParseStep::Finished => return Ok(()),
            ParseStep::Event(e) => events.push(e),
            ParseStep::NeedInput(request) => {
                let at = usize::try_from(request.offset).unwrap();
                assert!(at <= source.len());
                if at == source.len() {
                    r.finish(request.source, request.token)?;
                } else {
                    let end = (at + cap.min(request.maximum)).min(source.len());
                    let b = Bytes::copy_from_slice(r.budget(), &source[at..end])?;
                    r.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        b,
                    )?)?;
                }
            }
            other => panic!("bounded ample-pool drive failed: {other:?}"),
        }
    }
    panic!("bounded Cue operation stalled");
}
fn source_reader(bytes: &[u8], pool: &Budget, limits: Limits) -> Reader {
    Reader::new_with_payload_ownership(
        pool,
        SourceId(701),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        limits,
        4096,
        PayloadOwnership::SharedBounded,
    )
    .unwrap()
}
#[test]
fn work_zero_then_work_one_has_the_same_literal_landing_and_packets() {
    let bytes = raw("simple-both-rap3");
    let cancel = AtomicBool::new(false);
    for (cap, quantum) in [(1, 1), (17, 7), (4096, 8192)] {
        let pool = Budget::new(8 * 1024 * 1024).unwrap();
        let mut r = source_reader(&bytes, &pool, Limits::default());
        let mut events = Vec::new();
        drive(&mut r, &bytes, cap, quantum, &mut events).unwrap();
        events.clear();
        r.seek_previous(0, 1_000_000_000).unwrap();
        let before = r.position();
        let mut zero = WorkBudget::new(0);
        assert!(matches!(
            r.step(&mut zero, &cancel).unwrap(),
            ParseStep::Yield
        ));
        assert_eq!(r.position(), before);
        assert_eq!(zero.spent(), 0);
        drive(&mut r, &bytes, cap, quantum, &mut events).unwrap();
        let packets: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let Event::Packet(p) = e {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(packets.len(), 4);
        assert_eq!(packets[0].packet.pts_ns, Some(1_000_000_000));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Seek(s) if s.used_index && s.actual_ns==1_000_000_000))
        );
        assert!(packets.iter().all(|p| p.packet.dts_ns.is_none()));
    }
}
#[test]
fn an_earlier_invalid_lace_is_still_validated_before_the_referenced_rap() {
    let bytes = raw("strict-earlier-malformed-lace");
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let mut r = source_reader(&bytes, &pool, Limits::default());
    let mut events = Vec::new();
    // Opening only establishes catalogue/first Cluster, then start a real seek.
    let cancel = AtomicBool::new(false);
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(7);
        match r.step(&mut work, &cancel).unwrap() {
            ParseStep::NeedInput(req) => {
                let at = req.offset as usize;
                let end = (at + 17.min(req.maximum)).min(bytes.len());
                let b = Bytes::copy_from_slice(&pool, &bytes[at..end]).unwrap();
                r.provide(&InputWindow::new(req.source, req.token, req.offset, b).unwrap())
                    .unwrap();
            }
            ParseStep::Event(Event::Packet(_)) => break,
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("opening {other:?}"),
        }
    }
    r.seek_previous(0, 1_000_000_000).unwrap();
    assert!(matches!(
        drive(&mut r, &bytes, 17, 1, &mut events),
        Err(Error::Invalid(_))
    ));
    assert!(!events.iter().any(|e| matches!(e, Event::Seek(_))));
}
#[test]
fn caller_retention_cancel_transport_and_reset_keep_the_existing_lifecycle() {
    let bytes = raw("simple-both-rap3");
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut r = source_reader(&bytes, &pool, Limits::default());
    let mut events = Vec::new();
    drive(&mut r, &bytes, 4096, 8192, &mut events).unwrap();
    let held = events
        .iter()
        .find_map(|e| {
            if let Event::Packet(p) = e {
                Some(p.clone())
            } else {
                None
            }
        })
        .unwrap();
    events.clear();
    r.seek_previous(0, 1_000_000_000).unwrap();
    let stale = r.request();
    let old_generation = r.generation();
    let cancel = AtomicBool::new(true);
    let mut zero = WorkBudget::new(0);
    assert!(matches!(r.step(&mut zero, &cancel), Err(Error::Cancelled)));
    cancel.store(false, Ordering::Relaxed);
    r.reset().unwrap();
    assert_ne!(r.generation(), old_generation);
    let b = Bytes::copy_from_slice(&pool, b"x").unwrap();
    assert!(
        r.provide(&InputWindow::new(stale.source, stale.token, stale.offset, b).unwrap())
            .is_err()
    );
    let request = r.request();
    r.fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    let mut work = WorkBudget::new(0);
    assert!(r.step(&mut work, &cancel).is_err());
    drop(r);
    assert!(pool.used() > baseline);
    assert_eq!(held.packet.data.as_ref(), video());
    drop(held);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn inherited_flag_lacing_zero_is_a_strict_refusal_not_a_seek_recovery() {
    let cancel = AtomicBool::new(false);
    let mut r = blocking("simple-final-rap-laced", PayloadOwnership::OwnedExact, 7);
    let mut error = None;
    loop {
        match r.next_typed_packet(&cancel) {
            Ok(Some(_)) => (),
            Ok(None) => break,
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    assert!(matches!(
        error,
        Some(Error::Invalid("lacing forbidden by track"))
    ));
}

fn default_or_owned(name: &str, explicit: bool, maximum: usize) -> BlockingReader<Chunks> {
    let source = Chunks {
        input: Cursor::new(raw(name)),
        maximum,
    };
    let r = if explicit {
        BlockingReader::open_seekable_with_payload_ownership(
            source,
            Limits::default(),
            PayloadOwnership::OwnedExact,
        )
        .unwrap()
    } else {
        BlockingReader::open_seekable(source, Limits::default()).unwrap()
    };
    assert_eq!(r.payload_ownership(), PayloadOwnership::OwnedExact);
    r
}
#[test]
fn integration_default_and_explicit_owned_exact_replay_identical_retained_packets() {
    let cancel = AtomicBool::new(false);
    for maximum in [1, 4096] {
        let mut default = default_or_owned("simple-both-rap3", false, maximum);
        let mut explicit = default_or_owned("simple-both-rap3", true, maximum);
        let held_default = default.next_typed_packet(&cancel).unwrap().unwrap();
        let held_explicit = explicit.next_typed_packet(&cancel).unwrap().unwrap();
        for target in [1_000_000_000, 1_500_000_000, 0, 1_500_000_000] {
            let a = default.seek_previous(0, target, &cancel).unwrap();
            let b = explicit.seek_previous(0, target, &cancel).unwrap();
            assert_eq!(a.actual_ns, b.actual_ns);
            assert_eq!(a.used_index, b.used_index);
            assert_eq!(a.scanned_bytes, b.scanned_bytes);
            loop {
                let a = default.next_typed_packet(&cancel).unwrap();
                let b = explicit.next_typed_packet(&cancel).unwrap();
                match (a, b) {
                    (None, None) => break,
                    (Some(a), Some(b)) => {
                        assert_eq!(a.packet.data.as_ref(), b.packet.data.as_ref());
                        assert_eq!(
                            a.packet.data.retained_capacity(),
                            b.packet.data.retained_capacity()
                        );
                        assert_eq!(a.packet.timing, b.packet.timing);
                        assert_eq!(a.packet.dts_ns, None);
                        assert_eq!(b.packet.dts_ns, None);
                        assert_eq!(a.configuration.revision(), b.configuration.revision());
                        assert_eq!(a.configuration.generation(), b.configuration.generation());
                    }
                    _ => panic!("default changes source packet sequence"),
                }
            }
        }
        assert_eq!(held_default.packet.data.as_ref(), video());
        assert_eq!(held_explicit.packet.data.as_ref(), video());
    }
}
#[test]
fn integration_explicit_shared_reader_does_not_change_another_readers_default_policy() {
    let cancel = AtomicBool::new(false);
    let mut shared = blocking("group-both-rap3", PayloadOwnership::SharedBounded, 4096);
    let held = shared.next_typed_packet(&cancel).unwrap().unwrap();
    let mut default = default_or_owned("group-both-rap3", false, 4096);
    assert_eq!(shared.payload_ownership(), PayloadOwnership::SharedBounded);
    assert_eq!(default.payload_ownership(), PayloadOwnership::OwnedExact);
    for r in [&mut shared, &mut default] {
        let receipt = r.seek_previous(0, 1_000_000_000, &cancel).unwrap();
        assert!(receipt.used_index);
        let mut count = 0;
        while let Some(p) = r.next_typed_packet(&cancel).unwrap() {
            assert_eq!(p.packet.dts_ns, None);
            assert_eq!(p.packet.data.as_ref(), video());
            count += 1;
        }
        assert_eq!(count, 4);
    }
    drop(shared);
    drop(default);
    assert_eq!(held.packet.data.as_ref(), video());
}
#[test]
fn integration_default_keeps_strict_reference_and_lacing_refusals() {
    let cancel = AtomicBool::new(false);
    for name in [
        "strict-contradictory",
        "strict-inside-header",
        "strict-outside-cluster",
        "strict-number-outside",
    ] {
        let mut r = default_or_owned(name, false, 17);
        while r.next_typed_packet(&cancel).unwrap().is_some() {}
        assert!(matches!(
            r.seek_previous(0, 1_000_000_000, &cancel),
            Err(Error::Invalid(_))
        ));
    }
    for name in [
        "simple-final-rap-laced",
        "strict-earlier-malformed-lace-permitted-track",
    ] {
        let mut r = default_or_owned(name, false, 7);
        let mut refused = false;
        loop {
            match r.next_typed_packet(&cancel) {
                Ok(Some(_)) => (),
                Ok(None) => break,
                Err(Error::Invalid(_)) => {
                    refused = true;
                    break;
                }
                other => panic!("strict malformed stream {other:?}"),
            }
        }
        assert!(refused);
    }
}
