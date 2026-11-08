//! A packet the caption extractor refuses leaves it as it was: the frames
//! afterwards are those of a stream without that packet.

use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::cc::video::nal::{Codec, Framing};
use triergon_subtitles::compat::cc::{CaptionExtractor, VideoCoding, VideoPacket};

/// One output frame: pts, duration and caption bytes.
type Frame = (Option<i64>, Option<i64>, Vec<u8>);

/// Where a refused packet goes in a stream, and what it is.
struct Refused<'a> {
    before: usize,
    packet: VideoPacket<'a>,
    work: u64,
}

/// The frames of `packets` through an extractor over a pool of `pool` bytes,
/// popped after every packet and after the end. Each refused packet must be
/// refused.
fn extract(
    coding: VideoCoding,
    pool: usize,
    packets: &[VideoPacket<'_>],
    refused: &[Refused<'_>],
) -> Vec<Frame> {
    let pool = Budget::new(pool).expect("pool");
    let mut extractor = CaptionExtractor::new(&pool, coding, (1, 90_000));
    let mut frames = Vec::new();
    let mut collect = |extractor: &mut CaptionExtractor| {
        while let Some(f) = extractor.pop().expect("pop") {
            frames.push((f.pts, f.duration, f.data.to_vec()));
        }
    };
    for (n, packet) in packets.iter().enumerate() {
        for bad in refused.iter().filter(|bad| bad.before == n) {
            let mut work = WorkBudget::new(bad.work);
            assert!(extractor.push(bad.packet, &mut work).is_err());
            collect(&mut extractor);
        }
        extractor
            .push(*packet, &mut WorkBudget::new(u64::MAX))
            .expect("push");
        collect(&mut extractor);
    }
    extractor.finish().expect("finish");
    collect(&mut extractor);
    frames
}

/// An MPEG-2 picture of `kind` (1 I, 2 P, 3 B) with one A/53 caption pair and
/// a slice of `slice` bytes.
fn mpeg2_picture(kind: u8, caption: u8, slice: usize) -> Vec<u8> {
    let mut picture = vec![0, 0, 1, 0x00, 0, kind << 3, 0xff, 0xf8];
    picture.extend_from_slice(b"\0\0\x01\xb2GA94\x03\x41\xff\xfc");
    picture.extend_from_slice(&[caption, caption]);
    picture.extend_from_slice(&[0, 0, 1, 0x01]);
    picture.resize(picture.len() + slice, 0x55);
    picture
}

/// A 29.97 Hz sequence header.
const MPEG2_SEQUENCE: [u8; 12] = [
    0, 0, 1, 0xb3, 0x2d, 0x01, 0xe0, 0x24, 0xff, 0xff, 0xe0, 0x18,
];

#[test]
fn a_refused_mpeg2_packet_leaves_the_extractor_as_it_was() {
    let mut stream = MPEG2_SEQUENCE.to_vec();
    for (n, kind) in [1, 3, 3, 2, 3, 3, 2, 3, 3, 2, 3, 3].into_iter().enumerate() {
        stream.extend(mpeg2_picture(kind, 0x80 | n as u8, 300));
    }
    let packets: Vec<_> = stream
        .chunks(184)
        .enumerate()
        .map(|(n, data)| VideoPacket {
            data,
            pts: Some(n as i64 * 1501),
            dts: None,
            duration: None,
        })
        .collect();
    // It completes the picture being received and a captioned P picture,
    // then P pictures whose user data each announce 31 constructs read from
    // the bytes after them, nearly 6000 caption bytes a frame: more than the
    // pool holds.
    let mut bad = mpeg2_picture(2, 0x7f, 16);
    for _ in 0..500 {
        bad.extend_from_slice(&[0, 0, 1, 0x00, 0, 2 << 3, 0xff, 0xf8]);
        for _ in 0..64 {
            bad.extend_from_slice(b"\0\0\x01\xb2GA94\x03\x5f\xff");
        }
        bad.extend_from_slice(&[0, 0, 1, 0x01]);
        bad.extend_from_slice(&[0x55; 100]);
    }
    bad.extend_from_slice(&[0, 0, 1, 0x00, 0, 2 << 3]);
    let refused = [
        Refused {
            before: 9,
            packet: VideoPacket {
                data: &bad,
                pts: Some(1),
                dts: None,
                duration: None,
            },
            work: u64::MAX,
        },
        Refused {
            before: 15,
            packet: packets[15],
            work: 0,
        },
    ];
    for coding in [VideoCoding::Mpeg2Pes, VideoCoding::Mpeg2Raw] {
        let expected = extract(coding, 1 << 21, &packets, &[]);
        assert_eq!(expected.len(), 10);
        assert_eq!(extract(coding, 1 << 21, &packets, &refused), expected);
    }
}

/// An access unit: an SEI NAL unit of `messages` (type and payload), then a
/// slice NAL unit.
fn h264_access_unit(messages: &[(u8, Vec<u8>)], slice: [u8; 2]) -> Vec<u8> {
    let mut au = vec![0, 0, 1, 0x06];
    for (kind, payload) in messages {
        au.push(*kind);
        au.push(u8::try_from(payload.len()).expect("short payload"));
        au.extend_from_slice(payload);
    }
    au.extend_from_slice(&[0x80, 0, 0, 1]);
    au.extend_from_slice(&slice);
    au
}

/// A T.35 A/53 payload announcing `count` constructs, then `cc_data`.
fn atsc(count: u8, cc_data: &[u8]) -> Vec<u8> {
    let mut payload = b"\xb5\x00\x31GA94\x03".to_vec();
    payload.extend_from_slice(&[0x40 | count, 0xff]);
    payload.extend_from_slice(cc_data);
    payload
}

#[test]
fn a_refused_access_unit_leaves_the_extractor_as_it_was() {
    // IDR, then P pictures, one frame period apart in decode order and
    // without container durations, so each frame lasts the least step.
    let units: Vec<_> = (0..40u8)
        .map(|n| {
            let slice = if n == 0 { [0x65, 0x88] } else { [0x41, 0xc0] };
            h264_access_unit(&[(4, atsc(1, &[0xfc, 0x80 | n, 0x80]))], slice)
        })
        .collect();
    let packets: Vec<_> = units
        .iter()
        .enumerate()
        .map(|(n, data)| VideoPacket {
            data,
            pts: Some(n as i64 * 3003),
            dts: Some(n as i64 * 3003),
            duration: None,
        })
        .collect();
    // One tick after the last decode time, and more caption bytes than the
    // pool holds.
    let message = (4, atsc(31, &[0xfc, 0x94, 0x2c].repeat(31)));
    let bad = h264_access_unit(&vec![message; 40_000], [0x41, 0xc0]);
    let refused = [
        Refused {
            before: 20,
            packet: VideoPacket {
                data: &bad,
                pts: Some(19 * 3003 + 1),
                dts: Some(19 * 3003 + 1),
                duration: None,
            },
            work: u64::MAX,
        },
        Refused {
            before: 30,
            packet: packets[30],
            work: 0,
        },
    ];
    let coding = VideoCoding::Nal(Codec::H264, Framing::StartCodes);
    let expected = extract(coding, 1 << 20, &packets, &[]);
    assert_eq!(expected.len(), 40);
    assert!(expected.iter().all(|frame| frame.1 == Some(3003)));
    assert_eq!(extract(coding, 1 << 20, &packets, &refused), expected);
}
