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
use triergon_subtitles::compat::dvb::{AutomaticPalette, Options, ReceiverDecoder, Status};

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

/// Decode a case, each call with `quantum` units of work, and fold its
/// output: events, bytes and digest. Also the calls that yielded.
fn run(case: &Case, quantum: u64) -> ([u64; 3], u64) {
    let pool = Budget::new(1 << 28).unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    let (mut events, mut bytes, mut digest, mut yields) = (0, 0, 0, 0);
    let text = |digest: &mut u64| fold(digest, 0);
    text(&mut digest);
    for (index, (pts, packet)) in case.packets.iter().enumerate() {
        let mut status = decoder.decode(packet, *pts, &mut WorkBudget::new(quantum), &cancel);
        while matches!(status, Ok(Status::Yield)) {
            yields += 1;
            status = decoder.resume(&mut WorkBudget::new(quantum), &cancel);
        }
        let subtitle = match status {
            Ok(Status::Subtitle(subtitle)) => subtitle,
            Ok(_) | Err(Error::Invalid(_)) => continue,
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
    ([events, bytes, total], yields)
}

#[test]
fn reference_folds() {
    let cases = cases();
    assert!(cases.len() > 200, "{} fixture cases", cases.len());
    let failures: Vec<&str> = cases
        .iter()
        .filter(|case| run(case, u64::MAX).0 != case.expected)
        .map(|case| case.id.as_str())
        .collect();
    assert!(
        failures.is_empty(),
        "differ from the reference: {failures:?}"
    );
}

/// Decoded a little work at a time, yielding and resumed, every case presents
/// what it presents with unlimited work.
#[test]
fn small_budgets_yield_to_the_same_output() {
    let cases = cases();
    for quantum in [1, 97, 4096] {
        let mut yields = 0;
        for case in &cases {
            let (fold, yielded) = run(case, quantum);
            assert_eq!(fold, case.expected, "{} at {quantum}", case.id);
            yields += yielded;
        }
        assert!(yields > 0, "nothing yielded at {quantum}");
    }
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
            let status = decoder.decode(packet, *pts, &mut work, &cancel).unwrap();
            if let Some(subtitle) = status.subtitle() {
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
    let status = decoder.decode(packet, *pts, &mut work, &cancel).unwrap();
    assert!(status.subtitle().is_some());
    let whole = work.spent();
    assert!(whole >= packet.len() as u64);
    // A small budget yields, spending it all; the calls together spend what
    // one call with enough work does.
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    let mut spent = 0;
    for call in 0..whole.div_ceil(64) - 1 {
        let mut small = WorkBudget::new(64);
        let status = if call == 0 {
            decoder.decode(packet, *pts, &mut small, &cancel)
        } else {
            decoder.resume(&mut small, &cancel)
        };
        assert!(matches!(status.unwrap(), Status::Yield));
        assert_eq!(small.remaining(), 0);
        spent += 64;
    }
    // A kept packet is resumed, not replaced.
    assert!(matches!(
        decoder.decode(packet, *pts, &mut WorkBudget::new(64), &cancel),
        Err(Error::Invalid(_))
    ));
    let mut last = WorkBudget::new(64);
    let status = decoder.resume(&mut last, &cancel).unwrap();
    assert!(status.subtitle().is_some());
    assert_eq!(spent + last.spent(), whole);
    assert!(matches!(
        decoder.resume(&mut last, &cancel),
        Err(Error::Invalid(_))
    ));
    let pool = Budget::new(256).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    let mut work = WorkBudget::new(u64::MAX);
    assert!(decoder.decode(packet, *pts, &mut work, &cancel).is_err());
    // A cancelled packet is kept, to be resumed or abandoned.
    let pool = Budget::new(1 << 20).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
    assert!(matches!(
        decoder.decode(packet, *pts, &mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    let status = decoder.resume(&mut work, &cancel).unwrap();
    assert!(status.subtitle().is_some());
    assert!(matches!(
        decoder.decode(packet, *pts, &mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    decoder.abandon();
    assert!(!decoder.has_kept_packet());
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
                let status = decoder.decode(&bytes, *pts, &mut work, &cancel);
                assert!(
                    !matches!(status, Ok(Status::Yield | Status::Stalled(_))),
                    "{}: {status:?}",
                    case.id
                );
                assert!(!decoder.has_kept_packet(), "{}", case.id);
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
        assert_eq!(presented.subtitle().unwrap().rects().len(), 1);
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
            // One time for all, so that a packet's end does not depend on
            // which came before it.
            decoder
                .decode(packet, Some(0), &mut work, &cancel)
                .map(|subtitle| {
                    subtitle.subtitle().map_or_else(Vec::new, |subtitle| {
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
    // A region shrunk, which fits, then a second region past the pool: the
    // first keeps its pixels.
    let shrunk_then_grown = packet(&[
        segments::region(0, 320, 64, &[(1, 0, 0)]),
        segments::region(1, 1280, 1024, &[(1, 0, 0)]),
    ]);
    let next = Options {
        end_from_next: true,
        ..Options::default()
    };
    for (options, refused, after, room) in [
        (Options::default(), &grown, &present, 1280 * 1024 / 2),
        (
            Options::default(),
            &shrunk_then_grown,
            &present,
            1280 * 1024 / 2,
        ),
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

/// A packet the pool could never hold is refused with `Limit`, one it cannot
/// hold now with `Backpressure`; neither is kept, and both pay for the check.
#[test]
fn refusals_pay_for_the_check_and_say_whether_to_retry() {
    let large = packet(&[
        segments::page(0, false, &[(0, 0, 0)]),
        segments::region(0, 1280, 1024, &[(1, 0, 0)]),
        segments::end(),
    ]);
    let cancel = AtomicBool::new(false);
    let pool = Budget::new(1 << 20).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
    let mut work = WorkBudget::new(u64::MAX);
    assert!(matches!(
        decoder.decode(&large, Some(0), &mut work, &cancel),
        Err(Error::Limit { .. })
    ));
    assert!(work.spent() >= large.len() as u64);
    assert!(!decoder.has_kept_packet());
    let pool = Budget::new(1 << 22).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
    let taken = pool.reserve(pool.available() - (1 << 20)).unwrap();
    let mut work = WorkBudget::new(u64::MAX);
    assert!(matches!(
        decoder.decode(&large, Some(0), &mut work, &cancel),
        Err(Error::Backpressure { .. })
    ));
    assert!(work.spent() >= large.len() as u64);
    drop(taken);
    // Once the pool has room, it is decoded.
    let status = decoder.decode(&large, Some(0), &mut WorkBudget::new(u64::MAX), &cancel);
    assert!(matches!(status, Ok(Status::Subtitle(_))));
}

/// With the end taken from the next page, times subtract as the reference's
/// do: in 64 bits, wrapping, rounded to milliseconds in 128; and a packet
/// without a time stands for `i64::MIN`. The ends come from the reference's
/// trace of `no-pts-between`.
#[test]
fn end_from_next_at_the_ends_of_the_time_range() {
    let drawn = packet(&[
        segments::page(0, false, &[(0, 10, 10)]),
        segments::region(0, 64, 4, &[(1, 0, 0)]),
        segments::object(1),
        segments::end(),
    ]);
    let next = |version| {
        packet(&[
            segments::page(version, false, &[(0, 10, 10)]),
            segments::end(),
        ])
    };
    let options = Options {
        end_from_next: true,
        ..Options::default()
    };
    let cancel = AtomicBool::new(false);
    let ends = |times: &[Option<i64>]| {
        let pool = Budget::new(1 << 20).unwrap();
        let mut decoder = ReceiverDecoder::new(&pool, &[], options);
        let mut work = WorkBudget::new(u64::MAX);
        times
            .iter()
            .enumerate()
            .map(|(i, &pts)| {
                let packet = if i == 0 { drawn.clone() } else { next(i as u8) };
                decoder
                    .decode(&packet, pts, &mut work, &cancel)
                    .unwrap()
                    .subtitle()
                    .map(|s| (s.pts, s.end_display_ms))
            })
            .collect::<Vec<_>>()
    };
    // 0 to i64::MAX microseconds is past 64 bits once rounded.
    let end = ((i128::from(i64::MAX) + 500) / 1000 - 1) as u32;
    assert_eq!(
        ends(&[Some(0), Some(i64::MAX)]),
        [None, Some((Some(0), end))]
    );
    // i64::MIN - i64::MAX wraps to 1 us, which rounds to 0 ms.
    assert_eq!(
        ends(&[Some(i64::MAX), Some(i64::MIN)]),
        [None, Some((Some(i64::MAX), u32::MAX))]
    );
    // A packet without a time ends the one before it from i64::MIN, and the
    // packet after it presents nothing.
    assert_eq!(
        ends(&[Some(1_000_000), None, Some(3_000_000), Some(4_000_000)]),
        [
            None,
            Some((Some(1_000_000), 2_783_137_807)),
            None,
            Some((Some(3_000_000), 999)),
        ]
    );
}

/// A pool holding `bytes` for someone else.
fn take(pool: &Budget, bytes: usize) -> triergon_core::Reservation {
    pool.reserve(bytes).unwrap()
}

/// A segment refused memory after its packet was admitted stalls the packet:
/// the decoder keeps it, and resumed once the pool has room it presents what
/// it presents decoded in one call.
#[test]
fn a_stalled_packet_resumes_to_the_same_output() {
    let drawn = packet(&[
        segments::page(0, false, &[(0, 10, 10), (1, 300, 10)]),
        segments::region(0, 640, 64, &[(1, 0, 0)]),
        segments::region(1, 320, 64, &[(1, 0, 0)]),
        segments::object(1),
        segments::end(),
    ]);
    let cancel = AtomicBool::new(false);
    let shown = |status: Status<'_>| -> Shown {
        status
            .subtitle()
            .unwrap()
            .rects()
            .map(|r| {
                (
                    r.x,
                    r.y,
                    r.width,
                    r.height,
                    r.palette.to_vec(),
                    r.pixels.to_vec(),
                )
            })
            .collect()
    };
    let pool = Budget::new(1 << 22).unwrap();
    let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
    let whole = decoder
        .decode(&drawn, Some(0), &mut WorkBudget::new(u64::MAX), &cancel)
        .map(shown)
        .unwrap();
    let mut stalls = 0;
    for quantum in (1..20_000).step_by(97) {
        let pool = Budget::new(1 << 22).unwrap();
        let mut decoder = ReceiverDecoder::new(&pool, &[], Options::default());
        let status = decoder.decode(&drawn, Some(0), &mut WorkBudget::new(quantum), &cancel);
        if !matches!(status, Ok(Status::Yield)) {
            continue;
        }
        assert!(decoder.keeps(&drawn, Some(0)) && !decoder.keeps(&drawn, Some(1)));
        // Someone else takes the pool while the packet waits.
        let taken = take(&pool, pool.available());
        let mut work = WorkBudget::new(u64::MAX);
        let status = decoder.resume(&mut work, &cancel).unwrap();
        if let Status::Stalled(error) = status {
            assert!(matches!(error, Error::Backpressure { .. }), "{error:?}");
            stalls += 1;
            // Still no room: stalled again, at the same segment.
            assert!(matches!(
                decoder.resume(&mut work, &cancel),
                Ok(Status::Stalled(_))
            ));
            drop(taken);
            let status = decoder.resume(&mut work, &cancel).unwrap();
            assert_eq!(shown(status), whole, "quantum {quantum}");
        } else {
            assert_eq!(shown(status), whole, "quantum {quantum}");
        }
        assert!(!decoder.has_kept_packet());
    }
    assert!(stalls > 0);
}

/// The memory a packet is checked for is at least what it takes: with the
/// pool holding exactly that, no fixture packet, nor one mutated, stalls.
#[test]
fn admitted_packets_never_stall() {
    let cancel = AtomicBool::new(false);
    let mut state = 0x9e37_79b9_u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut checked = 0;
    for case in &cases() {
        let pool = Budget::new(1 << 26).unwrap();
        let mut decoder = ReceiverDecoder::new(&pool, &case.configuration, case.options);
        for (pts, packet) in &case.packets {
            let mut variants = vec![packet.clone()];
            for _ in 0..2 {
                let mut bytes = packet.clone();
                for _ in 0..1 + next() % 4 {
                    if let Some(byte) = bytes.get_mut(next() as usize % packet.len().max(1)) {
                        *byte ^= 1 << (next() % 8);
                    }
                }
                variants.push(bytes);
            }
            // The packet itself last, to decode the case on.
            variants.rotate_left(1);
            for bytes in &variants {
                let mut work = WorkBudget::new(u64::MAX);
                // Give the packet no room, then each time the room it says it
                // lacks, until it is admitted: then it must not stall.
                let mut room = 0;
                let status = loop {
                    let taken = take(&pool, pool.available() - room);
                    let status = decoder.decode(bytes, *pts, &mut work, &cancel);
                    drop(taken);
                    match status {
                        Err(Error::Backpressure {
                            requested,
                            available,
                        }) => room += requested - available,
                        status => break status.map(|_| ()),
                    }
                };
                assert!(
                    !decoder.has_kept_packet()
                        && !matches!(status, Err(e) if !matches!(e, Error::Invalid(_))),
                    "{}: {room} bytes of room, {status:?}",
                    case.id
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 500, "{checked} packets checked");
}
