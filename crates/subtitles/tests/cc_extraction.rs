//! Caption data taken from video as FFmpeg's decoders attach it to frames.
//!
//! `tests/cc/mpeg2-open-gop.m2v` (authored by `harness/subtitles/cc_streams.py`)
//! starts at an open GOP, so its two leading B pictures are not output; its
//! `.packets` file records the reference's caption packets.

use std::fs;
use std::path::PathBuf;
use triergon_core::{Budget, WorkBudget};
use triergon_harness_protocol::unhex;
use triergon_subtitles::cc::cc_data::{CcType, triplets};
use triergon_subtitles::cc::dtvcc::Assembler;
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
fn dtvcc_packets_survive_extraction() {
    let es = fs::read(fixture("mpeg2-open-gop.m2v")).expect("stream");
    let frames = extract(VideoCoding::Mpeg2Raw, [packet(&es, None)]);
    let mut assembler = Assembler::new();
    let mut texts = Vec::new();
    for (_, _, data) in &frames {
        for triplet in triplets(data) {
            if matches!(triplet.kind(), CcType::DtvccStart | CcType::DtvccData)
                && let Some(packet) = assembler.push(triplet)
            {
                for block in packet.service_blocks() {
                    texts.push((block.service, block.data.to_vec()));
                }
            }
        }
    }
    // The generator sends "DTVnn" on service 2 for every third packet, else 1.
    assert!(!texts.is_empty());
    for (service, text) in &texts {
        assert!(text.starts_with(b"DTV"), "{text:?}");
        let n: u32 = std::str::from_utf8(&text[3..])
            .expect("digits")
            .parse()
            .expect("n");
        assert_eq!(*service, if n.is_multiple_of(3) { 2 } else { 1 });
    }
}

/// An H.264 access unit: an SEI NAL of `messages`, then an IDR slice header.
fn h264_access_unit(messages: &[(u8, &[u8])]) -> Vec<u8> {
    let mut sei = vec![0x06];
    for (kind, payload) in messages {
        sei.push(*kind);
        sei.push(u8::try_from(payload.len()).expect("short payload"));
        // Emulation prevention, as an encoder inserts it.
        let mut zeros = 0;
        for &b in *payload {
            if zeros >= 2 && b <= 3 {
                sei.push(3);
                zeros = 0;
            }
            sei.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
    }
    sei.push(0x80);
    let mut au = vec![0, 0, 0, 1, 0x09, 0x10];
    au.extend_from_slice(&[0, 0, 1]);
    au.extend_from_slice(&sei);
    au.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x84]);
    au
}

#[test]
fn sei_captions_follow_presentation_order() {
    let atsc = |cc: &[u8]| {
        let mut payload = b"\xb5\x00\x31GA94\x03".to_vec();
        payload.push(0x40 | u8::try_from(cc.len() / 3).expect("count"));
        payload.push(0xff);
        payload.extend_from_slice(cc);
        payload.push(0xff);
        payload
    };
    let first = atsc(&[0xfc, 0x94, 0x20, 0xfd, 0x00, 0x00]);
    let second = atsc(&[0xfc, 0x80, 0x80]);
    // Unprocessed constructs and another provider's T.35 data are ignored.
    let unprocessed = b"\xb5\x00\x31GA94\x03\x01\xff\xfc\x94\x2c\xff".to_vec();
    let foreign = b"\xb5\x00\x2fGA94\x03\x41\xff\xfc\x94\x2c\xff".to_vec();
    let units = [
        h264_access_unit(&[(4, &first), (5, b"x264"), (4, &unprocessed)]),
        h264_access_unit(&[(4, &foreign), (4, &second)]),
        h264_access_unit(&[]),
    ];
    // Without stream timing or durations, a frame lasts the decode time step.
    let access_units = [(0, &units[2]), (2002, &units[0]), (1001, &units[1])]
        .into_iter()
        .zip([-1001, 0, 1001])
        .map(|((pts, data), dts)| VideoPacket {
            data,
            pts: Some(pts),
            dts: Some(dts),
            duration: None,
        });
    let frames = extract(
        VideoCoding::Nal(Codec::H264, Framing::StartCodes),
        access_units,
    );
    assert_eq!(
        frames,
        [
            (Some(1001), Some(1001), vec![0xfc, 0x80, 0x80]),
            (
                Some(2002),
                Some(1001),
                vec![0xfc, 0x94, 0x20, 0xfd, 0x00, 0x00]
            ),
        ]
    );
}

#[test]
fn sequence_parameter_sets_state_the_frame_rate() {
    use triergon_subtitles::cc::video::nal::{Timing, sps_timing};
    // From libx264 (High 4:4:4) and libx265 encodes at 30000/1001 Hz.
    let h264 = unhex("67f4000a919a623d808800001f4800075300789129c0");
    let hevc = unhex(
        "4201010408000003009e0800000300001e90041062cb2225c995e02dc080800100000303e90000753008",
    );
    assert_eq!(
        sps_timing(Codec::H264, &h264),
        Some(Timing {
            num_units_in_tick: 1001,
            time_scale: 60000
        })
    );
    assert_eq!(
        sps_timing(Codec::Hevc, &hevc),
        Some(Timing {
            num_units_in_tick: 1001,
            time_scale: 30000
        })
    );
}

/// An access unit of NAL units in start code framing.
fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|nal| [&[0, 0, 1][..], nal].concat())
        .collect()
}

/// A caption SEI message with one field 1 pair `0x80 + n`, then the trailing bits.
fn caption(n: u8) -> Vec<u8> {
    let mut message = vec![4, 14];
    message.extend_from_slice(b"\xb5\x00\x31GA94\x03\x41\xff");
    message.extend_from_slice(&[0xfc, 0x80 + n, 0x80, 0xff, 0x80]);
    message
}

/// Packets lasting 1001 ticks at `pts`.
fn timed(units: &[(Vec<u8>, i64)]) -> impl Iterator<Item = VideoPacket<'_>> {
    units.iter().map(|(data, pts)| VideoPacket {
        data,
        pts: Some(*pts),
        dts: None,
        duration: Some(1001),
    })
}

#[test]
fn h264_output_starts_at_an_intra_picture() {
    // Slice headers: first_mb_in_slice 0, then slice_type I (0xb0), P (0xc0), B (0xa0).
    let au = |n: u8, header: u8, slice: u8| {
        let sei = [&[0x06][..], &caption(n)].concat();
        annex_b(&[&sei, &[header, slice]])
    };
    // Decode order: P, I (non-IDR, no recovery point), B and B (leading), P, B.
    let units = [
        (au(0, 0x41, 0xc0), 0),
        (au(1, 0x61, 0xb0), 3003),
        (au(2, 0x01, 0xa0), 1001),
        (au(3, 0x01, 0xa0), 2002),
        (au(4, 0x41, 0xc0), 6006),
        (au(5, 0x01, 0xa0), 4004),
    ];
    let frames = extract(
        VideoCoding::Nal(Codec::H264, Framing::StartCodes),
        timed(&units),
    );
    // The leading pictures and, without a recovery point, the next P picture go.
    let pts: Vec<_> = frames.iter().map(|f| f.0).collect();
    assert_eq!(pts, [Some(3003), Some(4004)]);
}

#[test]
fn hevc_skipped_pictures_pass_their_captions_on() {
    let au = |n: u8, nal_type: u8| {
        let sei = [&[39 << 1, 1][..], &caption(n)].concat();
        annex_b(&[&sei, &[nal_type << 1, 1, 0xaf]])
    };
    // Decode order: TRAIL (before any IRAP), CRA, RASL, RASL, TRAIL.
    let units = [
        (au(0, 1), 0),
        (au(1, 21), 3003),
        (au(2, 8), 1001),
        (au(3, 8), 2002),
        (au(4, 1), 4004),
    ];
    let frames = extract(
        VideoCoding::Nal(Codec::Hevc, Framing::StartCodes),
        timed(&units),
    );
    let pair = |n: u8| [0xfc, 0x80 + n, 0x80];
    assert_eq!(
        frames,
        [
            (Some(3003), Some(1001), [pair(0), pair(1)].concat()),
            (Some(4004), Some(1001), [pair(2), pair(3), pair(4)].concat()),
        ]
    );
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
