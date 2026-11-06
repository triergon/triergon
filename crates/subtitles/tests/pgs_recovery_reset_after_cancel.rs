//! Public-API regression for recovery reset after cancellation.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::compat::pgs::RecoveryDecoder;
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayLimits, PacketClock, RecoveredPacket, RecoveryStatus,
};

fn packet() -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, body) in [
        (
            0x16,
            &[
                0, 8, 0, 4, 0x10, 0, 1, 0x80, 0, 0, 1, 0, 7, 0, 0, 0, 1, 0, 2,
            ][..],
        ),
        (
            0x14,
            &[0, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128][..],
        ),
        (0x15, &[0, 7, 0, 0xc0, 0, 0, 8, 0, 2, 0, 1, 1, 2, 0, 0][..]),
        (0x80, &[][..]),
    ] {
        out.push(kind);
        out.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}
fn decode(decoder: &mut RecoveryDecoder, budget: &Budget, cancel: &AtomicBool) -> RecoveredPacket {
    let mut input = Some(Bytes::copy_from_slice(budget, &packet()).unwrap());
    let clock = PacketClock {
        pts: Some(90_000),
        dts: None,
        timebase: TimeBase::new(1, 90_000).unwrap(),
    };
    decoder.push(&mut input, clock, cancel).unwrap();
    for _ in 0..100_000 {
        if decoder.poll(&mut WorkBudget::new(1), cancel).unwrap() == RecoveryStatus::OutputReady {
            return decoder.take_output(cancel).unwrap();
        }
    }
    panic!("bounded fixture did not produce output");
}
#[test]
fn reset_after_cancel_advances_source_generation_while_old_output_remains_valid() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder =
        RecoveryDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let old = decode(&mut decoder, &budget, &cancel);
    assert_eq!(old.source_error(), None);
    let old_generation = old.source_sets()[0].generation;
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        decoder.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    decoder.reset(&cancel).unwrap();
    let fresh = decode(&mut decoder, &budget, &cancel);
    assert_eq!(fresh.source_error(), None);
    assert_eq!(old.source_sets()[0].objects()[0].pixels().as_ref(), &[1, 2]);
    assert_eq!(
        fresh.source_sets()[0].generation,
        old_generation + 1,
        "reset must advance the strict source generation, including recovery from cancellation"
    );
}

#[test]
fn repeated_cancel_reset_preserves_all_retained_source_generations_and_storage() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder =
        RecoveryDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let mut held = Vec::new();
    for generation in 0..8 {
        let output = decode(&mut decoder, &budget, &cancel);
        assert_eq!(output.source_sets()[0].generation, generation);
        held.push(output);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            decoder.poll(&mut WorkBudget::new(0), &cancel),
            Err(Error::Cancelled)
        );
        let poisoned_used = budget.used();
        for _ in 0..4 {
            assert_eq!(decoder.reset(&cancel), Err(Error::Cancelled));
            assert_eq!(
                decoder.poll(&mut WorkBudget::new(4096), &cancel),
                Err(Error::Cancelled)
            );
            assert_eq!(budget.used(), poisoned_used);
        }
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            decoder.poll(&mut WorkBudget::new(0), &cancel),
            Err(Error::Cancelled)
        );
        decoder.reset(&cancel).unwrap();
        assert_eq!(budget.used(), poisoned_used);
    }
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        decoder.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    let before_drop = budget.used();
    drop(decoder);
    assert_eq!(
        budget.used(),
        before_drop,
        "poison must release the private graph before decoder drop"
    );
    for (generation, output) in held.iter().enumerate() {
        assert_eq!(output.source_sets()[0].generation, generation as u64);
        assert_eq!(
            output.source_sets()[0].objects()[0].pixels().as_ref(),
            &[1, 2]
        );
        assert_eq!(output.raw_packet().as_ref(), packet());
    }
    drop(held);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn cancelled_push_and_cancelled_take_do_not_recreate_source_identity() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder =
        RecoveryDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let old = decode(&mut decoder, &budget, &cancel);
    cancel.store(true, Ordering::Relaxed);
    let mut input = Some(Bytes::copy_from_slice(&budget, &packet()).unwrap());
    let owner = input.as_ref().unwrap().clone();
    let clock = PacketClock {
        pts: None,
        dts: None,
        timebase: TimeBase::new(1, 90_000).unwrap(),
    };
    assert_eq!(
        decoder.push(&mut input, clock, &cancel),
        Err(Error::Cancelled)
    );
    assert!(input.as_ref().unwrap().shares_storage_with(&owner));
    cancel.store(false, Ordering::Relaxed);
    decoder.reset(&cancel).unwrap();
    let fresh = decode(&mut decoder, &budget, &cancel);
    assert_eq!(fresh.source_sets()[0].generation, 1);
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        decoder.take_output(&cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    decoder.reset(&cancel).unwrap();
    let final_output = decode(&mut decoder, &budget, &cancel);
    assert_eq!(final_output.source_sets()[0].generation, 2);
    assert_eq!(old.source_sets()[0].generation, 0);
    assert_eq!(old.source_sets()[0].objects()[0].pixels().as_ref(), &[1, 2]);
}

#[test]
fn cancel_before_first_packet_and_empty_resets_each_advance_exactly_once() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(true);
    let mut decoder =
        RecoveryDecoder::new(&budget, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    for _ in 0..3 {
        assert_eq!(decoder.reset(&cancel), Err(Error::Cancelled));
    }
    cancel.store(false, Ordering::Relaxed);
    for _ in 0..4 {
        decoder.reset(&cancel).unwrap();
    }
    let output = decode(&mut decoder, &budget, &cancel);
    assert_eq!(output.source_sets()[0].generation, 4);
}

#[test]
fn work_limit_poison_releases_graph_without_reusing_zero_generation() {
    let budget = Budget::new(4 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let limits = DisplayLimits {
        max_work: 1,
        ..DisplayLimits::default()
    };
    let mut decoder = RecoveryDecoder::new(&budget, limits, DisplayConfig::default()).unwrap();
    for _ in 0..3 {
        let mut input = Some(Bytes::copy_from_slice(&budget, &packet()).unwrap());
        decoder
            .push(
                &mut input,
                PacketClock {
                    pts: None,
                    dts: None,
                    timebase: TimeBase::new(1, 90_000).unwrap(),
                },
                &cancel,
            )
            .unwrap();
        assert!(matches!(
            decoder.poll(&mut WorkBudget::new(4096), &cancel),
            Err(Error::Limit { .. })
        ));
        assert_eq!(budget.used(), Budget::control_storage_bytes());
        decoder.reset(&cancel).unwrap();
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}
