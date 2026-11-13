//! Unit tests of the picture output model. Where a test pins the reference's
//! departure from the specifications, it names the probe stream that shows it
//! (probes are run through FFmpeg's decoders as a black box).

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
        dpb.retain_references(|stored| references.contains(&stored));
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
    // flag (probe p2-hevc-eos-first22 shows the reference doing so).
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
fn ffmpeg_hevc_makes_room_before_the_reference_picture_set_applies() {
    // Probe p2-hevc-dpb2 (sps_max_dec_pic_buffering_minus1 2): before B5 is
    // decoded the buffer holds 0, 3, 6 and 4; holding 4 = 2 + 2 outputs 4 and
    // then 6, which C.5.2.2 alone would not.
    let limits = HevcLimits {
        max_dec_pic_buffering: None,
        ..HevcLimits::from_sps(2, 2, 0)
    };
    // The probe's reference picture sets keep picture 0 longer.
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
        dpb.retain_references(|stored| references.contains(&stored));
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
    let store = |poc, reference| AvcStore {
        poc,
        reference,
        output: true,
        second_field: false,
    };
    for (n, &(poc, reference)) in [(0, true), (6, true), (2, false), (4, false), (12, true)]
        .iter()
        .enumerate()
    {
        let current = AvcPicture {
            idr: n == 0,
            ..AvcPicture::default()
        };
        dpb.avc_remove(current, seen.leave());
        dpb.avc_store(store(poc, reference), n as u32, 2, seen.leave())
            .expect("store");
    }
    // B2 makes room by bumping I0, then comes before every picture waiting
    // and leaves at once, as B4 does; storing P12 bumps P6 (no picture is
    // marked unused here, so P12 is stored beyond the two buffers).
    assert_eq!(seen.output, [0, 2, 3, 1]);
}

#[test]
fn avc_idr_picture_discards_or_outputs_the_prior_pictures() {
    for (flag, output) in [(false, vec![0, 1]), (true, vec![])] {
        let mut dpb = DecodedPictures::new(&pool());
        let mut seen = Seen::default();
        for (n, poc) in [0, 2].into_iter().enumerate() {
            let store = AvcStore {
                poc,
                reference: true,
                output: true,
                second_field: false,
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
fn ffmpeg_h264_drops_a_picture_leaving_after_a_later_one() {
    // Probe p3-h264-pyr-r1-d2: I0 P8 B4 B2 B6 with a reorder depth of 1.
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
    // Probe p3-h264-mmco5-b (5-bit LSBs): P12 with operation 5, then B8 B10
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
    // Probes p13-every3 and p13-every2 (every third or second picture of a
    // stream with 4-bit frame_num, no IDR picture between): after frame_num
    // 15 comes 2. The frames of the gap carry FrameNumOffset past the wrap
    // (36); the reference keeps the offset (4), so the picture leaves after
    // picture 30 and is dropped. 14 to 0 counts 32 either way.
    for (after, expected) in [(AfterGap::Specified, 36), (AfterGap::OffsetKept, 4)] {
        let mut order = AvcOrder::new(AfterMmco5::Specified, after);
        let mut last = 0;
        for (n, frame_num) in [0, 3, 6, 9, 12, 15].into_iter().enumerate() {
            last = order.next(&avc_type2(frame_num, n == 0)).poc;
        }
        assert_eq!(last, 30);
        assert_eq!(order.next(&avc_type2(2, false)).poc, expected);
        let mut order = AvcOrder::new(AfterMmco5::Specified, after);
        for (n, frame_num) in [0, 2, 4, 6, 8, 10, 12, 14].into_iter().enumerate() {
            order.next(&avc_type2(frame_num, n == 0));
        }
        assert_eq!(order.next(&avc_type2(0, false)).poc, 32);
    }
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
