//! Caption data taken from video as FFmpeg's decoders attach it to frames.
//!
//! `tests/cc/mpeg2-open-gop.m2v` (authored by `harness/subtitles/cc_streams.py`)
//! starts at an open GOP, so its two leading B pictures are not output; its
//! `.packets` file records the reference's caption packets.
//!
//! The H.264 and HEVC tests rebuild, with small synthetic access units, the
//! structures of probe streams run through the reference (the probes are kept
//! with their outputs outside the repository); each names its probe and the
//! reference's output it pins.

mod nal_streams;

use nal_streams::*;
use std::fs;
use std::path::PathBuf;
use triergon_core::{Budget, WorkBudget};
use triergon_harness_protocol::unhex;
use triergon_subtitles::cc::video::nal::{Codec, Framing};
use triergon_subtitles::compat::cc::{CaptionExtractor, VideoCoding, VideoPacket};

/// One output frame: pts, duration and caption bytes.
type Frame = (Option<i64>, Option<i64>, Vec<u8>);

/// The frames of `packets` (decode order), popped after every packet and
/// after the end.
fn extract<'a>(
    coding: VideoCoding,
    packets: impl IntoIterator<Item = VideoPacket<'a>>,
) -> Vec<Frame> {
    let pool = Budget::new(1 << 24).expect("pool");
    let mut work = WorkBudget::new(1 << 24);
    let mut extractor = CaptionExtractor::new(&pool, coding, (1, 90_000));
    let mut frames = Vec::new();
    let mut collect = |extractor: &mut CaptionExtractor| {
        while let Some(f) = extractor.pop().expect("pop") {
            frames.push((f.pts, f.duration, f.data.to_vec()));
        }
    };
    for packet in packets {
        extractor.push(packet, &mut work).expect("push");
        collect(&mut extractor);
    }
    extractor.finish().expect("finish");
    collect(&mut extractor);
    frames
}

/// A packet without container times beyond `pts`.
fn packet(data: &[u8], pts: Option<i64>) -> VideoPacket<'_> {
    VideoPacket {
        data,
        pts,
        dts: None,
        duration: None,
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/cc")
        .join(name)
}

#[test]
fn raw_mpeg2_frames_match_the_reference() {
    let es = fs::read(fixture("mpeg2-open-gop.m2v")).expect("stream");
    let recorded = fs::read_to_string(fixture("mpeg2-open-gop.packets")).expect("packets");
    let expected: Vec<(Option<i64>, Option<i64>, Vec<u8>)> = recorded
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let pts = (fields[0] != "-").then(|| fields[0].parse().expect("pts"));
            (
                pts,
                Some(fields[1].parse().expect("duration")),
                unhex(fields[2]),
            )
        })
        .collect();
    // Whole, and in pieces that split pictures and start codes.
    assert_eq!(
        extract(VideoCoding::Mpeg2Raw, [packet(&es, None)]),
        expected
    );
    let pieces = es.chunks(97).map(|piece| packet(piece, None));
    assert_eq!(extract(VideoCoding::Mpeg2Raw, pieces), expected);
}

#[test]
fn cea708_constructs_pass_through_extraction_untouched() {
    let es = fs::read(fixture("mpeg2-open-gop.m2v")).expect("stream");
    let frames = extract(VideoCoding::Mpeg2Raw, [packet(&es, None)]);
    // Valid `cc_type` 2 and 3 constructs (DTVCC data and packet start) stay in
    // each frame's caption data, as the reference keeps them.
    let dtvcc = |data: &[u8]| {
        data.chunks_exact(3)
            .filter(|c| c[0] & 4 != 0 && c[0] & 3 >= 2)
            .count()
    };
    assert!(frames.iter().map(|(_, _, data)| dtvcc(data)).sum::<usize>() > 0);
}

/// An MPEG-2 picture of `kind` (1 I, 2 P) with the user data `blocks`
/// before its slice.
fn mpeg2_picture(kind: u8, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut picture = vec![0, 0, 1, 0x00, 0, kind << 3, 0xff, 0xf8];
    for block in blocks {
        picture.extend_from_slice(&[0, 0, 1, 0xb2]);
        picture.extend_from_slice(block);
    }
    picture.extend_from_slice(&[0, 0, 1, 0x01, 0x55, 0x55]);
    picture
}

/// `GA94` user data with `cc_data`.
fn a53(cc_data: &[u8]) -> Vec<u8> {
    let count = u8::try_from(cc_data.len() / 3).expect("count");
    [b"GA94\x03", &[0x40 | count, 0xff][..], cc_data, b"\xff"].concat()
}

/// A 29.97 Hz sequence header.
const MPEG2_SEQUENCE: &[u8] = b"\0\0\x01\xb3\x2d\x01\xe0\x24\xff\xff\xe0\x18";

#[test]
fn mpeg2_frames_keep_at_most_6000_caption_bytes() {
    let padding = |count: usize| a53(&[0xfa, 0, 0].repeat(count));
    let data = |n: u8| a53(&[0xfc, 0x80 + n, 0x80].repeat(10));
    let blocks = |last: usize, n: u8| {
        let mut blocks = vec![padding(31); 64];
        blocks.extend([padding(last), data(n)]);
        blocks
    };
    // The data passes the limit; reaches it exactly; follows padding that
    // would pass it.
    let mut es = MPEG2_SEQUENCE.to_vec();
    es.extend(mpeg2_picture(1, &blocks(7, 1)));
    es.extend(mpeg2_picture(2, &blocks(6, 2)));
    es.extend(mpeg2_picture(2, &blocks(20, 3)));
    let frames = extract(VideoCoding::Mpeg2Raw, [packet(&es, None)]);
    let lengths: Vec<_> = frames.iter().map(|frame| frame.2.len()).collect();
    assert_eq!(lengths, [5973, 6000, 5982]);
    let carries = |frame: &Frame, n: u8| frame.2.ends_with(&[0xfc, 0x80 + n, 0x80]);
    assert!(!carries(&frames[0], 1) && carries(&frames[1], 2) && carries(&frames[2], 3));
}

#[test]
fn mpeg2_scte20_without_pairs_does_not_lock_the_format() {
    let scte20 = vec![0x03, 0x81, 0x00, 0x00];
    let mut es = MPEG2_SEQUENCE.to_vec();
    es.extend(mpeg2_picture(1, &[scte20]));
    es.extend(mpeg2_picture(2, &[a53(&[0xfc, 0x94, 0x2c])]));
    es.extend(mpeg2_picture(2, &[]));
    let frames = extract(VideoCoding::Mpeg2Raw, [packet(&es, None)]);
    let data: Vec<_> = frames.iter().map(|frame| frame.2.clone()).collect();
    assert_eq!(data, [vec![0xfc, 0x94, 0x2c]]);
}

/// Packets in decode order, each with its PTS (90 kHz) and a decode time one
/// frame period (3003) after the last, as in MPEG-TS.
fn ts_packets(units: &[(Vec<u8>, i64)]) -> impl Iterator<Item = VideoPacket<'_>> {
    units
        .iter()
        .enumerate()
        .map(|(n, (data, pts))| VideoPacket {
            data,
            pts: Some(*pts),
            dts: Some(n as i64 * 3003),
            duration: None,
        })
}

/// The picture numbers (caption tags) of the frames, in output order.
fn tags(frames: &[Frame]) -> Vec<Vec<u8>> {
    frames
        .iter()
        .map(|(_, _, data)| data.chunks(3).map(|c| c[1] & 0x7f).collect())
        .collect()
}

/// An H.264 access unit: parameter sets when `params`, caption SEI `n`, a slice.
fn avc_au(sps: Option<&AvcSps>, n: u8, slice: &AvcSlice, poc_type: u32) -> Vec<u8> {
    let mut nals = Vec::new();
    if let Some(sps) = sps {
        nals.push(avc_sps(sps));
        nals.push(avc_pps());
    }
    nals.push(caption_sei(&[0x06], n));
    nals.push(avc_slice(slice, poc_type));
    annex_b(&nals)
}

/// H.264 pictures (slice type, reference, order count LSB) of a closed GOP
/// with two B pictures: I0 P6 B2 B4 P12 B8 B10, as libx264 codes them.
fn avc_gop(frame_num: u32) -> Vec<(u32, bool, u32, u32)> {
    let fn_ = |k| frame_num + k;
    vec![
        (2, true, 0, fn_(0)),
        (0, true, 6, fn_(1)),
        (1, false, 2, fn_(2)),
        (1, false, 4, fn_(2)),
        (0, true, 12, fn_(2)),
        (1, false, 8, fn_(3)),
        (1, false, 10, fn_(3)),
    ]
}

/// Access units of `pictures` (slice type, reference, LSB, frame_num), the
/// first an IDR picture with parameter sets, with PTS from the order count.
fn avc_stream(
    sps: &AvcSps,
    pictures: &[(u32, bool, u32, u32)],
    idr_at: &[usize],
) -> Vec<(Vec<u8>, i64)> {
    pictures
        .iter()
        .enumerate()
        .map(|(n, &(slice_type, reference, lsb, frame_num))| {
            let idr = idr_at.contains(&n);
            let slice = AvcSlice {
                idr,
                reference,
                slice_type,
                frame_num,
                poc_lsb: lsb,
                ..AvcSlice::default()
            };
            let au = avc_au(idr.then_some(sps), n as u8, &slice, sps.poc_type);
            (au, 90_000 + i64::from(lsb) / 2 * 3003)
        })
        .collect()
}

#[test]
fn h264_frames_leave_in_order_count_order_with_their_times() {
    // Probe p1-h264-closed: the reference outputs I0 B2 B4 P6 B8 B10 P12,
    // each lasting the VUI's two ticks of 1001/60000 s.
    let units = avc_stream(&AvcSps::default(), &avc_gop(0), &[0]);
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [[0], [2], [3], [1], [5], [6], [4]]);
    assert!(frames.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(frames.iter().all(|f| f.1 == Some(3003)));
}

#[test]
fn h264_ignores_no_output_of_prior_pics_flag() {
    // Probe p3-h264-closed-noout: the P picture waiting at an IDR picture
    // with no_output_of_prior_pics_flag set is still output.
    let mut pictures = avc_gop(0);
    pictures.extend(
        avc_gop(0)
            .into_iter()
            .map(|(t, r, lsb, f)| (t, r, lsb + 14, f)),
    );
    let sps = AvcSps::default();
    let mut units = avc_stream(&sps, &pictures, &[0, 7]);
    let slice = AvcSlice {
        idr: true,
        reference: true,
        slice_type: 2,
        poc_lsb: 14,
        no_output_of_prior_pics: true,
        ..AvcSlice::default()
    };
    units[7].0 = avc_au(Some(&sps), 7, &slice, 0);
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(frames.len(), 14);
    assert_eq!(tags(&frames)[6], [4]);
}

#[test]
fn h264_drops_a_picture_that_leaves_after_a_later_one() {
    // Probe p3-h264-pyr-r1-d2: a B pyramid (I0 P8 B4 B2 B6) needs a reorder
    // depth of 2 where the bitstream restriction states 1; the reference
    // drops B2, which would leave after B4.
    let pictures = [
        (2, true, 0, 0),
        (0, true, 8, 1),
        (1, true, 4, 2),
        (1, false, 2, 3),
        (1, false, 6, 3),
    ];
    let units = avc_stream(&AvcSps::default(), &pictures, &[0]);
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [[0], [2], [4], [1]]);
}

#[test]
fn h264_streams_starting_at_an_intra_picture_recover_when_it_leaves() {
    // Probes p4-h264-open-norec-from10 and p4-h264-open-from10: from a
    // non-IDR I picture (order count 8) with B4 B6 before it in output order,
    // then P14 B10 B12 P20 B16 B18. Without a recovery point SEI the
    // reference drops B4, B6 and P14, decoded before the I picture leaves;
    // with one (recovery_frame_cnt 0) only B4 and B6, which precede it.
    let pictures = [
        (2, true, 8, 5),
        (1, false, 4, 6),
        (1, false, 6, 6),
        (0, true, 14, 6),
        (1, false, 10, 7),
        (1, false, 12, 7),
        (0, true, 20, 7),
        (1, false, 16, 8),
        (1, false, 18, 8),
    ];
    let sps = AvcSps::default();
    let mut units = avc_stream(&sps, &pictures, &[]);
    let with_sets = |n: u8, recovery: bool| {
        let (t, r, lsb, f) = pictures[usize::from(n)];
        let slice = AvcSlice {
            reference: r,
            slice_type: t,
            frame_num: f,
            poc_lsb: lsb,
            ..AvcSlice::default()
        };
        let mut nals = vec![avc_sps(&sps), avc_pps()];
        if recovery {
            let mut point = Bits::default();
            point.u(6, 8).u(1, 8).ue(0).flag(true).flag(false).u(0, 2);
            nals.push(point.nal(&[0x06]));
        }
        nals.push(caption_sei(&[0x06], n));
        nals.push(avc_slice(&slice, 0));
        annex_b(&nals)
    };
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    units[0].0 = with_sets(0, false);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [[0], [4], [5], [7], [8], [6]]);
    units[0].0 = with_sets(0, true);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [[0], [4], [5], [3], [7], [8], [6]]);
}

#[test]
fn h264_memory_management_operation_5_starts_a_new_order() {
    // Probe p3-h264-mmco5-b: P12 with memory_management_control_operation 5
    // leaves after the pictures before it; B8 and B10 after it follow it,
    // their order counts taken from its coded LSB (12), not 0.
    let pictures = [
        (2, true, 0, 0),
        (0, true, 6, 1),
        (1, false, 2, 2),
        (1, false, 4, 2),
        (0, true, 12, 2),
        (1, false, 8, 1),
        (1, false, 10, 1),
        (0, true, 18, 1),
        (1, false, 14, 2),
        (1, false, 16, 2),
    ];
    let sps = AvcSps::default();
    let mut units = avc_stream(&sps, &pictures, &[0]);
    let slice = AvcSlice {
        reference: true,
        frame_num: 2,
        poc_lsb: 12,
        mmco5: true,
        ..AvcSlice::default()
    };
    units[4].0 = avc_au(None, 4, &slice, 0);
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(
        tags(&frames),
        [[0], [2], [3], [1], [4], [5], [6], [8], [9], [7]]
    );
}

#[test]
fn h264_frame_durations_come_from_the_vui_or_the_first_decode_steps() {
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let pictures: Vec<_> = (0..30).map(|n| (0, true, 0, n % 16)).collect();
    let stream = |sps: &AvcSps| {
        let mut units = avc_stream(sps, &pictures[..1], &[0]);
        units.extend((1..30).map(|n| {
            let slice = AvcSlice {
                reference: true,
                frame_num: n % 16,
                ..AvcSlice::default()
            };
            (avc_au(None, n as u8, &slice, 2), 0)
        }));
        units
    };
    // Probe p9-h264-k45: a VUI frame period of 1/1000 s or less lasts 0.
    let fast = AvcSps {
        poc_type: 2,
        timing: Some((45, 90_000)),
        ..AvcSps::default()
    };
    let frames = extract(coding, ts_packets(&stream(&fast)));
    assert!(frames.iter().all(|f| f.1 == Some(0)));
    // Probes p12-alt46 and p12-first1: without VUI timing, the greatest
    // common divisor of the decode time steps between packets 1 and 20;
    // the first step does not count.
    let untimed = AvcSps {
        poc_type: 2,
        timing: None,
        ..AvcSps::default()
    };
    let units = stream(&untimed);
    let mut dts = 0;
    let packets: Vec<_> = units
        .iter()
        .enumerate()
        .map(|(n, (data, _))| {
            dts += match n {
                0 => 0,
                1 => 3003,
                _ if n % 2 == 0 => 12012,
                _ => 18018,
            };
            VideoPacket {
                data,
                pts: Some(dts),
                dts: Some(dts),
                duration: None,
            }
        })
        .collect();
    let frames = extract(coding, packets);
    assert_eq!(frames.len(), 30);
    assert!(frames.iter().all(|f| f.1 == Some(6006)));
}

/// HEVC pictures (NAL type, order count, reference deltas) of an open GOP
/// with two B pictures, as libx265 codes them: IDR0 P3 B1 B2 P6 B4 B5, then a
/// CRA picture 9 with RASL pictures 7 and 8, P12 B10 B11.
fn hevc_open_gop() -> Vec<(u8, u32, Vec<i32>)> {
    vec![
        (20, 0, vec![]),
        (1, 3, vec![-3]),
        (0, 1, vec![-1, 2]),
        (0, 2, vec![-2, 1]),
        (1, 6, vec![-3, -6]),
        (0, 4, vec![-1, 2]),
        (0, 5, vec![-2, 1]),
        (21, 9, vec![-3]),
        (8, 7, vec![-1, 2]),
        (8, 8, vec![-2, 1]),
        (1, 12, vec![-3]),
        (0, 10, vec![-1, 2]),
        (0, 11, vec![-2, 1]),
    ]
}

fn hevc_stream(pictures: &[(u8, u32, Vec<i32>)]) -> Vec<(Vec<u8>, i64)> {
    pictures
        .iter()
        .enumerate()
        .map(|(n, (kind, poc, deltas))| {
            let irap = (16..=23).contains(kind);
            let slice = hevc_slice(*kind, *poc, deltas, false);
            (
                hevc_au(irap, n as u8, slice),
                90_000 + i64::from(*poc) * 3003,
            )
        })
        .collect()
}

fn with_end_of_sequence(au: &[u8]) -> Vec<u8> {
    [&[0, 0, 1, 0x48, 0x01][..], au].concat()
}

#[test]
fn hevc_cra_after_an_end_of_sequence_discards_the_pictures_waiting() {
    // Probe p2-hevc-eos-first22 (as the hevc-eos-ts case): the CRA picture
    // after an end of sequence NAL unit discards P6, still waiting, with its
    // captions; its RASL pictures are not decoded and pass their captions to
    // P12. Without the end of sequence (probe p1-hevc-open) everything is
    // output.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let mut units = hevc_stream(&hevc_open_gop());
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(
        tags(&frames),
        [
            [0],
            [2],
            [3],
            [1],
            [5],
            [6],
            [4],
            [8],
            [9],
            [7],
            [11],
            [12],
            [10]
        ]
        .map(|t| t.to_vec())
    );
    units[7].0 = with_end_of_sequence(&units[7].0);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(
        tags(&frames),
        [
            vec![0],
            vec![2],
            vec![3],
            vec![1],
            vec![5],
            vec![6],
            vec![7],
            vec![11],
            vec![12],
            vec![8, 9, 10]
        ]
    );
}

#[test]
fn hevc_picture_after_an_end_of_sequence_that_is_not_irap_outputs_those_waiting() {
    // Probe p2-hevc-eos-rasl: an end of sequence NAL unit before a RASL
    // picture of a CRA picture that is not the first: the CRA picture leaves
    // before the RASL pictures are decoded.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let mut units = hevc_stream(&hevc_open_gop());
    units[8].0 = with_end_of_sequence(&units[8].0);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames)[6..10], [vec![4], vec![7], vec![8], vec![9]]);
}

#[test]
fn hevc_pictures_before_the_first_irap_picture_pass_their_captions_on() {
    // Probe p7-hevc-from9: a stream starting at a trailing picture before a
    // CRA picture; the trailing picture and the CRA picture's RASL pictures
    // are not decoded and their captions go to the next picture decoded.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let pictures = hevc_open_gop();
    let mut units = hevc_stream(&pictures[6..]);
    units[0].0 = hevc_au(true, 0, hevc_slice(0, 5, &[-2, 1], false));
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [vec![0, 1], vec![5], vec![6], vec![2, 3, 4]]);
}

#[test]
fn hevc_idr_with_no_output_of_prior_pics_discards_the_pictures_waiting() {
    // Probe p2-hevc-noout-idr: an IDR picture with no_output_of_prior_pics_flag
    // discards the picture still waiting (P6 here); without the flag it is
    // output (probe p2-hevc-eos-idr).
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let mut pictures = hevc_open_gop()[..7].to_vec();
    pictures.extend(hevc_open_gop()[..7].iter().cloned());
    let mut units = hevc_stream(&pictures);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(frames.len(), 14);
    units[7].0 = hevc_au(true, 7, hevc_slice(20, 0, &[], true));
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(frames.len(), 13);
    assert!(!tags(&frames).contains(&vec![4]));
}

#[test]
fn hevc_suffix_sei_captions_are_not_taken() {
    // Corpus case hevc-suffix-ts: the reference takes no captions from
    // suffix SEI NAL units.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let mut au = hevc_au(true, 0, hevc_slice(20, 0, &[], false));
    au.extend(annex_b(&[caption_sei(&[0x50, 0x01], 9)]));
    let frames = extract(coding, ts_packets(&[(au, 0)]));
    assert_eq!(tags(&frames), [[0]]);
}
