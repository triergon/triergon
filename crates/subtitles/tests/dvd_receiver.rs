use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvd::{
    ControlPolicy, ControlState, TraditionalLimits, TraditionalSpuJob, TraditionalStatus,
    TraditionalSubpicture,
};

const COMPLETE: &[u8] = &[
    1, 3, 0x32, 0x10, 4, 0xff, 0xff, 5, 0, 0, 0, 0, 0, 1, 6, 0, 4, 0, 5, 0xff,
];

fn packet(commands: &[&[u8]]) -> Vec<u8> {
    let length = 6 + commands.iter().map(|c| 4 + c.len()).sum::<usize>();
    let mut out = Vec::new();
    out.extend_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
    out.extend_from_slice(&6_u16.to_be_bytes());
    out.extend_from_slice(&[0x40, 0x50]);
    for (i, block) in commands.iter().enumerate() {
        let at = out.len();
        let next = if i + 1 == commands.len() {
            at
        } else {
            at + 4 + block.len()
        };
        out.extend_from_slice(&(if i == 0 { 17_u16 } else { 90 }).to_be_bytes());
        out.extend_from_slice(&u16::try_from(next).unwrap().to_be_bytes());
        out.extend_from_slice(block);
    }
    out
}

fn decode(raw: &[u8], policy: ControlPolicy, quantum: u64) -> Result<TraditionalSubpicture, Error> {
    let budget = Budget::new(1 << 20)?;
    let mut job = TraditionalSpuJob::with_policy(
        Bytes::copy_from_slice(&budget, raw)?,
        ControlState::default(),
        None,
        &budget,
        TraditionalLimits::default(),
        policy,
    )?;
    let cancel = AtomicBool::new(false);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(quantum);
        let status = job.poll(&mut work, &cancel)?;
        assert!(work.spent() <= quantum);
        if status == TraditionalStatus::Ready {
            return job.try_finish(&cancel);
        }
    }
    panic!("bounded receiver did not terminate");
}

#[test]
fn opaque_receiver_stop_retains_every_operand_and_does_not_invent_commands() {
    for opcode in [7, 0x7e] {
        let mut commands = COMPLETE[..COMPLETE.len() - 1].to_vec();
        commands.extend_from_slice(&[opcode, 2, 0, 1, 0xff]);
        let raw = packet(&[&commands]);
        assert!(matches!(
            decode(&raw, ControlPolicy::Strict, 1),
            Err(Error::Unsupported(_))
        ));
        for quantum in [1, 4096] {
            let picture = decode(&raw, ControlPolicy::NativeD32, quantum).unwrap();
            assert!(picture.receiver_emits_bitmap());
            assert_eq!(picture.program().raw().as_ref(), raw);
            assert_eq!(picture.receiver_display_times().end_ms, 0);
            assert_eq!(picture.program().last_stop(), None);
            let fact = picture.program().controls()[0];
            let opaque = fact.opaque.unwrap();
            assert_eq!(opaque.opcode, opcode);
            assert_eq!(
                &raw[usize::from(opaque.operands_start)..usize::from(opaque.operands_end)],
                &[2, 0, 1, 0xff]
            );
        }
    }
}

#[test]
fn truncated_receiver_operands_drop_the_event_but_keep_partial_source_state() {
    for tail in [&[5, 0, 0][..], &[6, 0][..]] {
        let raw = packet(&[COMPLETE, tail]);
        assert!(matches!(
            decode(&raw, ControlPolicy::Strict, 1),
            Err(Error::Truncated)
        ));
        for quantum in [1, 4096] {
            let picture = decode(&raw, ControlPolicy::NativeD32, quantum).unwrap();
            assert!(!picture.receiver_emits_bitmap());
            assert_eq!(picture.program().raw().as_ref(), raw);
            assert_eq!(picture.program().controls().len(), 2);
            let fault = picture.program().truncated_control().unwrap();
            assert_eq!(fault.opcode, Some(tail[0]));
            assert_eq!(fault.command_offset, fault.control_offset + 4);
            assert_eq!(picture.program().state_after().palette_word, Some(0x3210));
            assert_eq!(picture.program().state_after().alpha_word, Some(0xffff));
        }
    }
}

#[test]
fn invalid_link_recovery_preserves_the_original_link_and_valid_prefix_clock() {
    for link in [0_u16, 5, 0xfffe] {
        let mut raw = packet(&[COMPLETE, &[2, 0xff]]);
        let last = 6 + 4 + COMPLETE.len();
        raw[last + 2..last + 4].copy_from_slice(&link.to_be_bytes());
        assert!(matches!(
            decode(&raw, ControlPolicy::Strict, 1),
            Err(Error::Invalid(_))
        ));
        for quantum in [1, 4096] {
            let picture = decode(&raw, ControlPolicy::NativeD32, quantum).unwrap();
            assert!(picture.receiver_emits_bitmap());
            assert_eq!(picture.receiver_display_times().end_ms, 1024);
            let last = picture.program().controls().last().unwrap();
            assert_eq!(last.next_offset, link);
            assert!(last.receiver_stopped_at_link);
            assert_eq!(picture.program().raw().as_ref(), raw);
        }
    }
}

#[test]
fn explicit_receiver_profile_preserves_valid_strict_output() {
    let raw = packet(&[COMPLETE, &[2, 0xff]]);
    let strict = decode(&raw, ControlPolicy::Strict, 1).unwrap();
    let native = decode(&raw, ControlPolicy::NativeD32, 4096).unwrap();
    assert_eq!(
        strict.receiver_display_times(),
        native.receiver_display_times()
    );
    assert_eq!(
        strict.program().state_after(),
        native.program().state_after()
    );
    let a = strict.bitmap().unwrap().native_view().unwrap();
    let b = native.bitmap().unwrap().native_view().unwrap();
    assert_eq!(a.crop(), b.crop());
    assert_eq!(a.palette(), b.palette());
    for row in 0..a.crop().height {
        assert_eq!(a.indices_row(row).unwrap(), b.indices_row(row).unwrap());
    }
    assert!(
        native
            .program()
            .controls()
            .iter()
            .all(|c| !c.receiver_stopped_at_link)
    );
    assert_eq!(native.program().truncated_control(), None);
}

#[test]
fn absent_end_is_an_explicit_source_fault_even_when_receiver_has_no_event() {
    let raw = packet(&[&COMPLETE[..COMPLETE.len() - 1]]);
    let picture = decode(&raw, ControlPolicy::NativeD32, 1).unwrap();
    let fault = picture.program().truncated_control().unwrap();
    assert_eq!(usize::from(fault.command_offset), raw.len());
    assert_eq!(fault.opcode, None);
    assert!(!picture.receiver_emits_bitmap());
    assert_eq!(picture.program().raw().as_ref(), raw);
}

#[test]
fn extended_receiver_controls_cannot_be_misreported_as_four_colour_output() {
    for opcode in 0x83..=0x86 {
        let mut commands = COMPLETE[..COMPLETE.len() - 1].to_vec();
        commands.push(opcode);
        let raw = packet(&[&commands]);
        assert_eq!(
            decode(&raw, ControlPolicy::NativeD32, 1).unwrap_err(),
            Error::Unsupported("DVD extended receiver control")
        );
    }
}

#[test]
fn recovery_cannot_bypass_control_count_or_work_limits() {
    let raw = packet(&[COMPLETE, &[2, 0xff]]);
    for work_limit in [false, true] {
        let budget = Budget::new(1 << 20).unwrap();
        let mut limits = TraditionalLimits::default();
        if work_limit {
            limits.program.max_work = 2;
        } else {
            limits.program.max_controls = 1;
        }
        let mut job = TraditionalSpuJob::with_policy(
            Bytes::copy_from_slice(&budget, &raw).unwrap(),
            ControlState::default(),
            None,
            &budget,
            limits,
            ControlPolicy::NativeD32,
        )
        .unwrap();
        assert!(matches!(
            job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Err(Error::Limit { .. })
        ));
    }
}

#[test]
fn receiver_zero_work_and_cancellation_preserve_accounting_and_are_sticky() {
    let raw = packet(&[COMPLETE]);
    let budget = Budget::new(1 << 20).unwrap();
    let mut job = TraditionalSpuJob::with_policy(
        Bytes::copy_from_slice(&budget, &raw).unwrap(),
        ControlState::default(),
        None,
        &budget,
        TraditionalLimits::default(),
        ControlPolicy::NativeD32,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let used = budget.used();
    for _ in 0..8 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            TraditionalStatus::Yield
        );
        assert_eq!(budget.used(), used);
    }
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &cancel).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(budget.used(), Budget::control_storage_bytes());
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &cancel).unwrap_err(),
        Error::Cancelled
    );
}
