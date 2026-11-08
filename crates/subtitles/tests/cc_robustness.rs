//! Malformed caption input never panics or hangs: random and mutated bytes
//! through the receiver, the SCC reader and the video extraction.

use std::fs;
use std::path::PathBuf;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::cc::video::nal::{Codec, Framing};
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
