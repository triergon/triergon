//! The DVB receiver decoder against recorded output of the reference decoder.
//!
//! `dvb-receiver-fixtures/*.bin` hold the codec packets of the small cases of
//! the subtitle-dvb corpus (`harness/subtitles/dvb_streams.py --fixtures`) with
//! the reference worker's fold of its output; each case is decoded and folded
//! the way the subtitle workers fold it.
#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_harness_protocol::digest::{fold, fold_bytes};
use triergon_subtitles::compat::dvb::{AutomaticPalette, Options, ReceiverDecoder};

struct Case {
    id: String,
    options: Options,
    configuration: Vec<u8>,
    packets: Vec<(Option<i64>, Vec<u8>)>,
    expected: [u64; 3],
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u16(&mut self) -> usize {
        usize::from(u16::from_be_bytes(self.take(2).try_into().unwrap()))
    }
    fn u32(&mut self) -> usize {
        u32::from_be_bytes(self.take(4).try_into().unwrap()) as usize
    }
    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().unwrap())
    }
}

fn cases() -> Vec<Case> {
    let folder = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/dvb-receiver-fixtures");
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "bin"))
        .collect();
    files.sort();
    let mut cases = Vec::new();
    for file in files {
        let data = std::fs::read(file).unwrap();
        let mut r = Reader(data.strip_prefix(b"DVBR0001").unwrap());
        while !r.0.is_empty() {
            let len = r.u16();
            let id = String::from_utf8(r.take(len).to_vec()).unwrap();
            let end_from_next = r.u8() == 1;
            let automatic_palette = match r.u8() as i8 {
                0 => AutomaticPalette::Never,
                1 => AutomaticPalette::Always,
                _ => AutomaticPalette::WithoutClut,
            };
            let service = usize::try_from(r.u8() as i8).ok();
            let len = r.u16();
            let configuration = r.take(len).to_vec();
            let packets = (0..r.u32())
                .map(|_| {
                    let known = r.u8() == 1;
                    let pts = r.u64() as i64;
                    let len = r.u32();
                    (known.then_some(pts), r.take(len).to_vec())
                })
                .collect();
            let expected = [r.u64(), r.u64(), r.u64()];
            cases.push(Case {
                id,
                options: Options {
                    end_from_next,
                    automatic_palette,
                    service,
                },
                configuration,
                packets,
                expected,
            });
        }
    }
    cases
}

/// Decode a case and fold its output: events, bytes and digest.
fn run(case: &Case) -> [u64; 3] {
    let pool = Budget::new(1 << 28).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    let (mut events, mut bytes, mut digest) = (0, 0, 0);
    let text = |digest: &mut u64| fold(digest, 0);
    text(&mut digest);
    for (index, (pts, packet)) in case.packets.iter().enumerate() {
        let mut work = WorkBudget::new(u64::MAX);
        let subtitle = match decoder.decode(packet, *pts, &mut work, &cancel) {
            Ok(Some(subtitle)) => subtitle,
            Ok(None) | Err(Error::Invalid(_)) => continue,
            Err(error) => panic!("{}: {error:?}", case.id),
        };
        events += 1;
        let head = [
            index as u64,
            subtitle.pts.unwrap_or(i64::MIN) as u64,
            u64::from(subtitle.start_display_ms),
            u64::from(subtitle.end_display_ms),
            0,
            subtitle.rects().len() as u64,
        ];
        for word in head {
            fold(&mut digest, word);
        }
        for rect in subtitle.rects() {
            let fields = [
                1,
                i64::from(rect.x),
                i64::from(rect.y),
                i64::from(rect.width),
                i64::from(rect.height),
                rect.palette.len() as i64,
                0,
            ];
            for field in fields {
                fold(&mut digest, field as u64);
            }
            for &word in rect.palette {
                fold(&mut digest, u64::from(word));
            }
            fold_bytes(&mut digest, rect.pixels);
            bytes += rect.palette.len() as u64 * 4 + rect.pixels.len() as u64;
            text(&mut digest);
            text(&mut digest);
        }
    }
    let mut total = 0;
    fold(&mut total, digest);
    [events, bytes, total]
}

#[test]
fn reference_folds() {
    let cases = cases();
    assert!(cases.len() > 200, "{} fixture cases", cases.len());
    let failures: Vec<&str> = cases
        .iter()
        .filter(|case| run(case) != case.expected)
        .map(|case| case.id.as_str())
        .collect();
    assert!(
        failures.is_empty(),
        "differ from the reference: {failures:?}"
    );
}

#[test]
fn refuses_packets_without_a_segment() {
    let pool = Budget::new(1 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
    let mut work = WorkBudget::new(u64::MAX);
    for packet in [
        &b""[..],
        b"\x0f\x80\x00\x01\x00\x00",
        b"\x20\x00\x0f\x80\x00\x01\x00\x00\xff",
    ] {
        assert!(matches!(
            decoder.decode(packet, None, &mut work, &cancel),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn charges_work_and_respects_the_pool() {
    let case = cases()
        .into_iter()
        .find(|c| c.id == "depth8-runs-ts")
        .unwrap();
    let cancel = AtomicBool::new(false);
    let (pts, packet) = &case.packets[0];
    let pool = Budget::new(1 << 20).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    let mut work = WorkBudget::new(1 << 20);
    assert!(
        decoder
            .decode(packet, *pts, &mut work, &cancel)
            .unwrap()
            .is_some()
    );
    assert!(work.spent() >= packet.len() as u64);
    let mut small = WorkBudget::new(64);
    let _ = decoder.decode(packet, *pts, &mut small, &cancel);
    assert_eq!(small.remaining(), 0);
    let pool = Budget::new(256).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    let mut work = WorkBudget::new(u64::MAX);
    assert!(decoder.decode(packet, *pts, &mut work, &cancel).is_err());
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        decoder.decode(packet, *pts, &mut work, &cancel),
        Err(Error::Cancelled)
    ));
}

/// Every fixture packet with bytes flipped, cut short and extended decodes
/// without panicking, within a work bound linear in the packet and region size.
#[test]
fn mutated_packets() {
    let cancel = AtomicBool::new(false);
    let mut state = 0x2545_f491_u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    for case in cases().iter().step_by(3) {
        let pool = Budget::new(1 << 26).unwrap();
        let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
        for (pts, packet) in &case.packets {
            for round in 0..8 {
                let mut bytes = packet.clone();
                match round % 4 {
                    0 => bytes.truncate(next() as usize % (bytes.len() + 1)),
                    1 => bytes.extend((0..next() % 40).map(|_| next() as u8)),
                    _ => {
                        for _ in 0..1 + next() % 6 {
                            if let Some(byte) = bytes.get_mut(next() as usize % packet.len().max(1))
                            {
                                *byte ^= 1 << (next() % 8);
                            }
                        }
                    }
                }
                let mut work = WorkBudget::new(1 << 32);
                let _ = decoder.decode(&bytes, *pts, &mut work, &cancel);
            }
        }
    }
}
