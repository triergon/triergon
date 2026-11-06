//! Letterbox output against digests captured from the reference converter
//! (the same pseudo-random pictures through FFmpeg 9.0.2's `scale,pad,format=rgba`
//! chain).
use triergon_pixel::letterbox::{CANVAS_BYTES, Scaler};
use triergon_pixel::{Colour, PictureView, Plane};

const FIXTURES: &str = include_str!("fixtures/rgba-reference-digests.txt");

/// The fixture generator's LCG (Knuth MMIX constants, top bits).
fn plane(state: &mut u64, n: usize, bits: u8) -> Vec<u16> {
    (0..n)
        .map(|_| {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((*state >> 33) & ((1 << bits) - 1)) as u16
        })
        .collect()
}

fn digest(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0_u64, |h, &b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
}

fn letterbox(
    w: usize,
    h: usize,
    bits: u8,
    colour: Colour,
    seed: u64,
    scaler: &mut Scaler,
) -> Vec<u8> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut state = seed;
    let y = plane(&mut state, w * h, bits);
    let u = plane(&mut state, cw * ch, bits);
    let v = plane(&mut state, cw * ch, bits);
    let mut rgba = vec![0_u8; CANVAS_BYTES];
    if bits == 8 {
        let narrow = |p: &[u16]| p.iter().map(|&s| s as u8).collect::<Vec<_>>();
        let (y, u, v) = (narrow(&y), narrow(&u), narrow(&v));
        let view = PictureView {
            width: w,
            height: h,
            bit_depth: 8,
            planes: [
                Plane::U8 {
                    data: &y,
                    stride: w,
                    offset: 0,
                },
                Plane::U8 {
                    data: &u,
                    stride: cw,
                    offset: 0,
                },
                Plane::U8 {
                    data: &v,
                    stride: cw,
                    offset: 0,
                },
            ],
            pts: None,
            sar: (1, 1),
            colour,
        };
        scaler.letterbox(&view, &mut rgba);
    } else {
        let view = PictureView {
            width: w,
            height: h,
            bit_depth: bits,
            planes: [
                Plane::U16 {
                    data: &y,
                    stride: w,
                    offset: 0,
                },
                Plane::U16 {
                    data: &u,
                    stride: cw,
                    offset: 0,
                },
                Plane::U16 {
                    data: &v,
                    stride: cw,
                    offset: 0,
                },
            ],
            pts: None,
            sar: (1, 1),
            colour,
        };
        scaler.letterbox(&view, &mut rgba);
    }
    rgba
}

#[test]
fn letterbox_matches_reference_digests() {
    let mut scaler = Scaler::default();
    let mut failures = Vec::new();
    for line in FIXTURES
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let f: Vec<u64> = line
            .split_whitespace()
            .map(|t| t.parse().expect("fixture field"))
            .collect();
        let [w, h, bits, matrix, full, seed, want] = f[..] else {
            panic!("fixture line {line}")
        };
        let colour = Colour {
            matrix: matrix as u8,
            full_range: full == 1,
        };
        let rgba = letterbox(
            w as usize,
            h as usize,
            bits as u8,
            colour,
            seed,
            &mut scaler,
        );
        if digest(&rgba) != want {
            failures.push(line.to_owned());
        }
    }
    assert!(
        failures.is_empty(),
        "differs from the reference: {failures:#?}"
    );
}

/// Harness hook (not a check): `RGBA_RAW="W H BITS MATRIX FULL IN OUT [CW CH LEFT TOP]"`
/// converts every 4:2:0 picture in IN (planar, 16-bit little-endian above 8
/// bits) to canvases in OUT, for comparison with the decode reference /
/// `probe_rgba` output. With the optional fields IN holds whole coded CW x CH
/// pictures and the W x H view starts at (LEFT, TOP), as the decoders hand it
/// over (see `codec::Plane`). `RGBA_BENCH=N` converts the first picture N
/// times instead and prints the median time.
#[test]
#[ignore = "needs RGBA_RAW input"]
fn raw_letterbox() {
    let Ok(spec) = std::env::var("RGBA_RAW") else {
        return;
    };
    let f: Vec<&str> = spec.split_whitespace().collect();
    let num = |i: usize| -> usize { f[i].parse().expect("RGBA_RAW number") };
    let (w, h, bits) = (num(0), num(1), num(2) as u8);
    let colour = Colour {
        matrix: num(3) as u8,
        full_range: f[4] == "1",
    };
    let (input, output) = (f[5], f[6]);
    let (cw, ch, left, top) = if f.len() > 7 {
        (num(7), num(8), num(9), num(10))
    } else {
        (w, h, 0, 0)
    };
    let data = std::fs::read(input).expect("input");
    let (ccw, cch) = (cw.div_ceil(2), ch.div_ceil(2));
    let samples = cw * ch + 2 * ccw * cch;
    let bytes = if bits > 8 { 2 } else { 1 };
    let mut scaler = Scaler::default();
    let mut rgba = vec![0_u8; CANVAS_BYTES];
    let mut out = Vec::new();
    let mut timings = Vec::new();
    let reps: usize = std::env::var("RGBA_BENCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let frames: Vec<&[u8]> = if reps > 0 {
        std::iter::repeat_n(&data[..samples * bytes], reps).collect()
    } else {
        data.chunks_exact(samples * bytes).collect()
    };
    let bounds = [
        (0, cw * ch, cw, top * cw + left),
        (cw * ch, ccw * cch, ccw, (top / 2) * ccw + left / 2),
    ];
    let bounds = [
        bounds[0],
        bounds[1],
        (bounds[1].0 + ccw * cch, ccw * cch, ccw, bounds[1].3),
    ];
    for frame in frames {
        let wide: Vec<u16> = frame
            .as_chunks::<2>()
            .0
            .iter()
            .take(if bits > 8 { samples } else { 0 })
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let planes = bounds.map(|(at, len, stride, offset)| {
            if bits > 8 {
                Plane::U16 {
                    data: &wide[at..at + len],
                    stride,
                    offset,
                }
            } else {
                Plane::U8 {
                    data: &frame[at..at + len],
                    stride,
                    offset,
                }
            }
        });
        let view = PictureView {
            width: w,
            height: h,
            bit_depth: bits,
            planes,
            pts: None,
            sar: (1, 1),
            colour,
        };
        let start = std::time::Instant::now();
        scaler.letterbox(&view, &mut rgba);
        timings.push(start.elapsed().as_nanos());
        if reps == 0 {
            out.extend_from_slice(&rgba);
        }
    }
    std::fs::write(output, out).expect("output");
    timings.sort_unstable();
    eprintln!(
        "frames {} median_ns {}",
        timings.len(),
        timings.get(timings.len() / 2).copied().unwrap_or(0)
    );
}
