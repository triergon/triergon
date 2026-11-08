//! Malformed caption input never panics or hangs: random and mutated bytes
//! through the receiver, the SCC reader and the video extraction.

use std::fs;
use std::path::PathBuf;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::cc::video::nal::{Codec, Framing, Timing, nal_units, sps_timing};
use triergon_subtitles::compat::cc::{
    CaptionExtractor, Clock, Decoder, Options, SccPackets, Subtitle, VideoCoding, VideoPacket,
};

/// A small deterministic generator (xorshift64*).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

#[test]
fn the_receiver_takes_any_bytes() {
    let pool = Budget::new(1 << 24).expect("pool");
    let mut random = Random(0x5eed);
    for options in [
        Options::default(),
        Options {
            real_time: true,
            real_time_latency_ms: 0,
            ..Options::default()
        },
    ] {
        let mut decoder = Decoder::new(&pool, options);
        let mut subtitle = Subtitle::new(&pool);
        for n in 0..4000 {
            let len = (random.next() % 64) as usize;
            let mut data = random.bytes(len);
            // Mostly well-formed flag bytes, so the pairs reach the caption memory.
            for flags in data.iter_mut().step_by(3) {
                if !random.next().is_multiple_of(4) {
                    *flags = 0xfc;
                }
            }
            let pts = match n % 50 {
                0 => None,
                1 => Some(i64::MIN + 1),
                2 => Some(i64::MAX),
                _ => Some(n * 33_366),
            };
            let clock = Clock {
                pts,
                duration_ms: random.next() as u32,
            };
            decoder.decode(&data, clock, &mut subtitle).expect("decode");
            for line in subtitle.lines() {
                assert!(!line.is_empty());
            }
        }
        while decoder.drain(&mut subtitle).expect("drain") {}
    }
}

#[test]
fn the_scc_reader_takes_any_text() {
    let pool = Budget::new(1 << 24).expect("pool");
    let mut random = Random(7);
    let alphabet = b"0123456789:; \t\r\nabcdefABCDEFxyz-+.,";
    for _ in 0..500 {
        let len = (random.next() % 400) as usize;
        let text: Vec<u8> = (0..len)
            .map(|_| alphabet[(random.next() % alphabet.len() as u64) as usize])
            .collect();
        let packets = SccPackets::read(&text, &pool, &mut WorkBudget::new(1 << 20)).expect("read");
        assert!(packets.iter().all(|p| p.data.len() % 3 == 0));
    }
}

#[test]
fn video_extraction_takes_any_bytes() {
    let pool = Budget::new(1 << 26).expect("pool");
    let es =
        fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cc/mpeg2-open-gop.m2v"))
            .expect("stream");
    let mut random = Random(99);
    let run = |coding: VideoCoding, packets: &[(Vec<u8>, Option<i64>)]| {
        let mut extractor = CaptionExtractor::new(&pool, coding, (1, 90_000));
        let mut work = WorkBudget::new(1 << 24);
        for (data, pts) in packets {
            let packet = VideoPacket {
                data,
                pts: *pts,
                dts: None,
                duration: None,
            };
            extractor.push(packet, &mut work).expect("push");
            while extractor.pop().expect("pop").is_some() {}
        }
        extractor.finish().expect("finish");
        while extractor.pop().expect("pop").is_some() {}
    };
    for _ in 0..200 {
        let mut mutated = es.clone();
        for _ in 0..(random.next() % 64) {
            let at = (random.next() as usize) % mutated.len();
            mutated[at] = random.next() as u8;
        }
        mutated.truncate((random.next() as usize) % (mutated.len() + 1));
        let size = 1 + (random.next() as usize) % 4000;
        let pieces: Vec<_> = mutated
            .chunks(size)
            .map(|piece| (piece.to_vec(), Some(random.next() as i64 >> 2)))
            .collect();
        run(VideoCoding::Mpeg2Pes, &pieces);
        run(VideoCoding::Mpeg2Raw, &pieces);
        let units: Vec<_> = (0..40)
            .map(|_| {
                let len = (random.next() % 300) as usize;
                (random.bytes(len), Some(random.next() as i64))
            })
            .collect();
        for framing in [
            Framing::StartCodes,
            Framing::Lengths(4),
            Framing::Lengths(1),
        ] {
            for codec in [Codec::H264, Codec::Hevc] {
                run(VideoCoding::Nal(codec, framing), &units);
            }
        }
    }
}

/// Writes RBSP bits and frames them as a NAL unit.
#[derive(Default)]
struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    fn put(&mut self, value: u64, width: u32) -> &mut Self {
        self.bits
            .extend((0..width).rev().map(|bit| value >> bit & 1 == 1));
        self
    }

    fn flag(&mut self, set: bool) -> &mut Self {
        self.put(u64::from(set), 1)
    }

    fn ue(&mut self, value: u32) -> &mut Self {
        let code = u64::from(value) + 1;
        let width = 64 - code.leading_zeros();
        self.put(0, width - 1).put(code, width)
    }

    /// The NAL unit: `header`, the bits with their stop bit, and emulation
    /// prevention bytes.
    fn nal(&mut self, header: &[u8]) -> Vec<u8> {
        self.flag(true);
        while !self.bits.len().is_multiple_of(8) {
            self.flag(false);
        }
        let mut nal = header.to_vec();
        let mut zeros = 0;
        for byte in self.bits.chunks(8) {
            let byte = byte.iter().fold(0u8, |b, &bit| b << 1 | u8::from(bit));
            if zeros == 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            nal.push(byte);
        }
        nal
    }
}

/// An HEVC sequence parameter set with VUI timing 1001/60000 whose
/// `log2_max_pic_order_cnt_lsb_minus4` is coded as `poc_lsb_minus4`, with one
/// long-term reference picture whose POC LSBs take that many bits plus four.
fn hevc_sps(poc_lsb_minus4: u32) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.put(0, 4).put(0, 3).flag(true); // VPS, one sub-layer, nesting
    bits.put(0x0160_0000, 32).put(0, 32).put(0, 24).put(93, 8); // profile_tier_level
    bits.ue(0).ue(1).ue(1920).ue(1080).flag(false); // id, 4:2:0, size, no window
    bits.ue(0).ue(0).ue(poc_lsb_minus4);
    bits.flag(true).ue(4).ue(0).ue(0); // sub-layer ordering
    for _ in 0..6 {
        bits.ue(0); // block sizes and transform depths
    }
    bits.flag(false).put(0, 2).flag(false); // no scaling lists, AMP, SAO, PCM
    bits.ue(0); // no short-term reference picture sets
    let poc_lsb_bits = poc_lsb_minus4.wrapping_add(4).min(16);
    bits.flag(true).ue(1).put(1, poc_lsb_bits).flag(true); // one long-term picture
    bits.put(0, 2).flag(true); // TMVP, strong intra smoothing; VUI
    bits.put(0, 4).put(0, 3).flag(false); // VUI flags up to the display window
    bits.flag(true).put(1001, 32).put(60_000, 32).flag(false);
    bits.flag(false).flag(false); // no bitstream restriction; no extensions
    bits.nal(&[0x42, 0x01])
}

#[test]
fn an_h264_scaling_delta_out_of_range_rejects_the_parameter_set() {
    // Profile 100 with a first scaling list delta of i32::MAX.
    let access_unit = [
        0, 0, 1, 0x67, 0x64, 0, 0, 0x5d, 0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xfe,
    ];
    let nal = nal_units(&access_unit, Framing::StartCodes)
        .next()
        .expect("NAL unit");
    assert_eq!(sps_timing(Codec::H264, nal), None);
    let pool = Budget::new(1 << 20).expect("pool");
    let mut extractor = CaptionExtractor::new(
        &pool,
        VideoCoding::Nal(Codec::H264, Framing::StartCodes),
        (1, 90_000),
    );
    let packet = VideoPacket {
        data: &access_unit,
        pts: Some(0),
        dts: None,
        duration: None,
    };
    extractor
        .push(packet, &mut WorkBudget::new(1 << 10))
        .expect("push");
    extractor.finish().expect("finish");
    assert_eq!(extractor.pop().expect("pop"), None);
}

#[test]
fn hevc_poc_lsb_lengths_are_limited_to_16_bits_as_ffmpeg_sums_them() {
    let timing = Some(Timing {
        num_units_in_tick: 1001,
        time_scale: 60_000,
    });
    assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(12)), timing);
    assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(13)), None);
    assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(0xffff_fffb)), None);
    // FFmpeg sums in 32 bits: 0xFFFFFFFE codes two-bit LSBs.
    assert_eq!(sps_timing(Codec::Hevc, &hevc_sps(0xffff_fffe)), timing);
}

#[test]
fn nal_lengths_past_the_access_unit_or_of_no_size_frame_nothing() {
    let access_unit = [0xff, 0xff, 0xff, 0xff, 0x00];
    assert_eq!(nal_units(&access_unit, Framing::Lengths(4)).count(), 0);
    for size in [0, 5, 255] {
        assert_eq!(nal_units(&access_unit, Framing::Lengths(size)).count(), 0);
    }
    // A complete unit before an overlong length is still read.
    let units: Vec<_> = nal_units(&[0, 1, 0x06, 0xff, 0xff, 9], Framing::Lengths(2)).collect();
    assert_eq!(units, [&[0x06][..]]);
    let pool = Budget::new(1 << 20).expect("pool");
    for size in [0, 4] {
        let mut extractor = CaptionExtractor::new(
            &pool,
            VideoCoding::Nal(Codec::H264, Framing::Lengths(size)),
            (1, 90_000),
        );
        let packet = VideoPacket {
            data: &access_unit,
            pts: Some(0),
            dts: None,
            duration: None,
        };
        extractor
            .push(packet, &mut WorkBudget::new(1 << 10))
            .expect("push");
        extractor.finish().expect("finish");
        assert_eq!(extractor.pop().expect("pop"), None);
    }
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

/// Pushes `stream` in transport-sized pieces without popping, then finishes
/// and pops; the work spent and the frames.
fn extract_unpopped(stream: &[u8]) -> (u64, usize) {
    let pool = Budget::new(1 << 26).expect("pool");
    let mut extractor = CaptionExtractor::new(&pool, VideoCoding::Mpeg2Raw, (1, 90_000));
    let mut work = WorkBudget::new(u64::MAX);
    for piece in stream.chunks(184) {
        let packet = VideoPacket {
            data: piece,
            pts: None,
            dts: None,
            duration: None,
        };
        extractor.push(packet, &mut work).expect("push");
    }
    extractor.finish().expect("finish");
    let mut frames = 0;
    while extractor.pop().expect("pop").is_some() {
        frames += 1;
    }
    (work.spent(), frames)
}

#[test]
fn caption_extraction_work_grows_linearly_with_the_stream() {
    // Many small captioned pictures waiting in the output, and one picture
    // whose slices arrive over many pushes.
    let many = |pictures: usize| {
        let mut stream = MPEG2_SEQUENCE.to_vec();
        for n in 0..pictures {
            stream.extend(mpeg2_picture(
                if n == 0 { 1 } else { 2 },
                0x80 | n as u8,
                64,
            ));
        }
        stream
    };
    let open = |slice: usize| {
        let mut stream = MPEG2_SEQUENCE.to_vec();
        stream.extend(mpeg2_picture(1, 0x41, slice));
        stream
    };
    let k = 8;
    for (small, large, frames) in [
        (many(500), many(500 * k), (500, 500 * k)),
        (open(1 << 16), open(k << 16), (1, 1)),
    ] {
        let (work, n_frames) = extract_unpopped(&small);
        let (work_k, k_frames) = extract_unpopped(&large);
        assert_eq!((n_frames, k_frames), frames);
        let ratio = work_k as f64 / work as f64;
        assert!(
            (0.9 * k as f64..=1.1 * k as f64).contains(&ratio),
            "work {work} for n, {work_k} for {k}n"
        );
    }
}
