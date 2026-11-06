//! Literal receiver dispatch, funding, pressure, errors and retained-owner guards.
//! These test output-neutral semantics; no synthetic failing performance oracle.
use super::*;
use triergon_core::TimeBase;

fn segment(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
    out.extend_from_slice(payload);
}

fn pcs(state: u8, ids: &[u16]) -> Vec<u8> {
    let mut body = vec![0, 16, 0, 1, 0x10, 0, 1, state, 0, 0, ids.len() as u8];
    for (i, id) in ids.iter().enumerate() {
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 3 + i as u8, 0, 0]);
    }
    let mut out = Vec::new();
    segment(&mut out, 0x16, &body);
    out
}

fn clear_packet() -> Vec<u8> {
    let mut out = pcs(0x80, &[]);
    segment(&mut out, 0x80, &[]);
    out
}

fn picture_packet() -> Vec<u8> {
    let mut out = pcs(0x80, &[7]);
    segment(&mut out, 0x14, &[0, 0, 1, 235, 128, 128, 255]);
    segment(
        &mut out,
        0x15,
        &[0, 7, 0, 0xc0, 0, 0, 7, 0, 1, 0, 1, 1, 0, 0],
    );
    segment(&mut out, 0x80, &[]);
    out
}

fn clock() -> PacketClock {
    PacketClock {
        pts: Some(123),
        dts: Some(-17),
        timebase: TimeBase::new(1, 90_000).unwrap(),
    }
}

fn seeded(b: &Budget, data: &[u8], limits: DisplayLimits) -> (IndexedNativeRecoveryDecoder, Bytes) {
    let mut d = IndexedNativeRecoveryDecoder::new(b, limits, DisplayConfig::default()).unwrap();
    let held = Bytes::copy_from_slice(b, data).unwrap();
    let mut input = Some(held.clone());
    d.push(&mut input, clock(), &AtomicBool::new(false))
        .unwrap();
    assert!(input.is_none());
    (d, held)
}

fn reach_dispatch(d: &mut IndexedNativeRecoveryDecoder) {
    for _ in 0..100_000 {
        if d.phase == Phase::Segment && d.at == 0 {
            return;
        }
        let status = d
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
        assert_ne!(status, RecoveryStatus::OutputReady);
    }
    panic!("bounded first receiver dispatch");
}

fn first_cost(d: &IndexedNativeRecoveryDecoder) -> usize {
    let raw = d.packet.as_ref().unwrap();
    3 + usize::from(u16::from_be_bytes([raw[1], raw[2]])) + d.objects.len() + d.palettes.len() + 256
}

fn finish(d: &mut IndexedNativeRecoveryDecoder, quantum: u64) -> IndexedRecoveredPacket {
    for _ in 0..100_000 {
        if d.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
            == RecoveryStatus::OutputReady
        {
            return d.take_output(&AtomicBool::new(false)).unwrap();
        }
    }
    panic!("bounded complete receiver packet");
}

#[test]
fn literal_source_receiver_and_raw_owners_survive_reset_and_producer_drop() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let data = picture_packet();
    let (mut d, held) = seeded(&b, &data, DisplayLimits::default());
    let output = finish(&mut d, 7);
    assert_eq!(output.clock, clock());
    assert_eq!(output.raw_packet().as_ref(), data.as_slice());
    assert!(output.raw_packet().shares_storage_with(&held));
    assert_eq!(output.source_error(), None);
    assert_eq!(output.context_canvas, [16, 1]);
    assert_eq!(output.source_sets().len(), 1);
    let source = &output.source_sets()[0];
    assert_eq!(source.presentation.width, 16);
    assert_eq!(source.presentation.height, 1);
    assert_eq!(source.presentation.stamp.packet_clock, clock());
    assert_eq!(source.objects().len(), 1);
    assert_eq!(source.objects()[0].pixels().as_ref(), &[1]);
    let colour = source.objects()[0].palette().source(1).unwrap();
    assert_eq!(
        (colour.y, colour.cr, colour.cb, colour.alpha),
        (235, 128, 128, 255)
    );
    assert_eq!(colour.stamp.packet_clock, clock());
    assert_eq!(output.receiver_sets().len(), 1);
    assert_eq!(output.receiver_sets()[0].pts_microseconds, Some(1367));
    let rect = &output.receiver_sets()[0].rectangles()[0];
    assert_eq!(
        (rect.x, rect.y, rect.width, rect.height, rect.stride),
        (3, 0, 1, 1, 1)
    );
    assert_eq!(rect.pixels().as_ref(), &[1]);
    assert_eq!(rect.palette()[1], 0xffff_ffff);
    d.reset(&AtomicBool::new(false)).unwrap();
    drop(d);
    drop(held);
    assert_eq!(output.raw_packet().as_ref(), data.as_slice());
    assert_eq!(
        output.receiver_sets()[0].rectangles()[0].pixels().as_ref(),
        &[1]
    );
    assert_eq!(
        output.source_sets()[0].objects()[0]
            .palette()
            .source(1)
            .unwrap()
            .y,
        235
    );
    assert!(b.used() > baseline);
    drop(output);
    assert_eq!(b.used(), baseline);
}

#[test]
fn partial_funding_and_work0_leave_pcs_and_input_identity_unpublished() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let data = clear_packet();
    let (mut d, held) = seeded(&b, &data, DisplayLimits::default());
    reach_dispatch(&mut d);
    let cost = first_cost(&d);
    let previous_credit = d.credit;
    let unpaid = cost - previous_credit;
    assert!(unpaid > 0);
    let offer = unpaid - 1;
    assert_eq!(previous_credit + offer, cost - 1);
    let spent = d.spent;
    let used = b.used();
    assert_eq!(cost, 14 + 256);
    assert_eq!(
        d.poll(&mut WorkBudget::new(offer as u64), &AtomicBool::new(false))
            .unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!((d.at, d.segments, d.epoch), (0, 0, 0));
    assert_eq!(d.canvas, [0, 0]);
    assert_eq!(
        (d.spent, d.credit),
        (spent + offer, previous_credit + offer)
    );
    for _ in 0..3 {
        assert_eq!(
            d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            RecoveryStatus::Yield
        );
        assert_eq!(
            (d.spent, d.credit, b.used()),
            (spent + offer, previous_credit + offer, used)
        );
        assert!(d.packet.as_ref().unwrap().shares_storage_with(&held));
        assert_eq!(d.canvas, [0, 0]);
    }
    let out = finish(&mut d, 1);
    assert_eq!(out.receiver_sets().len(), 1);
    assert!(out.receiver_sets()[0].rectangles().is_empty());
}

#[test]
fn funded_pcs_reference_pressure_keeps_credit_and_commits_exactly_once() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let mut data = pcs(0x80, &[7, 9]);
    segment(&mut data, 0x80, &[]);
    let (mut d, held) = seeded(&b, &data, DisplayLimits::default());
    reach_dispatch(&mut d);
    let cost = first_cost(&d);
    let spent = d.spent;
    assert!(d.references.is_empty() && d.references.capacity() == 0);
    let pressure = b.reserve(b.available()).unwrap();
    assert!(matches!(
        d.poll(&mut WorkBudget::new(cost as u64), &AtomicBool::new(false)),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!((d.spent, d.credit), (spent + cost, cost));
    assert_eq!((d.at, d.segments, d.epoch), (0, 0, 0));
    assert_eq!(d.canvas, [0, 0]);
    assert!(d.packet.as_ref().unwrap().shares_storage_with(&held));
    drop(pressure);
    let before = b.used();
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!((d.spent, d.credit, b.used()), (spent + cost, cost, before));
    assert_eq!(d.references.capacity(), 0);
    assert_eq!(
        d.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        RecoveryStatus::Yield
    );
    assert_eq!((d.epoch, d.segments, d.at), (1, 1, 3 + 11 + 16));
    assert_eq!(d.canvas, [16, 1]);
    assert_eq!(
        d.references
            .as_slice()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [7, 9]
    );
    assert_eq!((d.spent, d.credit), (spent + cost + 1, 1));
    let _out = finish(&mut d, 7);
    assert_eq!(d.epoch, 1);
}

#[test]
fn cancellation_precedes_work0_and_keeps_external_raw_bytes_alive() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = b.used();
    let data = clear_packet();
    let (mut d, held) = seeded(&b, &data, DisplayLimits::default());
    reach_dispatch(&mut d);
    let cost = first_cost(&d);
    assert_eq!(
        d.poll(
            &mut WorkBudget::new((cost - 1) as u64),
            &AtomicBool::new(false)
        )
        .unwrap(),
        RecoveryStatus::Yield
    );
    let spent = d.spent;
    let cancel = AtomicBool::new(true);
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(d.spent, spent);
    assert!(d.packet.is_none());
    assert_eq!(held.as_ref(), data.as_slice());
    assert_eq!(
        d.poll(&mut WorkBudget::new(10), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    );
    drop(d);
    assert_eq!(b.used(), baseline + held.retained_storage_bytes());
    drop(held);
    assert_eq!(b.used(), baseline);
}

#[test]
fn foreign_push_under_pressure_returns_the_exact_caller_owner() {
    let b = Budget::new(65536).unwrap();
    let foreign = Budget::new(65536).unwrap();
    let mut d =
        IndexedNativeRecoveryDecoder::new(&b, DisplayLimits::default(), DisplayConfig::default())
            .unwrap();
    let data = clear_packet();
    let mut input = Some(Bytes::copy_from_slice(&foreign, &data).unwrap());
    let pointer = input.as_ref().unwrap().as_ptr();
    let pressure = b.reserve(b.available()).unwrap();
    assert_eq!(
        d.push(&mut input, clock(), &AtomicBool::new(false)),
        Err(Error::ForeignPool)
    );
    assert_eq!(input.as_ref().unwrap().as_ptr(), pointer);
    assert_eq!(input.as_ref().unwrap().as_ref(), data.as_slice());
    assert_eq!(d.phase, Phase::Idle);
    assert_eq!(d.spent, 0);
    drop(pressure);
}

#[test]
fn short_physical_segment_keeps_raw_and_separate_source_refusal() {
    let b = Budget::new(4 * 1024 * 1024).unwrap();
    let data = [0x7f, 0, 5, 0xaa, 0xbb];
    let (mut d, held) = seeded(&b, &data, DisplayLimits::default());
    let out = finish(&mut d, 1);
    assert_eq!(out.raw_packet().as_ref(), &data);
    assert!(out.raw_packet().shares_storage_with(&held));
    assert_eq!(out.source_error(), Some(Error::Truncated));
    assert!(out.receiver_observations.truncated_segment_tail);
    assert_eq!(out.receiver_observations.palette_cross_segment_bytes, 0);
    assert_eq!(out.receiver_observations.palette_zero_padding_bytes, 0);
    assert!(out.receiver_sets().is_empty());
}

#[test]
fn first_clear_keeps_complete_work_and_literal_outputs_for_all_quanta() {
    let mut complete_work = None;
    for q in [1, 2, 7, 255, 256, 4096] {
        let b = Budget::new(4 * 1024 * 1024).unwrap();
        let (mut d, held) = seeded(&b, &clear_packet(), DisplayLimits::default());
        let out = finish(&mut d, q);
        assert_eq!(out.context_canvas, [16, 1]);
        assert_eq!(out.clock, clock());
        assert_eq!(out.receiver_sets().len(), 1);
        assert!(out.receiver_sets()[0].rectangles().is_empty());
        assert_eq!(out.receiver_sets()[0].pts_microseconds, Some(1367));
        assert_eq!(out.source_error(), None);
        assert_eq!(out.source_sets().len(), 1);
        assert!(out.source_sets()[0].is_clear());
        assert!(out.raw_packet().shares_storage_with(&held));
        assert_eq!(d.segments, 2);
        match complete_work {
            None => complete_work = Some(d.work_spent()),
            Some(expected) => assert_eq!(d.work_spent(), expected),
        }
    }
}

#[test]
fn exhausted_cumulative_work_latches_before_any_receiver_publication() {
    let b = Budget::new(65536).unwrap();
    let limits = DisplayLimits {
        max_work: 1,
        ..DisplayLimits::default()
    };
    let (mut d, held) = seeded(&b, &clear_packet(), limits);
    let error = Error::Limit {
        requested: 2,
        maximum: 1,
    };
    assert_eq!(
        d.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(error)
    );
    assert_eq!(d.spent, 1);
    assert_eq!(
        d.poll(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(error)
    );
    assert!(d.packet.is_none() && d.output.is_none());
    assert_eq!(held.as_ref(), clear_packet().as_slice());
}
