use crate::support::players::*;
use crate::support::*;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error as C, InputCursor, SourceCapabilities, WorkBudget};
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4Open, OpenLimits},
    probe::{Probe, ProbeProgress, SelectedInput},
    *,
};

#[test]
fn actual_signature_probe_selects_mp4_ts_and_m2ts_without_file_names() {
    for (raw, want) in [
        (
            include_bytes!("../fixtures/mp4/r-front-stco.mov").as_slice(),
            Format::Mp4,
        ),
        (
            include_bytes!("../fixtures/ts/ts188-basic.ts").as_slice(),
            Format::Ts(PacketFormat::Ts188),
        ),
        (
            include_bytes!("../fixtures/ts/m2ts192-opaque-prefix.m2ts").as_slice(),
            Format::Ts(PacketFormat::M2ts192),
        ),
    ] {
        for q in [1, 7, 4096] {
            let (selected, _, _) = probe(raw, q, false);
            assert_eq!(selected.format, want);
            assert_eq!(selected.input.position(), 0);
        }
    }
}

#[test]
fn sequential_ts_probe_replays_owned_prefix_without_transport_rewind() {
    let raw = include_bytes!("../fixtures/ts/ts188-basic.ts");
    for q in [1, 7, 4096] {
        let (selected, b, calls) = probe(raw, q, true);
        let input = selected.input;
        let engine = Engine::ts(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
            triergon_mpegts::psi::DescriptorNamespace::Unspecified,
        )
        .unwrap();
        let mut player = Player::new(
            engine,
            selected.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let out = collect(&mut player, 1).unwrap();
        assert_eq!(pes(&out).len(), 1);
        let polls: Vec<_> = calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                if let Call::Poll(at) = c {
                    Some(*at)
                } else {
                    None
                }
            })
            .collect();
        assert!(polls.windows(2).all(|pair| pair[0] < pair[1]));
        drop(out);
        drop(player);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn probe_work_zero_and_cancellation_have_no_source_io_or_false_selection() {
    let b = pool();
    let (source, calls, _) = observed(&b, b"not a movie", 7, false);
    let mut p = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        64,
        source,
    )
    .unwrap();
    let before = b.used();
    for _ in 0..10 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            ProbeProgress::Yield
        ));
    }
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(b.used(), before);
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    drop(p);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[Call::Abort, Call::Close]
    );
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn cancelled_opener_and_probe_cannot_later_publish_ready() {
    let raw = include_bytes!("../fixtures/mp4/r-front-stco.mov");
    let b = pool();
    let (source, _, _) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let mut job = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        job.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    assert!(matches!(
        job.advance(&mut WorkBudget::new(100), &off()),
        Err(Error::Closed)
    ));
    assert!(job.finish().is_err());
    assert_eq!(b.used(), Budget::control_storage_bytes());
    let (source, _, _) = observed(&b, raw, 7, false);
    let mut probe = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
        source,
    )
    .unwrap();
    assert!(
        probe
            .advance(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .is_err()
    );
    assert!(matches!(
        probe.advance(&mut WorkBudget::new(100), &off()),
        Err(Error::Closed)
    ));
    assert!(probe.finish().is_err());
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn finite_prefix_ambiguity_is_rejected_from_bytes_not_corpus_identity() {
    let raw = [0x47; 580];
    let b = pool();
    let (source, _, _) = observed(&b, &raw, 4096, false);
    let mut probe = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(580),
            random_access: true,
        },
        4096,
        source,
    )
    .unwrap();
    let mut refused = false;
    for _ in 0..5000 {
        match probe.advance(&mut WorkBudget::new(11), &off()) {
            Err(C::Invalid("ambiguous transport signature")) => {
                refused = true;
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("wrong ambiguity {e:?}"),
        }
    }
    assert!(refused);
    assert!(probe.finish().is_err());
}
