//! Unit tests of the picture output model. Where a test pins an observed
//! departure from the specifications, it names the corpus case of
//! `harness/features/subtitle-cc` that shows it.

use super::order::*;
use super::*;

fn pool() -> Budget {
    Budget::new(1 << 20).expect("pool")
}

/// The pictures output (by payload) and those removed without output.
#[derive(Default, Debug)]
struct Seen {
    output: Vec<u32>,
    discarded: Vec<u32>,
}

impl Seen {
    fn leave(&mut self) -> impl FnMut(Leave<'_, u32>) + '_ {
        |leave| match leave {
            Leave::Output { picture, .. } => self.output.push(*picture),
            Leave::Removed {
                picture,
                output: false,
                ..
            } => self.discarded.push(picture),
            Leave::Removed { .. } => {}
        }
    }
}

const LIMITS: HevcLimits = HevcLimits {
    max_dec_pic_buffering: Some(5),
    max_num_reorder: 1,
    max_latency_pictures: None,
};

/// Decodes H.265 pictures (POC, reference POCs) with C.5.2.2 and C.5.2.3;
/// the payload is the decode index.
fn hevc_decode(
    dpb: &mut DecodedPictures<u32>,
    seen: &mut Seen,
    pictures: &[(i32, &[i32])],
    first: u32,
    limits: HevcLimits,
) {
    for (n, &(poc, references)) in pictures.iter().enumerate() {
        dpb.retain_references(|stored, _| references.contains(&stored));
        dpb.hevc_remove(HevcPicture::default(), limits, seen.leave());
        dpb.hevc_store(poc, true, first + n as u32, limits, seen.leave())
            .expect("store");
    }
}

/// IDR0 P3 B1 B2 P6 B4 B5: a GOP with two B pictures as libx265 codes it.
const GOP: [(i32, &[i32]); 7] = [
    (0, &[]),
    (3, &[0]),
    (1, &[0, 3]),
    (2, &[0, 3]),
    (6, &[3, 0]),
    (4, &[3, 6]),
    (5, &[3, 6]),
];

#[test]
fn hevc_pictures_leave_by_reorder_count_in_order_count_order() {
    let mut dpb = DecodedPictures::new(&pool());
    let mut seen = Seen::default();
    hevc_decode(&mut dpb, &mut seen, &GOP, 0, LIMITS);
    assert_eq!(seen.output, [0, 2, 3, 1, 5, 6]);
    dpb.flush(seen.leave());
    assert_eq!(seen.output, [0, 2, 3, 1, 5, 6, 4]);
}

#[test]
fn hevc_cra_with_no_rasl_output_discards_the_pictures_waiting() {
    // C.5.2.2: NoOutputOfPriorPicsFlag is 1 for a CRA picture whatever its
    // flag (case hevc-eos-ts).
    let mut dpb = DecodedPictures::new(&pool());
    let mut seen = Seen::default();
    hevc_decode(&mut dpb, &mut seen, &GOP, 0, LIMITS);
    let cra = HevcPicture {
        irap_no_rasl_output: true,
        cra: true,
        ..HevcPicture::default()
    };
    dpb.mark_unused();
    dpb.hevc_remove(cra, LIMITS, seen.leave());
    assert_eq!(seen.discarded, [4]);
    assert!(dpb.is_empty());
}

#[test]
fn hevc_idr_outputs_the_pictures_waiting_unless_its_flag_says_not_to() {
    for (flag, output, discarded) in [
        (false, vec![0, 2, 3, 1, 5, 6, 4], vec![]),
        (true, vec![0, 2, 3, 1, 5, 6], vec![4]),
    ] {
        let mut dpb = DecodedPictures::new(&pool());
        let mut seen = Seen::default();
        hevc_decode(&mut dpb, &mut seen, &GOP, 0, LIMITS);
        let idr = HevcPicture {
            irap_no_rasl_output: true,
            no_output_of_prior_pics: flag,
            ..HevcPicture::default()
        };
        dpb.mark_unused();
        dpb.hevc_remove(idr, LIMITS, seen.leave());
        assert_eq!(seen.output, output);
        assert_eq!(seen.discarded, discarded);
    }
}

#[test]
fn hevc_latency_count_bumps_a_picture_waiting_too_long() {
    // sps_max_latency_increase_plus1 1 with 3 reorder pictures:
    // SpsMaxLatencyPictures is 3; P6 waits behind B4, B5 and P9.
    let limits = HevcLimits::from_sps(5, 3, 1);
    assert_eq!(limits.max_latency_pictures, Some(3));
    let mut dpb = DecodedPictures::new(&pool());
    let mut seen = Seen::default();
    let pictures: [(i32, &[i32]); 4] = [(6, &[]), (4, &[6]), (5, &[6]), (3, &[6])];
    hevc_decode(&mut dpb, &mut seen, &pictures, 0, limits);
    // Bumping outputs the smallest order counts until P6 has left.
    assert_eq!(seen.output, [3, 1, 2, 0]);
}

#[test]
fn observed_hevc_room_is_made_before_the_reference_picture_set_applies() {
    // Case hevc-dpb-ts (sps_max_dec_pic_buffering_minus1 2): before B5 is
    // decoded the buffer holds 0, 3, 6 and 4; holding 4 = 2 + 2 outputs 4 and
    // then 6, which C.5.2.2 alone would not.
    let limits = HevcLimits {
        max_dec_pic_buffering: None,
        ..HevcLimits::from_sps(2, 2, 0)
    };
    // These reference picture sets keep picture 0 longer.
    let gop: [(i32, &[i32]); 7] = [
        (0, &[]),
        (3, &[0]),
        (1, &[0, 3]),
        (2, &[0, 3]),
        (6, &[3, 0]),
        (4, &[3, 0, 6]),
        (5, &[3, 0, 6]),
    ];
    let mut dpb = DecodedPictures::new(&pool());
    let mut seen = Seen::default();
    for (n, &(poc, references)) in gop.iter().enumerate() {
        dpb.bump_while_holding(2 + 2, seen.leave());
        dpb.retain_references(|stored, _| references.contains(&stored));
        dpb.hevc_remove(HevcPicture::default(), limits, seen.leave());
        dpb.hevc_store(poc, true, n as u32, limits, seen.leave())
            .expect("store");
    }
    dpb.flush(seen.leave());
    assert_eq!(seen.output, [0, 2, 3, 1, 5, 4, 6]);
}

#[test]
fn avc_bumps_when_the_buffer_is_full_and_outputs_a_leading_non_reference_picture_directly() {
    // C.4.5.1/C.4.5.2 with two frame buffers: I0 P6 B2 B4 P12.
    let mut dpb = DecodedPictures::new(&pool());
    let mut seen = Seen::default();
    let store = |poc, reference: bool| AvcStore {
        poc,
        marking: if reference {
            Marking::ShortTerm
        } else {
            Marking::Unused
        },
        output: true,
        field: None,
        hold: false,
    };
    for (n, &(poc, reference)) in [(0, true), (6, true), (2, false), (4, false), (12, true)]
        .iter()
        .enumerate()
    {
        if poc == 12 {
            // The sliding window (8.2.5.3) of two reference frames.
            for mut entry in dpb.entries_mut() {
                if entry.poc() == 0 {
                    entry.set_marking(None, Marking::Unused);
                }
            }
        }
        let current = AvcPicture {
            idr: n == 0,
            ..AvcPicture::default()
        };
        dpb.avc_remove(current, seen.leave());
        dpb.avc_store(store(poc, reference), n as u32, 2, seen.leave())
            .expect("store");
    }
    // B2 makes room by bumping I0, then comes before every picture waiting
    // and leaves at once, as B4 does; I0, output and no longer a reference,
    // leaves its buffer to P12.
    assert_eq!(seen.output, [0, 2, 3]);
    dpb.flush(seen.leave());
    assert_eq!(seen.output, [0, 2, 3, 1, 4]);
}

#[test]
fn avc_idr_picture_discards_or_outputs_the_prior_pictures() {
    for (flag, output) in [(false, vec![0, 1]), (true, vec![])] {
        let mut dpb = DecodedPictures::new(&pool());
        let mut seen = Seen::default();
        for (n, poc) in [0, 2].into_iter().enumerate() {
            let store = AvcStore {
                poc,
                marking: Marking::ShortTerm,
                output: true,
                field: None,
                hold: false,
            };
            dpb.avc_store(store, n as u32, 4, seen.leave())
                .expect("store");
        }
        let idr = AvcPicture {
            idr: true,
            no_output_of_prior_pics: flag,
            ..AvcPicture::default()
        };
        dpb.avc_remove(idr, seen.leave());
        assert_eq!(seen.output, output);
        assert!(dpb.is_empty());
    }
}

#[test]
fn late_output_drops_a_picture_leaving_after_a_later_one() {
    // Case h264-reorder-ts: I0 P8 B4 B2 B6 with a reorder depth of 1.
    let mut late = LateOutput::default();
    let admitted: Vec<_> = [0, 4, 2, 6, 8]
        .into_iter()
        .filter(|&poc| late.admit(poc))
        .collect();
    assert_eq!(admitted, [0, 4, 6, 8]);
    late.restart();
    assert!(late.admit(1));
}

fn avc_type0(lsb: u32, reference: bool, mmco5: bool, idr: bool) -> AvcPocFields<'static> {
    AvcPocFields {
        idr,
        reference,
        frame_num: 0,
        log2_max_frame_num: 4,
        field: None,
        mmco5,
        coding: AvcPocCoding::Lsb {
            lsb,
            log2_max_lsb: 5,
            delta_bottom: 0,
        },
    }
}

#[test]
fn avc_type0_order_counts_wrap_their_lsbs() {
    let mut order = AvcOrder::default();
    let pocs: Vec<_> = [(0, true), (30, true), (2, true), (28, false)]
        .into_iter()
        .enumerate()
        .map(|(n, (lsb, reference))| order.next(&avc_type0(lsb, reference, false, n == 0)).poc)
        .collect();
    assert_eq!(pocs, [0, -2, 2, -4]);
}

#[test]
fn avc_after_memory_management_operation_5() {
    // Case h264-mmco5-ts (here 5-bit LSBs): P12 with operation 5, then B8 B10
    // P18. 8.2.1.1 takes 0 as the previous LSB, so P18 counts -14; the
    // reference takes P12's coded LSB, 12, and P18 counts 18.
    for (after, expected) in [
        (AfterMmco5::Specified, [12, 8, 10, -14]),
        (AfterMmco5::CodedLsb, [12, 8, 10, 18]),
    ] {
        let mut order = AvcOrder::new(after, AfterGap::Specified);
        order.next(&avc_type0(0, true, false, true));
        let mmco5 = order.next(&avc_type0(12, true, true, false));
        assert_eq!(mmco5.stored_poc, 0);
        let rest = [(8, false), (10, false), (18, true)]
            .map(|(lsb, reference)| order.next(&avc_type0(lsb, reference, false, false)).poc);
        assert_eq!([mmco5.poc, rest[0], rest[1], rest[2]], expected);
    }
}

fn avc_type2(frame_num: u32, idr: bool) -> AvcPocFields<'static> {
    AvcPocFields {
        idr,
        reference: true,
        frame_num,
        log2_max_frame_num: 4,
        field: None,
        mmco5: false,
        coding: AvcPocCoding::FrameNum,
    }
}

#[test]
fn avc_type2_counts_across_a_frame_num_gap() {
    // Case h264-gap-ts (every third picture of a stream with 4-bit frame_num,
    // no IDR picture between): after frame_num 15 comes 2. Inferring the
    // gap's frames 0 and 1 (8.2.5.2) carries FrameNumOffset past the wrap
    // once (36); the reference infers none and keeps the offset (4), so the
    // picture leaves after picture 30 and is dropped. 14 to 0 counts 32
    // either way.
    let specified = |frame_nums: &[u32], next: u32| {
        let mut order = AvcOrder::new(AfterMmco5::Specified, AfterGap::Specified);
        for (n, &frame_num) in frame_nums.iter().enumerate() {
            let gap: Vec<_> = order.gap(frame_num, 4).collect();
            for missing in gap {
                order.gap_frame(missing, 4, AvcPocCoding::FrameNum);
            }
            order.next(&avc_type2(frame_num, n == 0));
        }
        let gap: Vec<_> = order.gap(next, 4).collect();
        for &missing in &gap {
            order.gap_frame(missing, 4, AvcPocCoding::FrameNum);
        }
        (gap, order.next(&avc_type2(next, false)).poc)
    };
    let observed = |frame_nums: &[u32], next: u32| {
        let mut order = AvcOrder::new(AfterMmco5::Specified, AfterGap::OffsetKept);
        for (n, &frame_num) in frame_nums.iter().enumerate() {
            order.next(&avc_type2(frame_num, n == 0));
        }
        order.next(&avc_type2(next, false)).poc
    };
    let every3 = [0, 3, 6, 9, 12, 15];
    assert_eq!(specified(&every3, 2), (vec![0, 1], 36));
    assert_eq!(observed(&every3, 2), 4);
    let every2 = [0, 2, 4, 6, 8, 10, 12, 14];
    assert_eq!(specified(&every2, 0), (vec![15], 32));
    assert_eq!(observed(&every2, 0), 32);
}

#[test]
fn avc_frame_num_counts_modulo_max_frame_num_and_as_0_after_operation_5() {
    let mut order = AvcOrder::default();
    order.next(&avc_type2(0, true));
    // frame_num beyond its 4 bits counts modulo 16: 17 is 1.
    assert_eq!(order.next(&avc_type2(17, false)).poc, 2);
    let mmco5 = AvcPocFields {
        mmco5: true,
        ..avc_type2(2, false)
    };
    assert_eq!(order.next(&mmco5).stored_poc, 0);
    // The next frame follows frame_num 0: no gap, no wrap.
    assert_eq!(order.prev_ref_frame_num(), Some(0));
    assert_eq!(order.gap(1, 4).count(), 0);
    assert_eq!(order.next(&avc_type2(1, false)).poc, 2);
}

#[test]
fn avc_type1_follows_the_cycle_of_reference_frame_offsets() {
    // A cycle of one reference frame advancing 6, non-reference pictures 4
    // before the next: I0 P6 B2 B4 as order count type 1 codes them.
    let offsets = [6];
    let fields = |frame_num, reference, idr, delta| AvcPocFields {
        idr,
        reference,
        frame_num,
        log2_max_frame_num: 4,
        field: None,
        mmco5: false,
        coding: AvcPocCoding::Cycle {
            offset_for_non_ref_pic: -4,
            offset_for_top_to_bottom_field: 0,
            offsets_for_ref_frame: &offsets,
            delta: [delta, 0],
        },
    };
    let mut order = AvcOrder::default();
    let pocs = [
        order.next(&fields(0, true, true, 0)).poc,
        order.next(&fields(1, true, false, 0)).poc,
        order.next(&fields(2, false, false, 0)).poc,
        order.next(&fields(2, false, false, 2)).poc,
    ];
    assert_eq!(pocs, [0, 6, 2, 4]);
}

#[test]
fn hevc_order_counts_carry_into_the_msb() {
    let mut order = HevcOrder::default();
    let fields = |lsb, irap| HevcPocFields {
        irap_no_rasl_output: irap,
        lsb,
        log2_max_lsb: 4,
        tid0_anchor: true,
    };
    let pocs = [0, 6, 12, 2, 14].map(|lsb| order.next(fields(lsb, lsb == 0)));
    assert_eq!(pocs, [0, 6, 12, 18, 14]);
}

fn field_store(poc: i32, field: Parity, hold: bool) -> AvcStore {
    AvcStore {
        poc,
        marking: Marking::ShortTerm,
        output: true,
        field: Some(field),
        hold,
    }
}

#[test]
fn avc_second_field_joins_its_own_first_field() {
    let mut dpb: DecodedPictures<Vec<u32>> = DecodedPictures::new(&pool());
    let mut leave = |_: Leave<'_, Vec<u32>>| {};
    let first = dpb
        .avc_store(field_store(4, Parity::Top, true), vec![1], 4, &mut leave)
        .expect("store")
        .expect("stored");
    let other = dpb
        .avc_store(field_store(8, Parity::Top, true), vec![2], 4, &mut leave)
        .expect("store")
        .expect("stored");
    // A held first field is kept out of bumping.
    let mut output = Vec::new();
    assert!(!dpb.bump(|l| {
        if let Leave::Output { picture, .. } = l {
            output.push(picture.clone());
        }
    }));
    dpb.avc_store_second_field(
        first,
        field_store(5, Parity::Bottom, false),
        vec![3],
        &mut leave,
        Extend::extend,
    )
    .expect("joined");
    let entry = dpb.entry(first).expect("first");
    assert_eq!(entry.picture(), &vec![1, 3]);
    assert_eq!(entry.marking(Parity::Bottom), Marking::ShortTerm);
    assert!(entry.is_waiting() && !entry.is_held());
    assert!(dpb.entry(other).expect("other").is_held());
    assert!(dpb.bump(|l| {
        if let Leave::Output { picture, .. } = l {
            output.push(picture.clone());
        }
    }));
    assert_eq!(output, [vec![1, 3]]);
    // An unpaired first field the caller releases is bumped like a frame.
    dpb.entry(other).expect("other").release();
    assert!(dpb.bump(|l| {
        if let Leave::Output { picture, .. } = l {
            output.push(picture.clone());
        }
    }));
    assert_eq!(output, [vec![1, 3], vec![2]]);
    // A second field for no stored first field is given back.
    let missing = PictureId(99);
    assert_eq!(
        dpb.avc_store_second_field(
            missing,
            field_store(9, Parity::Bottom, false),
            vec![4],
            &mut leave,
            |_, _| {}
        ),
        Err(vec![4])
    );
}

#[test]
fn avc_second_field_of_a_frame_already_output_is_output_on_its_own() {
    let mut dpb: DecodedPictures<Vec<u32>> = DecodedPictures::new(&pool());
    let mut output = Vec::new();
    let first = dpb
        .avc_store(field_store(4, Parity::Top, false), vec![1], 4, |_| {})
        .expect("store")
        .expect("stored");
    assert!(dpb.bump(|_| {}));
    dpb.avc_store_second_field(
        first,
        field_store(5, Parity::Bottom, false),
        vec![2],
        |l| {
            if let Leave::Output { poc, picture } = l {
                output.push((poc, picture.clone()));
            }
        },
        Extend::extend,
    )
    .expect("joined");
    assert_eq!(output, [(5, vec![2])]);
    let entry = dpb.entry(first).expect("first");
    assert!(!entry.is_waiting() && entry.was_output());
}

#[test]
fn avc_store_reports_a_buffer_full_of_references() {
    let mut dpb = DecodedPictures::new(&pool());
    let reference = |poc| AvcStore {
        poc,
        marking: Marking::ShortTerm,
        output: false,
        field: None,
        hold: false,
    };
    for n in 0..2 {
        dpb.avc_store(reference(n), n as u32, 2, |_| {})
            .expect("store");
    }
    assert!(matches!(
        dpb.avc_store(reference(2), 2, 2, |_| {}),
        Err(StoreError::Full(2))
    ));
    assert!(matches!(
        dpb.avc_store_gap_frame(3, 3, 2, |_| {}),
        Err(StoreError::Full(3))
    ));
    // Marking one unused makes room.
    for mut entry in dpb.entries_mut() {
        if entry.poc() == 0 {
            entry.set_marking(None, Marking::Unused);
        }
    }
    dpb.remove_unused(|_| {});
    let id = dpb.avc_store_gap_frame(3, 3, 2, |_| {}).expect("gap frame");
    let entry = dpb.entry(id).expect("stored");
    assert!(entry.is_reference() && !entry.is_waiting());
}

#[test]
fn a_store_the_pool_refuses_gives_the_picture_back() {
    let pool = Budget::new(64).expect("pool");
    let mut dpb = DecodedPictures::new(&pool);
    let new = NewPicture {
        poc: 0,
        output: true,
        marking: Marking::Unused,
        field: None,
        hold: false,
    };
    let refused = dpb.store(new, [7u8; 256]).expect_err("refused");
    assert_eq!(refused.picture, [7u8; 256]);
}

#[test]
fn take_removes_a_picture_without_output() {
    let mut dpb = DecodedPictures::new(&pool());
    let new = |poc| NewPicture {
        poc,
        output: true,
        marking: Marking::Unused,
        field: None,
        hold: false,
    };
    let first = dpb.store(new(0), 'a').unwrap();
    dpb.store(new(2), 'b').unwrap();
    assert_eq!(dpb.take(first), Some('a'));
    assert_eq!(dpb.take(first), None);
    let mut out = Vec::new();
    dpb.flush(|leave| {
        if let Leave::Output { poc, .. } = leave {
            out.push(poc);
        }
    });
    assert_eq!(out, [2]);
}
