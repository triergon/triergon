//! The DVD receiver (`compat::dvd::Decoder`) on hostile units: extreme
//! offsets and links, and control areas whose sequences read each other's
//! bytes. Work is measured in charged `WorkBudget` units, never in time.

use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvd::{Decoder, Limits, Settings, Status};

/// Decode one packet with unbounded work: whether it presented a subtitle,
/// and the work it was charged.
fn charged(packet: &[u8]) -> (bool, u64) {
    let budget = Budget::new(1 << 26).unwrap();
    let mut decoder = Decoder::new(&budget, Settings::default(), Limits::default());
    let mut work = WorkBudget::new(u64::MAX);
    let shown = match decoder.decode(packet, &mut work).unwrap() {
        Status::Subtitle(_) => true,
        Status::Nothing => false,
        Status::Yield => panic!("unbounded work yielded"),
    };
    (shown, work.spent())
}

/// A unit of `blocks` chained five-byte sequences after a 16-bit header, then
/// `run` zero bytes. Each sequence `06 00 | link | 00` links to the next (the
/// last to itself), and its commands never end: its forced start is followed
/// by the next sequence's bytes, which read as field offsets (`06` and four
/// operand bytes), and so on to the zero run, a forced start per byte.
fn chained(blocks: usize, run: usize) -> Vec<u8> {
    let length = 4 + 5 * blocks + run;
    let mut unit = Vec::with_capacity(length);
    unit.extend((length as u16).to_be_bytes());
    unit.extend(4_u16.to_be_bytes());
    for i in 0..blocks {
        let next = 4 + 5 * (i + 1).min(blocks - 1);
        unit.extend([6, 0]);
        unit.extend((next as u16).to_be_bytes());
        unit.push(0);
    }
    unit.resize(length, 0);
    unit
}

/// The work [`chained`] costs: the unit's bytes, then for every sequence the
/// bytes from its commands to the end of the unit.
fn chained_work(blocks: usize, run: usize) -> u64 {
    let length = 4 + 5 * blocks + run;
    let read: usize = (0..blocks).map(|i| length - (4 + 5 * i + 4)).sum();
    (length + read) as u64
}

#[test]
fn every_sequence_pays_for_the_commands_it_reads() {
    for (blocks, run) in [(2, 0), (1, 100), (40, 0), (40, 300), (500, 4000)] {
        assert_eq!(
            charged(&chained(blocks, run)),
            (false, chained_work(blocks, run)),
            "{blocks} blocks, {run} zero bytes"
        );
    }
}

#[test]
fn charged_work_scales_with_the_work_the_input_demands() {
    let ratio = |small: &[u8], large: &[u8]| charged(large).1 as f64 / charged(small).1 as f64;
    for k in [2_usize, 4, 8] {
        let k_f = k as f64;
        // One sequence then a zero run: each byte is read once by the unit
        // and once by the sequence, so k times the input is k times the work.
        let r = ratio(&chained(1, 2000), &chained(1, 2000 * k));
        assert!(
            (r - k_f).abs() < 0.01 * k_f,
            "k = {k}: linear input charged {r}x"
        );
        // Chained sequences each read to the end of the unit, as the
        // reference's do: k times the blocks is about k squared the work,
        // all of it charged.
        let r = ratio(&chained(400, 0), &chained(400 * k, 0));
        let k2 = k_f * k_f;
        assert!(
            (r - k2).abs() < 0.01 * k2,
            "k = {k}: chained input charged {r}x"
        );
    }
}

/// Decode one packet in slices of `quantum`: the calls it took and the work
/// they were charged in all.
fn sliced(packet: &[u8], quantum: u64) -> (u64, u64) {
    let budget = Budget::new(1 << 26).unwrap();
    let mut decoder = Decoder::new(&budget, Settings::default(), Limits::default());
    let mut work = WorkBudget::new(quantum);
    let mut status = decoder.decode(packet, &mut work).unwrap();
    let (mut calls, mut spent) = (1, work.spent());
    while status == Status::Yield {
        let mut work = WorkBudget::new(quantum);
        status = decoder.resume(&mut work).unwrap();
        calls += 1;
        spent += work.spent();
    }
    (calls, spent)
}

#[test]
fn slices_pay_the_work_of_one_call() {
    for (blocks, run) in [(40, 300), (500, 4000), (2000, 0)] {
        let unit = chained(blocks, run);
        let whole = chained_work(blocks, run);
        for quantum in [1, 7, whole / 100, whole / 3] {
            let (calls, spent) = sliced(&unit, quantum.max(1));
            assert_eq!(spent, whole, "{blocks} blocks, quantum {quantum}");
            assert_eq!(
                calls,
                whole.div_ceil(quantum.max(1)),
                "{blocks} blocks, quantum {quantum}"
            );
        }
    }
}

#[test]
fn the_largest_chained_unit_finishes_in_worker_slices() {
    // Thirteen thousand chained sequences in a 16-bit unit, at the subtitle
    // worker's slice: each call continues where the last one stopped.
    let blocks = 13_000;
    let unit = chained(blocks, 0xffff - 4 - 5 * blocks);
    let whole = chained_work(blocks, 0xffff - 4 - 5 * blocks);
    let (calls, spent) = sliced(&unit, 1 << 16);
    assert_eq!(spent, whole);
    assert_eq!(calls, whole.div_ceil(1 << 16));
}

#[test]
fn quadratic_control_areas_stop_at_the_work_budget() {
    // Thirteen thousand chained sequences fill a 16-bit unit; reading them all
    // would take about 4 * 10^8 units.
    let blocks = 13_000;
    let unit = chained(blocks, 0xffff - 4 - 5 * blocks);
    let budget = Budget::new(1 << 26).unwrap();
    let mut decoder = Decoder::new(&budget, Settings::default(), Limits::default());
    let quantum = 1 << 20;
    let mut work = WorkBudget::new(quantum);
    assert_eq!(decoder.decode(&unit, &mut work).unwrap(), Status::Yield);
    assert_eq!(work.remaining(), 0);
    assert!(chained_work(blocks, 0xffff - 4 - 5 * blocks) > 300 * quantum);
}

#[test]
fn control_offsets_and_links_at_the_top_of_their_range_present_nothing() {
    // A wide unit (zero first word) with a 32-bit control offset of all ones.
    let wide = [0, 0, 0, 0, 0, 10, 0xff, 0xff, 0xff, 0xff];
    assert_eq!(charged(&wide), (false, 10));
    // A 16-bit control offset of all ones, in a unit that is complete by size.
    let narrow = [0, 10, 0xff, 0xff, 0, 0, 0, 0, 0, 0];
    assert_eq!(charged(&narrow), (false, 10));
    // A wide sequence linking to offset 2^32 - 1.
    let mut linked = vec![0, 0, 0, 0, 0, 17, 0, 0, 0, 10];
    linked.extend([0, 0, 0xff, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(charged(&linked), (false, 17 + 1));
}
