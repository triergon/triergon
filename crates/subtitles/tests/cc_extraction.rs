//! Caption data taken from video as FFmpeg's decoders attach it to frames.
//!
//! `tests/cc/mpeg2-open-gop.m2v` (authored by `harness/subtitles/cc_streams.py`)
//! starts at an open GOP, so its two leading B pictures are not output; its
//! `.packets` file records the reference's caption packets.
//!
//! The H.264 and HEVC tests rebuild, with small synthetic access units, the
//! structures of the corpus cases of `harness/features/subtitle-cc`
//! (`harness/subtitles/cc_streams.py` authors them); each names its case and
//! the reference's output it pins.

mod nal_streams;

use nal_streams::*;
use std::fs;
use std::path::PathBuf;
use triergon_core::{Budget, WorkBudget};
use triergon_harness_protocol::unhex;
use triergon_subtitles::cc::video::nal::{Codec, Framing};
use triergon_subtitles::compat::cc::{CaptionExtractor, CaptionFrame, VideoCoding, VideoPacket};

/// One output frame: pts, duration and caption bytes.
type Frame = (Option<i64>, Option<i64>, Vec<u8>);

/// The frames of `packets` (decode order), popped after every packet and
/// after the end.
fn extract<'a>(
    coding: VideoCoding,
    packets: impl IntoIterator<Item = VideoPacket<'a>>,
) -> Vec<Frame> {
    extract_configured(coding, &[], packets)
}

/// [`extract`] with the parameter sets of a decoder configuration record
/// taken first.
fn extract_configured<'a>(
    coding: VideoCoding,
    record: &[u8],
    packets: impl IntoIterator<Item = VideoPacket<'a>>,
) -> Vec<Frame> {
    let pool = Budget::new(1 << 24).expect("pool");
    let mut work = WorkBudget::new(1 << 24);
    let mut extractor = CaptionExtractor::new(&pool, coding, (1, 90_000));
    extractor.configure(record, &mut work).expect("configure");
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

/// The NAL units of an H.264 access unit: the parameter sets of `sps` when
/// given, caption SEI `n`, a slice.
fn avc_nals(sps: Option<&AvcSps>, n: u8, slice: &AvcSlice, poc_type: u32) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    if let Some(sps) = sps {
        nals.push(avc_sps(sps));
        nals.push(avc_pps());
    }
    nals.push(caption_sei(&[0x06], n));
    nals.push(avc_slice(slice, poc_type));
    nals
}

/// An H.264 access unit in start code framing.
fn avc_au(sps: Option<&AvcSps>, n: u8, slice: &AvcSlice, poc_type: u32) -> Vec<u8> {
    annex_b(&avc_nals(sps, n, slice, poc_type))
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
    // Case h264-a53-ts: the reference outputs I0 B2 B4 P6 B8 B10 P12,
    // each lasting the VUI's two ticks of 1001/60000 s.
    let units = avc_stream(&AvcSps::default(), &avc_gop(0), &[0]);
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [[0], [2], [3], [1], [5], [6], [4]]);
    assert!(frames.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(frames.iter().all(|f| f.1 == Some(3003)));
}

/// An SEI NAL unit (`header`) of messages given as (payload type, payload).
fn sei_of(header: &[u8], messages: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut bits = Bits::default();
    for (kind, payload) in messages {
        bits.u(u64::from(*kind), 8).u(payload.len() as u64, 8);
        for &byte in payload {
            bits.u(u64::from(byte), 8);
        }
    }
    bits.nal(header)
}

/// A `user_data_registered_itu_t_t35` payload: country and provider code,
/// then `ATSC_user_data()` with `cc_data()` of `flags` (A/53 Part 4: bit 6
/// `process_cc_data_flag`, bits 0-4 `cc_count`) and its constructs.
fn t35(country_provider: [u8; 3], flags: u8, constructs: &[u8]) -> Vec<u8> {
    [
        &country_provider[..],
        b"GA94\x03",
        &[flags, 0xff],
        constructs,
        &[0xff],
    ]
    .concat()
}

/// SEI messages around picture `n`'s captions: an ATSC message with
/// `tag(n)`, then messages whose constructs are not caption data (T.35 of
/// DirecTV, of another country, ATSC with `process_cc_data_flag` 0, and
/// `user_data_unregistered`), then a second ATSC message with
/// `FC 80|n 81`.
fn mixed_messages(n: u8) -> Vec<(u8, Vec<u8>)> {
    const ATSC: [u8; 3] = [0xb5, 0x00, 0x31];
    let ignored = [0xfc, 0xff, 0xff];
    vec![
        (4, t35(ATSC, 0x41, &tag(n))),
        (4, t35([0xb5, 0x00, 0x2f], 0x41, &ignored)),
        (4, t35([0x26, 0x00, 0x31], 0x41, &ignored)),
        (4, t35(ATSC, 0x01, &ignored)),
        (
            5,
            [&[0x47; 16][..], &t35(ATSC, 0x41, &ignored)[3..]].concat(),
        ),
        (4, t35(ATSC, 0x41, &[0xfc, 0x80 | n, 0x81])),
    ]
}

#[test]
fn sei_captions_are_the_processed_atsc_t35_constructs_in_presentation_order() {
    // ITU-T T.35 names the United States 0xB5 and A/53 Part 4 the ATSC
    // provider 0x0031; H.264 D.1.6 and H.265 D.2.6 carry T.35 data as SEI
    // payload type 4. Only ATSC cc_data with process_cc_data_flag 1 is
    // caption data; payload type 5 (user_data_unregistered) is not. Picture
    // 3 carries only the others and gives no frame.
    let sps = AvcSps::default();
    let units: Vec<_> = avc_gop(0)
        .into_iter()
        .enumerate()
        .map(|(n, (slice_type, reference, lsb, frame_num))| {
            let n = n as u8;
            let slice = AvcSlice {
                idr: n == 0,
                reference,
                slice_type,
                frame_num,
                poc_lsb: lsb,
                ..AvcSlice::default()
            };
            let mut messages = mixed_messages(n);
            if n == 3 {
                messages.retain(|(_, payload)| payload.ends_with(&[0xfc, 0xff, 0xff, 0xff]));
            }
            let mut nals = Vec::new();
            if n == 0 {
                nals.extend([avc_sps(&sps), avc_pps()]);
            }
            nals.extend([sei_of(&[0x06], &messages), avc_slice(&slice, 0)]);
            (annex_b(&nals), 90_000 + i64::from(lsb) / 2 * 3003)
        })
        .collect();
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    let expected: Vec<Vec<u8>> = [0, 2, 1, 5, 6, 4]
        .into_iter()
        .map(|n| [tag(n), [0xfc, 0x80 | n, 0x81]].concat())
        .collect();
    let data: Vec<_> = frames.into_iter().map(|(_, _, data)| data).collect();
    assert_eq!(data, expected);
    // The same in an HEVC prefix SEI NAL unit.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let au = annex_b(&[
        hevc_sps(4, 1, 0),
        hevc_pps(),
        sei_of(&[0x4e, 0x01], &mixed_messages(7)),
        hevc_slice(19, 0, &[], false),
    ]);
    let frames = extract(coding, ts_packets(&[(au, 0)]));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].2, [tag(7), [0xfc, 0x87, 0x81]].concat());
}

#[test]
fn h264_ignores_no_output_of_prior_pics_flag() {
    // Case h264-noout-ts: the P picture waiting at an IDR picture
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
    // Case h264-reorder-ts: a B pyramid (I0 P8 B4 B2 B6) needs a reorder
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
    // Cases h264-norecovery-ts and h264-recovery-ts: from a
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
    // Case h264-mmco5-ts: P12 with memory_management_control_operation 5
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
    // Case h264-vui-fast-ts: a VUI frame period of 1/1000 s or less lasts 0.
    let fast = AvcSps {
        poc_type: 2,
        timing: Some((45, 90_000)),
        ..AvcSps::default()
    };
    let frames = extract(coding, ts_packets(&stream(&fast)));
    assert!(frames.iter().all(|f| f.1 == Some(0)));
    // Case h264-estimate-ts: without VUI timing, a common divisor of the
    // decode time steps from packet 3 to 20 longer than 1/500 s.
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

/// The durations of 40 H.264 P frames without VUI timing or bitstream
/// restriction, order counts 2 apart (4 from the IDR picture with `jump`),
/// at the (PTS, DTS) `times` gives.
fn estimated(jump: bool, times: impl Fn(i64) -> (i64, Option<i64>)) -> Vec<Option<i64>> {
    let sps = AvcSps {
        reorder: None,
        timing: None,
        ..AvcSps::default()
    };
    let pictures: Vec<_> = (0..40u32)
        .map(|n| match n {
            0 => (2, true, 0, 0),
            _ => (0, true, (2 * n + u32::from(jump) * 2) % 256, n % 16),
        })
        .collect();
    let units = avc_stream(&sps, &pictures, &[0]);
    let packets = units.iter().enumerate().map(|(n, (data, _))| {
        let (pts, dts) = times(n as i64);
        VideoPacket {
            data,
            pts: Some(pts),
            dts,
            duration: None,
        }
    });
    let frames = extract(VideoCoding::Nal(Codec::H264, Framing::StartCodes), packets);
    assert_eq!(frames.len(), 40);
    frames.iter().map(|f| f.1).collect()
}

#[test]
fn h264_durations_without_vui_timing_fit_a_frame_rate() {
    // Cases h264-rate-twelfths-ts and h264-rate-multiple-ts: one tick added
    // on odd frames, so that the steps share no divisor.
    let at =
        |p: i64, q: i64| move |n: i64| (90_000 + (2 * n * 90_000 * q + p) / (2 * p) + n % 2, None);
    assert!(
        estimated(false, at(359, 12))
            .iter()
            .all(|&d| d == Some(3008))
    );
    assert!(
        estimated(false, at(11, 12))
            .iter()
            .all(|&d| d == Some(49_090))
    );
    // Case h264-rate-jump-ts: after the order count jump at frame 1, frames
    // 2-6 give no time, so frame 2 moved 1501 later does not count; with
    // DTS other than the PTS it does (probe p84).
    let moved = |n: i64| 90_000 + 3003 * n + n % 2 + if n == 2 { 1501 } else { 0 };
    assert!(
        estimated(true, |n| (moved(n), None))
            .iter()
            .all(|&d| d == Some(3003))
    );
    assert!(
        estimated(true, |n| (moved(n), Some(moved(n))))
            .iter()
            .all(|&d| d == Some(3003))
    );
    assert!(
        estimated(false, |n| (moved(n), None))
            .iter()
            .all(|&d| d == Some(1501))
    );
    assert!(
        estimated(true, |n| (moved(n) + 6006, Some(moved(n))))
            .iter()
            .all(|&d| d == Some(1501))
    );
}

/// The durations of 300 H.264 pictures without VUI timing or bitstream
/// restriction: P frames with order counts 2 apart (4 from the frame before
/// at the indices in `jumps`), and at the indices in `bs` a non-reference B
/// picture that many P frames below the last; `framing` and `time` (the
/// PTS of a decode index and order count) as given.
fn durations(
    bs: &[(u32, u32)],
    jumps: &[u32],
    framing: Framing,
    time: impl Fn(i64, u32) -> i64,
) -> Vec<Option<i64>> {
    let sps = AvcSps {
        reorder: None,
        timing: None,
        ..AvcSps::default()
    };
    let (mut pocs, mut frame_num, mut units) = (vec![0u32], 0u32, Vec::new());
    for n in 0..300u32 {
        let slice = if n == 0 {
            AvcSlice {
                idr: true,
                reference: true,
                slice_type: 2,
                ..AvcSlice::default()
            }
        } else if let Some(&(_, below)) = bs.iter().find(|&&(at, _)| at == n) {
            let last = pocs[pocs.len() - below as usize];
            AvcSlice {
                slice_type: 1,
                frame_num: (frame_num + 1) % 16,
                poc_lsb: if below == 0 { last + 1 } else { last - 1 } % 256,
                ..AvcSlice::default()
            }
        } else {
            frame_num += 1;
            let poc = pocs[pocs.len() - 1] + if jumps.contains(&n) { 4 } else { 2 };
            pocs.push(poc);
            AvcSlice {
                reference: true,
                frame_num: frame_num % 16,
                poc_lsb: poc % 256,
                ..AvcSlice::default()
            }
        };
        let nals = avc_nals((n == 0).then_some(&sps), n as u8, &slice, 0);
        let data = match framing {
            Framing::StartCodes => nals
                .iter()
                .flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat())
                .collect(),
            Framing::Lengths(_) => length_prefixed(&nals),
        };
        units.push((data, time(i64::from(n), slice.poc_lsb)));
    }
    let packets = units.iter().map(|(data, pts)| VideoPacket {
        data,
        pts: Some(*pts),
        dts: None,
        duration: None,
    });
    let frames = extract(VideoCoding::Nal(Codec::H264, framing), packets);
    frames.iter().map(|f| f.1).collect()
}

#[test]
fn h264_pts_only_packets_give_no_time_while_the_start_hold_waits() {
    // Cases h264-rate-pts-b-ts and h264-rate-pts-run-ts (probe p99): 29.97
    // fps, a tick added on odd packets, one packet moved half a frame. A B
    // picture one below at 2 raises the hold's depth to 1: packets 3 to 7
    // give no time, so packet 4 moved leaves the estimate at 3003, packet 8
    // moved does not. After a step at 1, packets 2 to 6 give none but for the
    // B picture at 4, which gives its own time.
    let at = |moved: i64| {
        move |n: i64, _| {
            90_000
                + (2 * n * 90_000 * 1001 + 30_000) / 60_000
                + n % 2
                + if n == moved { 1501 } else { 0 }
        }
    };
    let all = |d: Vec<Option<i64>>, want: i64| d.iter().all(|&x| x == Some(want));
    let start = Framing::StartCodes;
    assert!(all(durations(&[(2, 1)], &[], start, at(4)), 3003));
    assert!(!all(durations(&[(2, 1)], &[], start, at(8)), 3003));
    assert!(all(durations(&[(4, 1)], &[1], start, at(6)), 3003));
    assert!(!all(durations(&[(4, 1)], &[1], start, at(4)), 3003));
}

#[test]
fn h264_matroska_frames_last_0_until_the_presentation_times_settle() {
    // Cases h264-nodur-mkv and h264-nodur-fine-mkv (probes p96, p97):
    // length-prefixed packets with presentation times only. The first
    // picture leaves at index 6; at 0.1 ms per tick the estimate takes the
    // 21 times after it, a first B picture displaced at 10 waits two packets
    // more: 30 frames last 0, the rest one frame of 25 fps (the B picture,
    // half a frame off, sorts before the fourth time and leaves the divisor).
    let shown = |_, poc: u32| 10_000 + i64::from(poc) * 200;
    let d = durations(&[(10, 1)], &[], Framing::Lengths(4), shown);
    let lead = d.iter().take_while(|&&x| x == Some(0)).count();
    assert_eq!((lead, d[lead]), (30, Some(400)));
    // Without B pictures, 28 frames last 0.
    let plain = durations(&[], &[], Framing::Lengths(4), shown);
    assert_eq!(plain.iter().take_while(|&&x| x == Some(0)).count(), 28);
    assert!(plain[28..].iter().all(|&x| x == Some(400)));
}

#[test]
fn h264_sequences_reordering_more_than_16_frames_are_not_taken() {
    // The reference takes a bitstream restriction's max_num_reorder_frames
    // up to 16, above max_dec_frame_buffering too, and outputs nothing for a
    // sequence stating 17.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    for (reorder, count) in [(16, 7), (17, 0)] {
        let sps = AvcSps {
            reorder: Some(reorder),
            ..AvcSps::default()
        };
        let units = avc_stream(&sps, &avc_gop(0), &[0]);
        assert_eq!(extract(coding, ts_packets(&units)).len(), count);
    }
}

/// A B pyramid: I0 P8 B4 B2 B6, then P16 B12 B10 B14.
const AVC_PYRAMID: [(u32, bool, u32, u32); 9] = [
    (2, true, 0, 0),
    (0, true, 8, 1),
    (1, true, 4, 2),
    (1, false, 2, 3),
    (1, false, 6, 3),
    (0, true, 16, 3),
    (1, true, 12, 4),
    (1, false, 10, 5),
    (1, false, 14, 5),
];

#[test]
fn h264_reorder_depth_without_a_bitstream_restriction() {
    // Probe p57: without a bitstream restriction nothing leaves before the
    // seventh picture, and B2's displacement (2) becomes the depth: the
    // pyramid leaves in full.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: None,
        max_ref_frames: 3,
        ..AvcSps::default()
    };
    let units = avc_stream(&sps, &AVC_PYRAMID[..5], &[0]);
    assert_eq!(
        tags(&extract(coding, ts_packets(&units))),
        [[0], [3], [2], [4], [1]]
    );
}

#[test]
fn h264_depth_grows_to_the_displacement_of_a_picture() {
    // Probe p57: without a bitstream restriction nothing leaves before the
    // seventh picture, so B2 (two pictures above it) is not late; the depth
    // becomes 2 and the second pyramid leaves in full.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: None,
        max_ref_frames: 2,
        ..AvcSps::default()
    };
    let units = avc_stream(&sps, &AVC_PYRAMID, &[0]);
    assert_eq!(
        tags(&extract(coding, ts_packets(&units))),
        [[0], [3], [2], [4], [1], [7], [6], [8], [5]]
    );
}

/// P frames (order counts 0, 2, 4, ...; no bitstream restriction)
/// with one B picture at decode index `at`, `above` P frames above it.
fn avc_late_b(at: u32, above: u32, count: u32) -> Vec<(Vec<u8>, i64)> {
    let below = at - 1 - above;
    let mut pictures = vec![(2, true, 0, 0)];
    let mut frame = 0;
    for n in 1..count {
        if n == at {
            pictures.push((1, false, 2 * below + 1, frame + 1));
        } else {
            frame += 1;
            pictures.push((0, true, 2 * frame, frame));
        }
    }
    let sps = AvcSps {
        reorder: None,
        ..AvcSps::default()
    };
    avc_stream(&sps, &pictures, &[0])
}

#[test]
fn h264_start_of_a_stream_holds_its_pictures() {
    // Probes p27 and p47: without a bitstream restriction nothing leaves
    // before the seventh picture; until picture 20 one picture waits; then
    // the depth (0 here) applies.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let kept = |at: u32, above: u32| {
        let frames = extract(coding, ts_packets(&avc_late_b(at, above, 40)));
        tags(&frames).iter().any(|tags| tags == &[at as u8])
    };
    assert!(kept(6, 5));
    assert!(!kept(7, 2));
    assert!(kept(12, 1));
    assert!(!kept(12, 2));
    assert!(!kept(24, 1));
}

#[test]
fn h264_picture_far_below_the_last_decoded_leaves_at_once() {
    // Probes p22, p23 and p87; case h264-far-back-ts: a picture below the
    // last 16 pictures decoded is output after those waiting, not dropped;
    // one above the 16th is late.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    for (above, kept) in [(16, true), (15, false)] {
        let frames = extract(coding, ts_packets(&avc_late_b(30, above, 40)));
        let order = tags(&frames);
        assert_eq!(order.iter().any(|tags| tags == &[30]), kept);
        if kept {
            let at = order.iter().position(|tags| tags == &[30]).expect("kept");
            assert_eq!(order[at - 1], [29]);
        }
    }
}

#[test]
fn h264_stream_starting_at_an_intra_picture_with_memory_management_operation_5() {
    // Case h264-mmco5-start-ts: from a non-IDR I picture with
    // memory_management_control_operation 5 (its order count LSB 8 below
    // the wrap), B1 and B2 before it in output order, then P3 B4 B5 P6 B7
    // B8 P9 B10 B11. Without a recovery point SEI the reference drops B1
    // only; with one (recovery_frame_cnt 0) it outputs every picture.
    let pictures = [
        (2, true, 248, 4),
        (1, false, 244, 1),
        (1, false, 246, 1),
        (0, true, 254, 1),
        (1, false, 250, 2),
        (1, false, 252, 2),
        (0, true, 4, 2),
        (1, false, 0, 3),
        (1, false, 2, 3),
        (0, true, 10, 3),
        (1, false, 6, 4),
        (1, false, 8, 4),
    ];
    let sps = AvcSps::default();
    let mut units = avc_stream(&sps, &pictures, &[]);
    let first = |recovery: bool| {
        let slice = AvcSlice {
            reference: true,
            slice_type: 2,
            frame_num: 4,
            poc_lsb: 248,
            mmco5: true,
            ..AvcSlice::default()
        };
        let mut nals = vec![avc_sps(&sps), avc_pps()];
        if recovery {
            let mut point = Bits::default();
            point.u(6, 8).u(1, 8).ue(0).flag(true).flag(false).u(0, 2);
            nals.push(point.nal(&[0x06]));
        }
        nals.push(caption_sei(&[0x06], 0));
        nals.push(avc_slice(&slice, 0));
        annex_b(&nals)
    };
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    units[0].0 = first(false);
    assert_eq!(
        tags(&extract(coding, ts_packets(&units))),
        [[0], [2], [4], [5], [3], [7], [8], [6], [10], [11], [9]]
    );
    units[0].0 = first(true);
    assert_eq!(
        tags(&extract(coding, ts_packets(&units))),
        [[0], [1], [2], [4], [5], [3], [7], [8], [6], [10], [11], [9]]
    );
}

#[test]
fn h264_field_pairs_leave_as_one_frame() {
    // Case h264-fields-ts: field pairs I0 P3 B1 B2 (frames in display order;
    // each field with its own caption). The reference outputs one frame per
    // pair with both fields' captions in decode order, the first field's
    // time and the VUI's one tick (a field period, 1501), from fields in
    // separate packets or a pair in one packet alike.
    let sps = AvcSps {
        frame_mbs_only: false,
        reorder: Some(2),
        max_ref_frames: 4,
        ..AvcSps::default()
    };
    // Slice type, reference, display index, frame_num.
    let frames = [
        (2, true, 0, 0),
        (0, true, 3, 1),
        (1, false, 1, 2),
        (1, false, 2, 2),
    ];
    let fields: Vec<(Vec<Vec<u8>>, i64)> = frames
        .iter()
        .enumerate()
        .flat_map(|(k, &(slice_type, reference, display, frame_num))| {
            [false, true].map(|bottom| {
                let idr = k == 0 && !bottom;
                let slice = AvcSlice {
                    idr,
                    reference,
                    slice_type,
                    frame_num,
                    poc_lsb: 4 * display + u32::from(bottom),
                    field: Some(bottom),
                    ..AvcSlice::default()
                };
                let n = (2 * k + usize::from(bottom)) as u8;
                let pts = 90_000 + i64::from(display) * 3003 + i64::from(bottom) * 1501;
                (avc_nals(idr.then_some(&sps), n, &slice, 0), pts)
            })
        })
        .collect();
    let expected_tags = [[0, 1], [4, 5], [6, 7], [2, 3]];
    let expected_pts = [90_000, 93_003, 96_006, 99_009].map(Some);
    let check = |frames: Vec<Frame>| {
        assert_eq!(tags(&frames), expected_tags);
        assert_eq!(frames.iter().map(|f| f.0).collect::<Vec<_>>(), expected_pts);
        assert!(frames.iter().all(|f| f.1 == Some(1501)));
    };
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let separate: Vec<_> = fields
        .iter()
        .map(|(nals, pts)| (annex_b(nals), *pts))
        .collect();
    let packets = separate
        .iter()
        .enumerate()
        .map(|(n, (data, pts))| VideoPacket {
            data,
            pts: Some(*pts),
            dts: Some(n as i64 * 1501),
            duration: None,
        });
    check(extract(coding, packets));
    let paired: Vec<_> = fields
        .chunks(2)
        .map(|pair| {
            (
                annex_b(&[pair[0].0.clone(), pair[1].0.clone()].concat()),
                pair[0].1,
            )
        })
        .collect();
    check(extract(coding, ts_packets(&paired)));
}

#[test]
fn h264_order_count_type_1() {
    // I0 P6 B2 B4 P12 coded with a cycle of one reference frame advancing
    // 6 and non-reference pictures 4 before the next (8.2.1.2).
    let sps = AvcSps {
        poc_type: 1,
        cycle: vec![6],
        offset_for_non_ref_pic: -4,
        ..AvcSps::default()
    };
    // Slice type, reference, frame_num, delta_pic_order_cnt[ 0 ], order count.
    let pictures = [
        (2, true, 0, 0, 0),
        (0, true, 1, 0, 6),
        (1, false, 2, 0, 2),
        (1, false, 2, 2, 4),
        (0, true, 2, 0, 12),
    ];
    let units: Vec<_> = pictures
        .iter()
        .enumerate()
        .map(|(n, &(slice_type, reference, frame_num, delta, poc))| {
            let slice = AvcSlice {
                idr: n == 0,
                reference,
                slice_type,
                frame_num,
                delta,
                ..AvcSlice::default()
            };
            let au = avc_au((n == 0).then_some(&sps), n as u8, &slice, 1);
            (au, 90_000 + i64::from(poc) / 2 * 3003)
        })
        .collect();
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let frames = extract(coding, ts_packets(&units));
    assert_eq!(tags(&frames), [[0], [2], [3], [1], [4]]);
    assert!(frames.windows(2).all(|w| w[0].0 < w[1].0));
}

#[test]
fn h264_length_prefixed_nal_units_give_the_same_frames() {
    let sps = AvcSps::default();
    let units = avc_stream(&sps, &avc_gop(0), &[0]);
    let prefixed: Vec<_> = avc_gop(0)
        .iter()
        .enumerate()
        .map(|(n, &(slice_type, reference, lsb, frame_num))| {
            let slice = AvcSlice {
                idr: n == 0,
                reference,
                slice_type,
                frame_num,
                poc_lsb: lsb,
                ..AvcSlice::default()
            };
            let nals = avc_nals((n == 0).then_some(&sps), n as u8, &slice, 0);
            (length_prefixed(&nals), units[n].1)
        })
        .collect();
    assert_eq!(
        extract(
            VideoCoding::Nal(Codec::H264, Framing::Lengths(4)),
            ts_packets(&prefixed)
        ),
        extract(
            VideoCoding::Nal(Codec::H264, Framing::StartCodes),
            ts_packets(&units)
        )
    );
}

/// An `avcC` record (ISO/IEC 14496-15 §5.3.3.1) with 4-byte lengths, one SPS
/// and one PPS.
fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let list = |nal: &[u8]| [&(nal.len() as u16).to_be_bytes()[..], nal].concat();
    [
        &[1, 0x64, 0, 0x1e, 0xff, 0xe1][..],
        &list(sps),
        &[1],
        &list(pps),
    ]
    .concat()
}

/// An `hvcC` record (ISO/IEC 14496-15 §8.3.3.1) with 4-byte lengths and one
/// array of each NAL unit given.
fn hvcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut record = vec![1, 1, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x5d];
    record.extend([0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f]);
    record.push(nals.len() as u8);
    for nal in nals {
        record.extend([0x80 | (nal[0] >> 1), 0, 1]);
        record.extend((nal.len() as u16).to_be_bytes());
        record.extend(nal);
    }
    record
}

#[test]
fn h264_parameter_sets_can_come_from_the_configuration_record_alone() {
    // An MP4 or Matroska track whose samples carry no SPS or PPS: the
    // record's sets give the frames of a stream that repeats them in band,
    // and without them no picture is decoded. A stream read from its second
    // IDR picture, as after a seek, gives that stream's frames too.
    let sps = AvcSps::default();
    let coding = VideoCoding::Nal(Codec::H264, Framing::Lengths(4));
    let gop = [avc_gop(0), avc_gop(0)].concat();
    let samples = |in_band: bool| -> Vec<(Vec<u8>, i64)> {
        gop.iter()
            .enumerate()
            .map(|(n, &(slice_type, reference, lsb, frame_num))| {
                let idr = n % 7 == 0;
                let slice = AvcSlice {
                    idr,
                    reference,
                    slice_type,
                    frame_num,
                    poc_lsb: lsb,
                    ..AvcSlice::default()
                };
                let sets = (idr && in_band).then_some(&sps);
                let pts = 90_000 + (n / 7 * 14 + lsb as usize / 2) as i64 * 3003;
                (length_prefixed(&avc_nals(sets, n as u8, &slice, 0)), pts)
            })
            .collect()
    };
    let (in_band, alone) = (samples(true), samples(false));
    let record = avcc(&avc_sps(&sps), &avc_pps());
    let expected = extract(coding, ts_packets(&in_band));
    assert_eq!(expected.len(), 14);
    assert_eq!(
        extract_configured(coding, &record, ts_packets(&alone)),
        expected
    );
    assert_eq!(extract(coding, ts_packets(&alone)), []);
    assert_eq!(
        extract_configured(coding, &record, ts_packets(&alone[7..])),
        extract(coding, ts_packets(&in_band[7..]))
    );
    assert_eq!(tags(&extract(coding, ts_packets(&in_band[7..]))).len(), 7);
}

#[test]
fn hevc_parameter_sets_can_come_from_the_configuration_record_alone() {
    // As for H.264: without the record's sets no picture is decoded and the
    // captions only gather, waiting for one. Read from the CRA picture, as
    // after a seek, its RASL pictures are not decoded either way.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::Lengths(4));
    let samples = |in_band: bool| -> Vec<(Vec<u8>, i64)> {
        hevc_open_gop()
            .iter()
            .enumerate()
            .map(|(n, (kind, poc, deltas))| {
                let mut nals = Vec::new();
                if in_band && (16..=23).contains(kind) {
                    nals.extend([hevc_sps(4, 1, 0), hevc_pps()]);
                }
                nals.push(caption_sei(&[0x4e, 0x01], n as u8));
                nals.push(hevc_slice(*kind, *poc, deltas, false));
                (length_prefixed(&nals), 90_000 + i64::from(*poc) * 3003)
            })
            .collect()
    };
    let (in_band, alone) = (samples(true), samples(false));
    let record = hvcc(&[hevc_sps(4, 1, 0), hevc_pps()]);
    let expected = extract(coding, ts_packets(&in_band));
    assert_eq!(expected.len(), 13);
    assert_eq!(
        extract_configured(coding, &record, ts_packets(&alone)),
        expected
    );
    assert_eq!(extract(coding, ts_packets(&alone)), []);
    let from_cra = extract(coding, ts_packets(&in_band[7..]));
    assert!(!from_cra.is_empty());
    assert_eq!(
        extract_configured(coding, &record, ts_packets(&alone[7..])),
        from_cra
    );
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
fn hevc_reserved_vcl_types_do_not_decide_a_picture() {
    // Case hevc-reserved-ts: a reserved VCL NAL unit type (10 to 15, 22 to
    // 31) is not a slice, and the CRA slice after it decides the picture.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    for reserved in (10..=15).chain(22..=31) {
        let first = annex_b(&[
            hevc_sps(4, 1, 0),
            hevc_pps(),
            vec![reserved << 1, 1, 0x80],
            caption_sei(&[0x4e, 0x01], 0),
            hevc_slice(21, 0, &[], false),
        ]);
        let units = [
            (first, 0),
            (hevc_au(false, 1, hevc_slice(1, 1, &[-1], false)), 1001),
        ];
        let frames = extract(coding, ts_packets(&units));
        assert_eq!(tags(&frames), [[0], [1]], "type {reserved}");
        let pts: Vec<_> = frames.iter().map(|f| f.0).collect();
        assert_eq!(pts, [Some(0), Some(1001)], "type {reserved}");
    }
}

#[test]
fn hevc_cra_after_an_end_of_sequence_discards_the_pictures_waiting() {
    // Case hevc-eos-ts: the CRA picture
    // after an end of sequence NAL unit discards P6, still waiting, with its
    // captions; its RASL pictures are not decoded and pass their captions to
    // P12. Without the end of sequence (case hevc-cra-ts) everything is
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
    // Case hevc-eos-rasl-ts: an end of sequence NAL unit before a RASL
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
    // Case hevc-trail-ts: a stream starting at a trailing picture before a
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
    // C.5.2.2: an IDR picture with no_output_of_prior_pics_flag discards the
    // picture still waiting (P6 here); without the flag it is output.
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

/// An HEVC access unit with the parameter sets `sps` when given.
fn hevc_au_with(sps: Option<Vec<u8>>, n: u8, slice: Vec<u8>) -> Vec<u8> {
    let mut nals = Vec::new();
    if let Some(sps) = sps {
        nals.extend([sps, hevc_pps()]);
    }
    nals.extend([caption_sei(&[0x4e, 0x01], n), slice]);
    annex_b(&nals)
}

#[test]
fn hevc_buffer_sizes_below_the_reorder_count_are_raised_and_above_16_refused() {
    // Case hevc-dpb-raise-ts: sps_max_dec_pic_buffering_minus1 below
    // sps_max_num_reorder_pics is taken as the reorder count; the reference
    // outputs nothing for a sequence whose buffer, so raised, passes 16
    // pictures.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    for (dpb_minus1, reorder, count) in [(1, 3, 7), (15, 15, 7), (16, 2, 0), (15, 16, 0)] {
        let mut units = hevc_stream(&hevc_open_gop()[..7]);
        let idr = hevc_slice(20, 0, &[], false);
        units[0].0 = hevc_au_with(Some(hevc_sps(dpb_minus1, reorder, 0)), 0, idr);
        assert_eq!(extract(coding, ts_packets(&units)).len(), count);
    }
}

#[test]
fn hevc_pictures_waiting_too_long_leave_by_latency() {
    // C.5.2.3 with sps_max_num_reorder_pics 2 and
    // sps_max_latency_increase_plus1 2 (SpsMaxLatencyPictures 3): IDR0 P10
    // P1 P2 P3 P4 P5. P10 leaves once three pictures follow it in decode
    // order; without the latency limit, at the end.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let pocs = [0, 10, 1, 2, 3, 4, 5];
    for (latency_plus1, order) in [
        (2, [[0], [2], [3], [4], [1], [5], [6]]),
        (0, [[0], [2], [3], [4], [5], [6], [1]]),
    ] {
        let units: Vec<_> = pocs
            .iter()
            .enumerate()
            .map(|(n, &poc)| {
                let au = if n == 0 {
                    let sps = hevc_sps(4, 2, latency_plus1);
                    hevc_au_with(Some(sps), 0, hevc_slice(19, 0, &[], false))
                } else {
                    hevc_au_with(None, n as u8, hevc_slice(1, poc, &[], false))
                };
                (au, 90_000 + i64::from(poc) * 3003)
            })
            .collect();
        assert_eq!(tags(&extract(coding, ts_packets(&units))), order);
    }
}

#[test]
fn hevc_reference_picture_sets_in_every_coding_are_read() {
    // Short-term sets from the SPS by index (the second predicted from the
    // first), a slice's own set predicted from the SPS's last, and long-term
    // pictures by SPS index or LSBs with an MSB cycle: every picture is
    // decoded and gets its own frame.
    let sets = [
        StSet::Explicit(vec![-1]),
        StSet::Predicted {
            delta_rps: -1,
            used: vec![true, true],
        },
    ];
    let sps = hevc_sps_with(4, 1, 0, &sets, &[0]);
    let predicted = SliceRps::Own(StSet::Predicted {
        delta_rps: -1,
        used: vec![true, true, true],
    });
    let pictures: [(SliceRps, Vec<LongTerm>); 5] = [
        (SliceRps::Index { index: 0, sets: 2 }, vec![]),
        (SliceRps::Index { index: 1, sets: 2 }, vec![]),
        (predicted, vec![]),
        (
            SliceRps::Own(StSet::Explicit(vec![-1])),
            vec![LongTerm::Sps { index: 0 }],
        ),
        (
            SliceRps::Own(StSet::Explicit(vec![-1])),
            vec![LongTerm::Own {
                lsb: 0,
                msb_cycle: Some(0),
            }],
        ),
    ];
    let mut units = vec![(
        hevc_au_with(Some(sps), 0, hevc_slice(19, 0, &[], false)),
        90_000,
    )];
    for (k, (rps, long_term)) in pictures.iter().enumerate() {
        let poc = k as u32 + 1;
        let slice = hevc_slice_with(1, poc, rps, Some((1, long_term)));
        units.push((
            hevc_au_with(None, poc as u8, slice),
            90_000 + i64::from(poc) * 3003,
        ));
    }
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    assert_eq!(
        tags(&extract(coding, ts_packets(&units))),
        [[0], [1], [2], [3], [4], [5]]
    );
}

#[test]
fn hevc_pictures_kept_for_reference_beyond_the_buffer_are_let_go() {
    // Trailing pictures alternating order counts 1 and 0 after an IDR
    // picture, each with short-term deltas -1 and +1 and, in the second
    // stream, long-term pictures named by LSBs 0 and 1: there every picture
    // stays a reference. The pictures stored beyond the SPS's buffer size are
    // removed once output, so memory does not grow with the stream.
    let coding = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let sps = hevc_sps_with(4, 1, 0, &[StSet::Explicit(vec![-1])], &[0]);
    let long_term = [
        LongTerm::Own {
            lsb: 0,
            msb_cycle: None,
        },
        LongTerm::Own {
            lsb: 1,
            msb_cycle: None,
        },
    ];
    for named in [&long_term[..0], &long_term[..]] {
        let pool = Budget::new(1 << 24).expect("pool");
        let mut work = WorkBudget::new(1 << 32);
        let mut extractor = CaptionExtractor::new(&pool, coding, (1, 90_000));
        let mut frames = 0;
        let mut used = Vec::new();
        for n in 0..4000u32 {
            let au = if n == 0 {
                hevc_au_with(Some(sps.clone()), 0, hevc_slice(19, 0, &[], false))
            } else {
                let rps = SliceRps::Own(StSet::Explicit(vec![-1, 1]));
                let slice = hevc_slice_with(1, n % 2, &rps, Some((1, named)));
                hevc_au_with(None, (n % 128) as u8, slice)
            };
            let packet = VideoPacket {
                data: &au,
                pts: Some(90_000 + i64::from(n) * 3003),
                dts: Some(i64::from(n) * 3003),
                duration: None,
            };
            extractor.push(packet, &mut work).expect("push");
            while extractor.pop().expect("pop").is_some() {
                frames += 1;
            }
            used.push(pool.used());
        }
        extractor.finish().expect("finish");
        while extractor.pop().expect("pop").is_some() {
            frames += 1;
        }
        assert_eq!(frames, 4000);
        // The caption bytes are compacted from 4096 on: past that, the most
        // memory used stays put.
        let early = used[1500..2750].iter().max().copied();
        let late = used[2750..].iter().max().copied();
        assert!(late <= early, "{late:?} after {early:?}");
    }
}

/// Access units of `pictures` (slice type, reference, LSB, frame_num) from a
/// non-IDR first picture with the parameter sets and, when given, a
/// recovery point of that `recovery_frame_cnt`.
fn avc_start(
    sps: &AvcSps,
    pictures: &[(u32, bool, u32, u32)],
    recovery: Option<u32>,
) -> Vec<(Vec<u8>, i64)> {
    let mut units = avc_stream(sps, pictures, &[]);
    let (slice_type, reference, poc_lsb, frame_num) = pictures[0];
    let slice = AvcSlice {
        reference,
        slice_type,
        frame_num,
        poc_lsb,
        ..AvcSlice::default()
    };
    let mut nals = vec![avc_sps(sps), avc_pps()];
    if let Some(count) = recovery {
        // A one-byte payload for recovery_frame_cnt up to 2.
        let mut point = Bits::default();
        point
            .u(6, 8)
            .u(1, 8)
            .ue(count)
            .flag(true)
            .flag(false)
            .u(0, 2);
        nals.push(point.nal(&[0x06]));
    }
    nals.push(caption_sei(&[0x06], 0));
    nals.push(avc_slice(&slice, sps.poc_type));
    units[0].0 = annex_b(&nals);
    units
}

/// Pictures from a non-IDR I picture (order count 40, `frame_num` 5): the
/// kinds of the first (then P pictures), order counts 2 apart but where
/// `steps` says otherwise.
fn start_pictures(
    kinds: &[u32],
    steps: &[(usize, u32)],
    count: usize,
) -> Vec<(u32, bool, u32, u32)> {
    let (mut poc, mut frame_num) = (40u32, 5u32);
    (0..count)
        .map(|n| {
            if n > 0 {
                poc += steps.iter().find(|s| s.0 == n).map_or(2, |s| s.1);
            }
            let kind = kinds.get(n).copied().unwrap_or(0);
            let reference = kind != 1;
            let picture = (kind, reference, poc % 256, frame_num % 16);
            frame_num += u32::from(reference);
            picture
        })
        .collect()
}

#[test]
fn h264_intra_pictures_before_recovery_are_kept() {
    // Probe p89 I-k3-r2; case h264-start-intra-run-ts: with reorder 2 the two
    // pictures above the first intra picture are not recovered, but intra
    // ones are output.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: Some(2),
        ..AvcSps::default()
    };
    let frames = extract(
        coding,
        ts_packets(&avc_start(&sps, &start_pictures(&[2, 2, 2], &[], 30), None)),
    );
    assert_eq!(tags(&frames)[..5], [[0], [1], [2], [3], [4]]);
    let frames = extract(
        coding,
        ts_packets(&avc_start(&sps, &start_pictures(&[2, 0, 2], &[], 30), None)),
    );
    assert_eq!(tags(&frames)[..4], [[0], [2], [3], [4]]);
}

#[test]
fn h264_a_b_picture_among_the_first_21_drops_the_first_above_the_intra_picture() {
    // Probes p89d, p89e; case h264-start-b-window-ts: without a restriction
    // a B picture at index 14 or 20 drops picture 1 and keeps the intra
    // picture, one at 21 does not.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: None,
        ..AvcSps::default()
    };
    for (at, first) in [(14, [0, 2]), (20, [0, 2]), (21, [0, 1])] {
        let mut kinds = vec![0; at + 1];
        kinds[0] = 2;
        kinds[at] = 1;
        let pictures = start_pictures(&kinds, &[(at, 255), (at + 1, 3)], 45);
        let frames = extract(coding, ts_packets(&avc_start(&sps, &pictures, None)));
        assert_eq!(tags(&frames)[..2], [[first[0]], [first[1]]], "B at {at}");
    }
}

#[test]
fn h264_a_non_reference_intra_picture_with_frame_num_0_does_not_recover() {
    // Probe p91 fn-k5R-n; case h264-start-nonref-ts: the reference intra
    // picture at index 5 is the first; picture 6 is the first output.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: None,
        ..AvcSps::default()
    };
    let mut pictures = start_pictures(&[2, 0, 0, 0, 0, 2], &[], 30);
    let mut frame_num = 0;
    for (n, picture) in pictures.iter_mut().enumerate() {
        picture.1 = n != 0;
        picture.3 = frame_num % 16;
        frame_num += u32::from(picture.1);
    }
    let frames = extract(coding, ts_packets(&avc_start(&sps, &pictures, None)));
    assert_eq!(frames.len(), 24);
    assert_eq!(tags(&frames)[0], [6]);
}

#[test]
fn h264_a_recovery_point_above_the_intra_picture_recovers_from_it() {
    // Probe p90 pb-c1-r2; case h264-recovery-above-ts: the recovery point
    // names P50; B46 below it is the second picture above the intra picture
    // with reorder 2 and is dropped, B48 is kept.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: Some(2),
        ..AvcSps::default()
    };
    let mut pictures = vec![(2, true, 44, 5)];
    let (mut anchor, mut frame_num) = (44, 6);
    while pictures.len() < 30 {
        anchor += 6;
        pictures.extend([
            (0, true, anchor, frame_num),
            (1, false, anchor - 4, frame_num + 1),
            (1, false, anchor - 2, frame_num + 1),
        ]);
        frame_num += 1;
    }
    let frames = extract(coding, ts_packets(&avc_start(&sps, &pictures, Some(1))));
    assert_eq!(tags(&frames)[..5], [[0], [3], [1], [5], [6]]);
}

/// P frames from an IDR picture, a `frame_num` gap of one frame and an order
/// count step of 4 at index 30, a B picture one below at 31.
fn avc_gap_then_step(sps: &AvcSps) -> Vec<(Vec<u8>, i64)> {
    let mut pictures = vec![(2, true, 0, 0)];
    let (mut poc, mut frame_num) = (0, 0);
    for n in 1..50 {
        if n == 31 {
            pictures.push((1, false, poc - 1, frame_num + 1));
            continue;
        }
        frame_num += if n == 30 { 2 } else { 1 };
        poc += if n == 30 { 4 } else { 2 };
        pictures.push((0, true, poc % 256, frame_num % 16));
    }
    avc_stream(sps, &pictures, &[0])
}

#[test]
fn h264_a_gap_blocks_the_depth_raise_only_when_gaps_are_not_allowed() {
    // Probe p88c; cases h264-gap-jump-ts and h264-gap-allowed-ts.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    for gaps_allowed in [false, true] {
        let sps = AvcSps {
            reorder: None,
            gaps_allowed,
            ..AvcSps::default()
        };
        let frames = extract(coding, ts_packets(&avc_gap_then_step(&sps)));
        assert_eq!(tags(&frames).iter().any(|t| t == &[31]), gaps_allowed);
    }
}

#[test]
fn h264_a_stated_depth_of_0_is_raised_by_a_b_picture_up_to_index_20() {
    // Probe p88 B-k20, B-k21; case h264-stated-b20-ts: a B picture one below
    // the P frame before it raises reorder 0 to 1 at index 20, not at 21; the
    // B picture one below a step of 4 at 41 is kept only then.
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let sps = AvcSps {
        reorder: Some(0),
        ..AvcSps::default()
    };
    for (at, kept) in [(20, true), (21, false)] {
        let mut pictures = vec![(2, true, 0, 0)];
        let (mut poc, mut frame_num) = (0, 0);
        for n in 1..60 {
            if n == at || n == 41 {
                pictures.push((1, false, poc - 1, frame_num + 1));
                continue;
            }
            frame_num += 1;
            poc += if n == 40 { 4 } else { 2 };
            pictures.push((0, true, poc % 256, frame_num % 16));
        }
        let frames = extract(coding, ts_packets(&avc_stream(&sps, &pictures, &[0])));
        assert_eq!(tags(&frames).iter().any(|t| t == &[41]), kept, "B at {at}");
    }
}

/// The frames of `packets` (decode order) with every push and the finish
/// first refused: each is tried with the pool holding back all but a few
/// bytes, a few more on every try, until it succeeds. Also gives the number of
/// calls refused.
fn extract_starved(coding: VideoCoding, packets: &[VideoPacket<'_>]) -> (Vec<Frame>, usize) {
    let pool = Budget::new(1 << 24).expect("pool");
    let mut work = WorkBudget::new(1 << 24);
    let mut extractor = CaptionExtractor::new(&pool, coding, (1, 90_000));
    let owned = |f: CaptionFrame<'_>| (f.pts, f.duration, f.data.to_vec());
    let mut frames = Vec::new();
    let mut refused = 0;
    for at in 0..=packets.len() {
        for spare in (0..).step_by(8) {
            let front = extractor.peek().map(owned);
            let held = pool.reserve(pool.available() - spare).expect("hold");
            let result = match packets.get(at) {
                Some(&packet) => extractor.push(packet, &mut work),
                None => extractor.finish(),
            };
            drop(held);
            if result.is_ok() {
                break;
            }
            refused += 1;
            assert_eq!(extractor.peek().map(owned), front, "packet {at}");
        }
        while let Some(f) = extractor.pop().expect("pop") {
            frames.push(owned(f));
        }
    }
    (frames, refused)
}

#[test]
fn a_refused_push_or_finish_leaves_the_extractor_as_it_was() {
    // Whatever the pool refuses, the frames are those of an extraction it
    // never starves: B pictures, field pairs in one packet and in two,
    // durations estimated from the decode times, a start without an IDR
    // picture with a B picture among the first 21, and HEVC pictures not
    // decoded and discarded after an end of sequence.
    let h264 = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let hevc = VideoCoding::Nal(Codec::Hevc, Framing::StartCodes);
    let mut pictures = avc_gop(0);
    pictures.extend(
        avc_gop(0)
            .into_iter()
            .map(|(t, r, lsb, f)| (t, r, lsb + 14, f)),
    );
    let gop = avc_stream(&AvcSps::default(), &pictures, &[0, 7]);
    let untimed = AvcSps {
        reorder: None,
        timing: None,
        ..AvcSps::default()
    };
    let estimated = avc_stream(&untimed, &pictures.repeat(2), &[0, 7, 14, 21]);
    let mut kinds = vec![0; 15];
    kinds[0] = 2;
    kinds[14] = 1;
    let start = avc_start(
        &untimed,
        &start_pictures(&kinds, &[(14, 255), (15, 3)], 40),
        None,
    );
    let fields = AvcSps {
        frame_mbs_only: false,
        reorder: Some(2),
        max_ref_frames: 4,
        ..AvcSps::default()
    };
    let field = |k: u32, bottom: bool| {
        let (slice_type, reference, display) =
            [(2, true, 0), (0, true, 3), (1, false, 1)][k as usize];
        let slice = AvcSlice {
            idr: k == 0 && !bottom,
            reference,
            slice_type,
            frame_num: k.min(2),
            poc_lsb: 4 * display + u32::from(bottom),
            field: Some(bottom),
            ..AvcSlice::default()
        };
        let sps = (k == 0 && !bottom).then_some(&fields);
        avc_nals(sps, (2 * k + u32::from(bottom)) as u8, &slice, 0)
    };
    let paired: Vec<_> = (0..3)
        .map(|k| {
            (
                annex_b(&[field(k, false), field(k, true)].concat()),
                90_000 + i64::from(k) * 3003,
            )
        })
        .collect();
    let separate: Vec<_> = (0..6)
        .map(|n| {
            (
                annex_b(&field(n / 2, n % 2 == 1)),
                90_000 + i64::from(n) * 1501,
            )
        })
        .collect();
    let mut open = hevc_stream(&hevc_open_gop());
    open[7].0 = with_end_of_sequence(&open[7].0);
    for (coding, units) in [
        (h264, gop),
        (h264, estimated),
        (h264, start),
        (h264, paired),
        (h264, separate),
        (hevc, open),
    ] {
        let packets: Vec<_> = ts_packets(&units).collect();
        let (frames, refused) = extract_starved(coding, &packets);
        assert!(refused > units.len(), "{refused} refused");
        assert_eq!(frames, extract(coding, packets));
    }
}
