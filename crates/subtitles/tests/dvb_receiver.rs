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

/// Every pixel value of a region recomposed at a smaller depth has a colour,
/// as in the reference's 256-entry palettes: zero past the CLUT's entries,
/// and a level of its own under the automatic palette.
#[test]
fn reduced_depth_pixels_have_colours() {
    let cases: Vec<Case> = cases()
        .into_iter()
        .filter(|c| c.id.starts_with("depth-reduced-unfilled"))
        .collect();
    assert_eq!(cases.len(), 2);
    for case in &cases {
        let pool = Budget::new(1 << 24).unwrap();
        let cancel = AtomicBool::new(false);
        let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
        let mut last = None;
        for (pts, packet) in &case.packets {
            let mut work = WorkBudget::new(u64::MAX);
            if let Some(subtitle) = decoder.decode(packet, *pts, &mut work, &cancel).unwrap() {
                let rect = subtitle.rects().next().unwrap();
                last = Some((rect.palette.to_vec(), *rect.colours, rect.pixels.to_vec()));
            }
        }
        let (palette, colours, pixels) = last.unwrap();
        assert_eq!(palette, colours[..4]);
        let mut values: Vec<u8> = pixels.clone();
        values.sort_unstable();
        values.dedup();
        assert!(values.iter().any(|&p| p >= 4), "{}", case.id);
        if case.options.automatic_palette == AutomaticPalette::Always {
            let mut levels: Vec<u32> = values.iter().map(|&p| colours[usize::from(p)]).collect();
            levels.sort_unstable();
            levels.dedup();
            assert_eq!(levels.len(), values.len(), "{}", case.id);
        } else {
            assert!(colours[4..].iter().all(|&c| c == 0), "{}", case.id);
        }
    }
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

/// Authored segments of page 1.
mod segments {
    fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x0f, kind, 0, 1];
        bytes.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    /// A page composition placing `regions` as (id, x, y).
    pub fn page(version: u8, acquisition: bool, regions: &[(u8, u16, u16)]) -> Vec<u8> {
        let mut body = vec![5, version << 4 | u8::from(acquisition) << 2];
        for &(id, x, y) in regions {
            body.extend([id, 0]);
            body.extend(x.to_be_bytes());
            body.extend(y.to_be_bytes());
        }
        segment(0x10, &body)
    }

    /// A 2-bit region composition without fill, placing `objects` as (id, x, y).
    pub fn region(id: u8, width: u16, height: u16, objects: &[(u16, u16, u16)]) -> Vec<u8> {
        let mut body = vec![id, 0];
        body.extend(width.to_be_bytes());
        body.extend(height.to_be_bytes());
        body.extend([1 << 5 | 1 << 2, 0, 0, 0]);
        for &(object, x, y) in objects {
            body.extend(object.to_be_bytes());
            body.extend(x.to_be_bytes());
            body.extend(y.to_be_bytes());
        }
        segment(0x11, &body)
    }

    /// Object `id`: pixels 1, 2, 3, 1 on each line of both fields.
    pub fn object(id: u16) -> Vec<u8> {
        let field = [0x10, 0x6d, 0x00, 0xf0];
        let mut body = id.to_be_bytes().to_vec();
        body.push(0);
        body.extend(4_u16.to_be_bytes());
        body.extend(0_u16.to_be_bytes());
        body.extend(field);
        segment(0x13, &body)
    }

    pub fn end() -> Vec<u8> {
        segment(0x80, &[])
    }
}

fn packet(segments: &[Vec<u8>]) -> Vec<u8> {
    segments.concat()
}

/// The work of drawing one object at `placements` places in a region grows
/// with the placements, not faster.
#[test]
fn work_scales_with_placements() {
    let spent = |placements: usize| {
        let objects = vec![(1, 0, 0); placements];
        let packet = packet(&[
            segments::page(0, false, &[(0, 0, 0)]),
            segments::region(0, 128, 64, &objects),
            segments::object(1),
            segments::end(),
        ]);
        let pool = Budget::new(1 << 22).unwrap();
        let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
        let mut work = WorkBudget::new(u64::MAX);
        let presented = decoder
            .decode(&packet, None, &mut work, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(presented.unwrap().rects().len(), 1);
        work.spent() as f64
    };
    let (n, k) = (1000, 4);
    let ratio = spent(k * n) / spent(n);
    assert!(
        (0.95 * k as f64..=1.05 * k as f64).contains(&ratio),
        "work ratio {ratio} for {k} times the placements"
    );
}

type Shown = Vec<(i32, i32, u16, u16, Vec<u32>, Vec<u8>)>;

/// Decode `packets` in order with a pool of `limit` bytes: each packet's
/// rectangles, or its error, and the bytes the decoder then holds.
fn decode_all(
    limit: usize,
    options: Options,
    packets: &[&[u8]],
) -> (Vec<Result<Shown, Error>>, usize) {
    let pool = Budget::new(limit).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &[], options);
    let outcomes = packets
        .iter()
        .map(|packet| {
            let mut work = WorkBudget::new(u64::MAX);
            decoder
                .decode(packet, None, &mut work, &cancel)
                .map(|subtitle| {
                    subtitle.map_or_else(Vec::new, |subtitle| {
                        subtitle
                            .rects()
                            .map(|r| {
                                let palette = r.palette.to_vec();
                                (r.x, r.y, r.width, r.height, palette, r.pixels.to_vec())
                            })
                            .collect()
                    })
                })
        })
        .collect();
    (outcomes, pool.used())
}

/// A packet refused for want of memory changes nothing: the packets after it
/// decode as if it had never been sent.
#[test]
fn refused_packet_changes_nothing() {
    let drawn = packet(&[
        segments::page(0, false, &[(0, 10, 10)]),
        segments::region(0, 640, 64, &[(1, 0, 0)]),
        segments::object(1),
        segments::end(),
    ]);
    let present = packet(&[segments::end(), vec![0xff]]);
    let moved = packet(&[segments::page(1, false, &[(0, 50, 60)]), segments::end()]);
    // A region grown past the pool.
    let grown = packet(&[segments::region(0, 1280, 1024, &[(1, 0, 0)])]);
    // A new epoch whose page first presents the previous one, whose pixels
    // must then be copied out of the regions.
    let reset = packet(&[segments::page(1, true, &[(0, 10, 10)])]);
    let next = Options {
        end_from_next: true,
        ..Options::default()
    };
    for (options, refused, after, room) in [
        (Options::default(), &grown, &present, 1280 * 1024 / 2),
        (next, &reset, &moved, 640 * 64 / 2),
    ] {
        let (_, used) = decode_all(1 << 24, options, &[&drawn]);
        let limit = used + room;
        let (outcomes, _) = decode_all(limit, options, &[&drawn, refused, after]);
        let (expected, _) = decode_all(limit, options, &[&drawn, after]);
        assert!(outcomes[0].is_ok() && outcomes[1].is_err(), "{outcomes:?}");
        assert_eq!(outcomes[2], expected[1]);
        assert!(!expected[1].as_ref().unwrap().is_empty());
    }
}
