//! These guards use existing baseline fields/APIs, so a test-only module hook
//! can execute the storage regressions against unchanged tested PGS4 first.
use super::*;
use triergon_core::TimeBase;

fn clock(ticks: i64) -> PacketClock {
    PacketClock {
        pts: Some(ticks),
        dts: Some(ticks - 1),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn segment(raw: &mut Vec<u8>, kind: u8, body: &[u8]) {
    raw.push(kind);
    raw.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    raw.extend_from_slice(body);
}

fn presentation(state: u8) -> Vec<u8> {
    let mut raw = Vec::new();
    segment(&mut raw, 0x16, &[0, 1, 0, 1, 0x10, 0, 1, state, 0, 0, 0]);
    raw
}

fn push(d: &mut DisplayDecoder, b: &Budget, raw: &[u8], ticks: i64) {
    d.push(
        Bytes::copy_from_slice(b, raw).unwrap(),
        clock(ticks),
        Envelope::Packet,
        &AtomicBool::new(false),
    )
    .unwrap();
}

fn drive(d: &mut DisplayDecoder, quantum: u64) -> Result<DisplayStatus> {
    for _ in 0..2_000_000 {
        let status = d.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?;
        if status != DisplayStatus::Yield {
            return Ok(status);
        }
    }
    panic!("bounded context setup progress");
}

#[test]
fn pending_first_physical_fact_requires_no_heap_trace_backing() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    push(&mut d, &b, &presentation(0x80), 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    assert_eq!(
        d.frame.as_ref().unwrap().segments.capacity(),
        0,
        "PGS4 reserves four trace cells for a lone PCS"
    );
}

#[test]
fn fresh_source_context_does_not_allocate_seven_unused_palette_slots() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    push(&mut d, &b, &presentation(0x80), 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    assert_eq!(
        d.palettes.len(),
        1,
        "PGS4 initializes all eight slots before any PDS"
    );
    assert_eq!(d.palettes.capacity(), 1);
}

#[test]
fn complete_trace_contains_every_original_fact_once_in_literal_order() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    for quantum in [1, 2, 3, 17, 4096] {
        let mut d =
            DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
        let first = presentation(0x80);
        let mut raw = first.clone();
        let mut second = Vec::new();
        segment(&mut second, 0xee, &[3, 2, 1]);
        raw.extend_from_slice(&second);
        raw.extend_from_slice(&[0x80, 0, 0]);
        push(&mut d, &b, &raw, 11);
        assert_eq!(drive(&mut d, quantum), Ok(DisplayStatus::OutputReady));
        let out = d.take_output(&AtomicBool::new(false)).unwrap();
        let facts = out.segments();
        assert_eq!(facts.len(), 3);
        assert_eq!(
            facts.iter().map(|f| f.kind).collect::<Vec<_>>(),
            [0x16, 0xee, 0x80]
        );
        assert_eq!(facts[0].raw.as_ref(), first);
        assert_eq!(facts[1].raw.as_ref(), second);
        assert_eq!(facts[2].raw.as_ref(), [0x80, 0, 0]);
        assert_eq!(facts[0].stamp.packet_offset, 0);
        assert_eq!(facts[1].stamp.packet_offset, first.len());
        assert!(facts.iter().all(|f| f.stamp.packet_clock == clock(11)));
    }
    assert_eq!(b.used(), baseline);
}

#[test]
fn split_physical_packets_keep_distinct_clocks_and_actual_retained_owners() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let first = Bytes::copy_from_slice(&b, &presentation(0x80)).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    d.push(
        first.clone(),
        clock(11),
        Envelope::Packet,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    let second = [0xee, 0, 2, 9, 8, 0x80, 0, 0];
    push(&mut d, &b, &second, 22);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::OutputReady));
    let out = d.take_output(&AtomicBool::new(false)).unwrap();
    assert_eq!(out.segments().len(), 3);
    assert!(out.segments()[0].raw.shares_storage_with(&first));
    assert_eq!(out.segments()[0].stamp.packet_clock, clock(11));
    assert_eq!(out.segments()[1].stamp.packet_clock, clock(22));
    assert_ne!(
        out.segments()[0].stamp.packet_sequence,
        out.segments()[1].stamp.packet_sequence
    );
    assert_eq!(out.segments()[1].stamp.packet_offset, 0);
    drop(d);
    drop(first);
    assert_eq!(out.segments()[0].raw.as_ref(), presentation(0x80));
    assert!(b.used() > baseline);
    drop(out);
    assert_eq!(b.used(), baseline);
}

#[test]
fn pending_first_fact_still_counts_toward_the_original_segment_limit() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let limits = DisplayLimits {
        max_segments: 1,
        ..DisplayLimits::default()
    };
    let mut d = DisplayDecoder::new(&b, limits, DisplayConfig::default()).unwrap();
    push(&mut d, &b, &presentation(0x80), 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    push(&mut d, &b, &[0x80, 0, 0], 22);
    let expected = Error::Limit {
        requested: 2,
        maximum: 1,
    };
    assert_eq!(drive(&mut d, 1), Err(expected));
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(expected)
    );
}

#[test]
fn first_fact_pressure_retry_keeps_paid_credit_and_publishes_no_duplicate() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    push(&mut d, &b, &presentation(0x80), 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    push(&mut d, &b, &[0xee, 0, 1, 7, 0x80, 0, 0], 22);
    for _ in 0..1000 {
        if matches!(d.phase, Phase::Trace) {
            break;
        }
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
    }
    assert!(matches!(d.phase, Phase::Trace));
    let charged = d.work_spent();
    let pressure = b.reserve(b.limit() - b.used()).unwrap();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(DisplayStatus::Yield)
    );
    assert_eq!(d.work_spent(), charged);
    assert!(matches!(
        d.poll(&mut WorkBudget::new(2), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    let funded = d.work_spent();
    for _ in 0..3 {
        assert_eq!(
            d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
            Ok(DisplayStatus::Yield)
        );
        assert_eq!(d.work_spent(), funded);
    }
    drop(pressure);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::OutputReady));
    let out = d.take_output(&AtomicBool::new(false)).unwrap();
    assert_eq!(out.segments().len(), 3);
    assert_eq!(out.segments()[0].raw.as_ref(), presentation(0x80));
    assert_eq!(out.segments()[1].raw.as_ref(), [0xee, 0, 1, 7]);
}

#[test]
fn all_eight_palette_slots_grow_without_narrowing_the_original_limit() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let mut raw = presentation(0x80);
    for id in 0..8u8 {
        segment(&mut raw, 0x14, &[id, id, 1, id + 16, 128, 128, 255]);
    }
    segment(&mut raw, 0x80, &[]);
    push(&mut d, &b, &raw, 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::OutputReady));
    let out = d.take_output(&AtomicBool::new(false)).unwrap();
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    assert_eq!(out.segments().len(), 10);
    assert_eq!(d.palettes.len(), 8);
    assert_eq!(d.palettes.capacity(), 8);
    for (i, slot) in d.palettes.as_slice().iter().enumerate() {
        let image = slot.image.as_ref().unwrap();
        assert_eq!(image.id(), u8::try_from(i).unwrap());
        assert_eq!(
            image.source(1).unwrap().palette_version,
            u8::try_from(i).unwrap()
        );
    }
    let mut too_many = presentation(0);
    segment(&mut too_many, 0x14, &[8, 0]);
    push(&mut d, &b, &too_many, 22);
    assert_eq!(
        drive(&mut d, 1),
        Err(Error::Limit {
            requested: 9,
            maximum: 8
        })
    );
}

#[test]
fn cancel_pending_fact_releases_private_storage_and_keeps_caller_bytes() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let held = Bytes::copy_from_slice(&b, &presentation(0x80)).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    d.push(
        held.clone(),
        clock(11),
        Envelope::Packet,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    drop(d);
    assert_eq!(b.used(), baseline + held.retained_storage_bytes());
    assert_eq!(held.as_ref(), presentation(0x80));
    drop(held);
    assert_eq!(b.used(), baseline);
}

#[test]
fn lone_pcs_eof_remains_truncated_and_never_publishes_an_invented_display() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    push(&mut d, &b, &presentation(0x80), 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    assert_eq!(
        d.finish_input(&AtomicBool::new(false)),
        Err(Error::Truncated)
    );
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::Truncated)
    );
    assert!(d.output.is_none());
}

#[test]
fn full_palette_and_empty_version_carry_preserve_all_source_components() {
    for quantum in [1, 17, 4096] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let mut d =
            DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
        let mut raw = presentation(0x80);
        let mut entries = vec![0, 7];
        for i in 0..=255u8 {
            entries.extend_from_slice(&[i, i, i ^ 0xa5, i.wrapping_mul(7), i ^ 0x55]);
        }
        segment(&mut raw, 0x14, &entries);
        segment(&mut raw, 0x80, &[]);
        push(&mut d, &b, &raw, 11);
        assert_eq!(drive(&mut d, quantum), Ok(DisplayStatus::OutputReady));
        let out = d.take_output(&AtomicBool::new(false)).unwrap();
        let held = d.palettes.as_slice()[0].image.clone().unwrap();
        assert_eq!(drive(&mut d, quantum), Ok(DisplayStatus::NeedPacket));
        let mut carry = presentation(0);
        segment(&mut carry, 0x14, &[0, 8]);
        segment(&mut carry, 0x80, &[]);
        push(&mut d, &b, &carry, 22);
        assert_eq!(drive(&mut d, quantum), Ok(DisplayStatus::OutputReady));
        let fresh = d.take_output(&AtomicBool::new(false)).unwrap();
        let next = d.palettes.as_slice()[0].image.as_ref().unwrap();
        assert_eq!(next.version(), 8);
        for i in 0..=255u8 {
            assert_eq!(next.source(i), held.source(i));
            assert_eq!(next.source(i).unwrap().palette_version, 7);
            assert_eq!(next.native_argb(i), held.native_argb(i));
            assert_eq!(next.native_origin(i), NativeColourOrigin::ExplicitSource);
        }
        drop(fresh);
        drop(out);
        drop(d);
        assert!(held.source(255).is_some());
    }
}

#[test]
fn foreign_packet_rejection_keeps_pending_source_owner_and_clock() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let other = Budget::new(4096).unwrap();
    let mut d =
        DisplayDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default()).unwrap();
    let held = Bytes::copy_from_slice(&b, &presentation(0x80)).unwrap();
    d.push(
        held.clone(),
        clock(11),
        Envelope::Packet,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    let source_stamp = d.frame.as_ref().unwrap().presentation.stamp;
    assert_eq!(source_stamp.packet_clock, clock(11));
    let generation = d.generation();
    let foreign = Bytes::copy_from_slice(&other, &[0x80, 0, 0]).unwrap();
    let error = d
        .push(
            foreign.clone(),
            clock(999),
            Envelope::Packet,
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.error, Error::ForeignPool);
    assert!(error.packet.shares_storage_with(&foreign));
    assert_eq!(error.clock, clock(999));
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::ForeignPool)
    );
    assert_eq!(d.generation(), generation);
    assert!(d.output.is_none());
    assert_eq!(held.as_ref(), presentation(0x80));
    assert_eq!(source_stamp.packet_clock, clock(11));
    d.reset(&AtomicBool::new(false)).unwrap();
    assert_eq!(d.generation(), generation + 1);
    drop(d);
    assert_eq!(held.as_ref(), presentation(0x80));
}

#[test]
fn partial_palette_refusal_precedes_pressure_from_a_new_slot() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let limits = DisplayLimits {
        max_palettes: 2,
        ..DisplayLimits::default()
    };
    let mut d = DisplayDecoder::new(&b, limits, DisplayConfig::default()).unwrap();
    let mut first = presentation(0x80);
    segment(&mut first, 0x14, &[0, 0]);
    segment(&mut first, 0x80, &[]);
    push(&mut d, &b, &first, 11);
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::OutputReady));
    drop(d.take_output(&AtomicBool::new(false)).unwrap());
    assert_eq!(drive(&mut d, 1), Ok(DisplayStatus::NeedPacket));
    let mut invalid = presentation(0);
    segment(&mut invalid, 0x14, &[1, 0, 9]);
    push(&mut d, &b, &invalid, 22);
    for _ in 0..1000 {
        if matches!(d.phase, Phase::Dispatch) && d.info().unwrap().kind == 0x14 {
            break;
        }
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
    }
    assert!(matches!(d.phase, Phase::Dispatch));
    assert_eq!(d.info().unwrap().kind, 0x14);
    assert_eq!(
        d.frame
            .as_ref()
            .unwrap()
            .segments
            .as_slice()
            .last()
            .unwrap()
            .kind,
        0x14
    );
    let pressure = b.reserve(b.limit() - b.used()).unwrap();
    assert_eq!(
        d.poll(&mut WorkBudget::new(32), &AtomicBool::new(false)),
        Err(Error::Invalid("PGS partial palette entry"))
    );
    drop(pressure);
}
