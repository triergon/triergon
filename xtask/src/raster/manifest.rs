//! The rasterising features' manifests, generated. Glyph cases are a file of the
//! font corpus (`harness/fonts/corpus.json`) at one 26.6 pixel height with cases
//! over glyphs spread across its first face; line cases are generated files of
//! random outlines or random bitmaps (`.research/raster/inputs`). Every case's
//! parameters come from a xorshift generator seeded by the case's name, so the
//! rows are deterministic. The work each case must fold is pinned in
//! `harness/features/<id>/pins.json`, libass's folds (see [`crate::lanes`]).

use std::fmt::Write as _;
use std::path::Path;

use crate::json::Value;
use crate::lanes::Lane;

type R<T> = Result<T, String>;

/// The case files of every feature, relative to the root: corpus fonts under
/// `fonts/files` and generated inputs under `raster/inputs`.
const DIRECTORY: &str = ".research";
const FONT_FILES: &str = "fonts/files";
pub const INPUTS: &str = "raster/inputs";

/// The files case rows are generated from, besides the pins.
pub const ROW_SOURCES: [&str; 2] = ["harness/fonts/corpus.json", "xtask/src/raster/manifest.rs"];

/// 26.6 pixel heights: 13.33 and 37.5 pixels, which libass requests with hinting
/// on, and the 256 pixels it loads every glyph at by default.
const HEIGHTS: [i32; 3] = [853, 2400, 16384];

/// The required glyph cases: a file of each outline format and an authored one.
const REQUIRED_FONTS: [&str; 6] = [
    "Arimo-Regular.ttf",
    "NotoSans-Regular.otf",
    "NotoSansJP-Regular.otf",
    "NotoSansJP-VF.otf",
    "NotoSansCJK-Regular.ttc",
    "authored-cff-features.otf",
];

/// The cases `cargo xtask check` mutates: fonts small enough for the dev build
/// within the per-input timeout, and a file of each kind of line case.
pub const SMOKE_CASES: [(&str, &[&str]); 3] = [
    (
        "raster-glyphs",
        &["arimo-regular-ttf-853", "notosans-regular-otf-853"],
    ),
    ("raster-outlines", &["outlines-required-0"]),
    ("raster-bitmaps", &["bitmaps-required-0"]),
];

/// What a feature runs.
#[derive(Clone, Copy)]
enum Load {
    /// Glyph cases: the files and heights, and glyphs per case.
    Glyphs { all: bool, glyphs: usize },
    /// Files of random outline cases: how many, and outlines per file.
    Outlines { files: usize, outlines: usize },
    /// Files of random bitmap cases: how many, and case sets per file.
    Bitmaps { files: usize, sets: usize },
    /// Every corpus file at every height, and many files of both line kinds.
    Sweep,
    /// The required features' cases, against the host's SIMD engine.
    Simd,
}

/// How far generated transforms reach. The required features stay moderate:
/// rotations within 60 degrees, matrices near the target size and runs of small
/// random outlines only, so no case draws a bitmap of gigabytes. The sweep
/// reaches as far as libass draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    Moderate,
    Full,
}

/// The sweep's glyphs per case and files of each line kind.
const SWEEP_GLYPHS: usize = 30;
const SWEEP_OUTLINES: Load = Load::Outlines {
    files: 90,
    outlines: 50,
};
const SWEEP_BITMAPS: Load = Load::Bitmaps {
    files: 60,
    sets: 50,
};
const REQUIRED_OUTLINES: Load = Load::Outlines {
    files: 2,
    outlines: 100,
};
const REQUIRED_BITMAPS: Load = Load::Bitmaps { files: 2, sets: 40 };

pub const FEATURES: [&str; 5] = [
    "raster-glyphs",
    "raster-outlines",
    "raster-bitmaps",
    "raster-sweep",
    "raster-simd",
];

pub const LANE: Lane = Lane {
    command: "raster",
    features: &FEATURES,
    reference: "libass-native",
    pin_note: "libass 0.17.5's folds, recorded by `cargo xtask raster manifest --record`; never edited by hand.",
    report_directory: ".research/raster-report",
    manifest_directory: ".research/raster/manifests",
    row_sources: &ROW_SOURCES,
    rows,
    note,
    directory: |_| DIRECTORY,
    write_inputs,
    check_inputs: crate::lanes::no_checks,
    smoke: &SMOKE_CASES,
};

fn load(id: &str) -> R<Load> {
    match id {
        "raster-glyphs" => Ok(Load::Glyphs {
            all: false,
            glyphs: 8,
        }),
        "raster-outlines" => Ok(REQUIRED_OUTLINES),
        "raster-bitmaps" => Ok(REQUIRED_BITMAPS),
        "raster-sweep" => Ok(Load::Sweep),
        "raster-simd" => Ok(Load::Simd),
        _ => Err(format!("{id}: not a rasterising feature")),
    }
}

fn note(id: &str) -> &'static str {
    match id {
        "raster-glyphs" => {
            "A corpus file of each outline format and an authored one at 13.33 and 256 pixels: eight glyphs each converted, rotated, stroked, transformed, rasterised and drawn in runs, with moderate transforms."
        }
        "raster-outlines" => {
            "Two files of 100 seeded random outlines each (valid, degenerate, malformed and out of range) converted, rotated, stroked, transformed and rasterised, the small ones also drawn in runs, with moderate transforms."
        }
        "raster-bitmaps" => {
            "Two files of 40 sets each of seeded random bitmaps blurred, added, multiplied, subtracted, shifted and clipped."
        }
        "raster-simd" => {
            "The cases of raster-glyphs, raster-outlines and raster-bitmaps, with the reference on the host's SIMD engine and the pins of libass's C engine."
        }
        _ => {
            "Every corpus file at 13.33, 37.5 and 256 pixels with 30 glyphs each, 90 files of 50 random outlines and 60 files of 50 random bitmap sets, with transforms as far as libass draws them."
        }
    }
}

// ------------------------------------------------------------ generation

/// xorshift64, seeded.
struct Rng(u64);

impl Rng {
    /// The generator of one named input.
    fn named(name: &str) -> Self {
        let mut seed = 0xcbf2_9ce4_8422_2325_u64;
        for b in name.bytes() {
            seed = (seed ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn real(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

fn hex(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

fn hexes(values: &[f64]) -> String {
    values.iter().map(|&v| hex(v)).collect::<Vec<_>>().join(" ")
}

const EMPTY_OUTLINE: &str = "O 0 0";

/// A random font-like outline: 1 to 4 contours of lines, quadratic runs and
/// cubic pairs with coordinates up to `range`, repeated points and tiny
/// contours; one in eight has a malformed kind, contour order or coordinate.
fn random_outline(rng: &mut Rng, range: i64) -> String {
    let malformed = rng.chance(0.125);
    let contours = rng.range(1, 4);
    let mut ends = Vec::new();
    let mut points: Vec<(i64, i64, u8)> = Vec::new();
    for _ in 0..contours {
        let (cx, cy) = (rng.range(-range, range), rng.range(-range, range));
        let spread = rng.range(1, range.max(1));
        let start = points.len();
        let point = |rng: &mut Rng, points: &mut Vec<(i64, i64, u8)>, kind: u8| {
            let repeat = rng.chance(0.08) && points.len() > start;
            let (x, y) = match points.last() {
                Some(&(px, py, _)) if repeat => (px, py),
                _ => (
                    (cx + rng.range(-spread, spread)).clamp(-range, range),
                    (cy + rng.range(-spread, spread)).clamp(-range, range),
                ),
            };
            points.push((x, y, kind));
        };
        point(rng, &mut points, 1);
        let pieces = if rng.chance(0.1) { 1 } else { rng.range(1, 10) };
        for _ in 0..pieces {
            match rng.below(4) {
                0 | 1 => point(rng, &mut points, 1),
                2 => {
                    for _ in 0..rng.range(1, 3) {
                        point(rng, &mut points, 0);
                    }
                    point(rng, &mut points, 1);
                }
                _ => {
                    point(rng, &mut points, 2);
                    point(rng, &mut points, 2);
                    point(rng, &mut points, 1);
                }
            }
        }
        if rng.chance(0.2) {
            // Start on a control point; the contour then closes onto it.
            points[start].2 = 0;
        }
        ends.push(points.len());
    }
    if malformed {
        match rng.below(3) {
            0 if ends.len() > 1 => ends.swap(0, 1),
            1 => {
                let at = rng.below(points.len() as u64) as usize;
                points[at].2 = 2;
            }
            _ => {
                let at = rng.below(points.len() as u64) as usize;
                points[at].0 = if rng.chance(0.5) { 1 << 28 } else { -(1 << 29) };
            }
        }
    }
    let mut text = format!("O {}", ends.len());
    for e in &ends {
        write!(text, " {e}").ok();
    }
    write!(text, " {}", points.len()).ok();
    for (x, y, kind) in &points {
        write!(text, " {x} {y} {kind}").ok();
    }
    text
}

/// A random affine or perspective matrix that keeps an outline of `extent`
/// within about `target` units, and occasionally one that does not.
fn random_matrix(
    rng: &mut Rng,
    extent: f64,
    target: f64,
    perspective: bool,
    reach: Reach,
) -> [f64; 9] {
    let outsize = reach == Reach::Full && rng.chance(0.05);
    let k = target / extent.max(1.0) * if outsize { 1e4 } else { 1.0 };
    let angle = if rng.chance(0.3) {
        0.0
    } else {
        rng.real(-3.2, 3.2)
    };
    let (sx, sy) = (k * rng.real(0.05, 1.0), k * rng.real(0.05, 1.0));
    let shear = if rng.chance(0.5) {
        0.0
    } else {
        rng.real(-1.0, 1.0)
    };
    let (c, s) = (angle.cos(), angle.sin());
    let mut m = [
        [c * sx, (shear * c - s) * sy, rng.real(-target, target)],
        [s * sx, (shear * s + c) * sy, rng.real(-target, target)],
        [0.0, 0.0, 1.0],
    ];
    if perspective {
        let depth = if reach == Reach::Full && rng.chance(0.1) {
            2.0
        } else {
            0.25
        };
        m[2][0] = rng.real(-depth, depth) / extent.max(1.0);
        m[2][1] = rng.real(-depth, depth) / extent.max(1.0);
        m[2][2] = rng.real(0.5, 1.5);
        let depth = m[2][2];
        for row in &mut m[..2] {
            for v in row.iter_mut() {
                *v *= depth;
            }
        }
    }
    let [a, b, c] = m;
    [a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]
}

/// Conversion, rotation, stroking, a transform and a rasterisation of `source`,
/// whose coordinates reach about `extent`; the rasterisation sometimes unites it
/// with one of `others`.
fn outline_cases(
    rng: &mut Rng,
    source: &str,
    extent: f64,
    others: &[String],
    reach: Reach,
) -> Vec<String> {
    let mut cases = vec![format!("convert {source}")];
    let offset = |rng: &mut Rng| {
        if rng.chance(0.1) {
            rng.range(-(1 << 28), 1 << 28)
        } else {
            rng.range(-(1 << 16), 1 << 16)
        }
    };
    let (x, y) = (offset(rng), offset(rng));
    cases.push(format!("rotate {x} {y} {source}"));
    // Borders in stroker units, from below a pixel to many.
    let unit = (extent / 8.0).max(64.0);
    let eps = [16, 16, 16, 4, 1, 64][rng.below(6) as usize];
    let mut bx = (rng.real(0.0, 1.0).powi(2) * unit * 4.0) as i64;
    let mut by = if rng.chance(0.7) {
        bx
    } else {
        (rng.real(0.0, 1.0).powi(2) * unit * 4.0) as i64
    };
    if rng.chance(0.03) {
        (bx, by) = (rng.range(0, 1 << 29), rng.range(0, 1 << 29));
    }
    cases.push(format!("stroke {bx} {by} {eps} {source}"));
    for (kind, target) in [("transform", 2.0e4), ("raster", 1.5e4)] {
        let perspective = rng.chance(0.4);
        let m = hexes(&random_matrix(rng, extent, target, perspective, reach));
        let projection = if perspective { "p" } else { "a" };
        if kind == "transform" {
            cases.push(format!("transform {projection} {m} {source}"));
        } else {
            let second = if !others.is_empty() && rng.chance(0.3) {
                others[rng.below(others.len() as u64) as usize].as_str()
            } else {
                EMPTY_OUTLINE
            };
            cases.push(format!("raster {projection} {m} {source} {second}"));
        }
    }
    cases
}

/// A run of `glyphs` with random style, transform and filter, for outlines of
/// about `em` units.
fn run_case(rng: &mut Rng, glyphs: &[&str], em: f64, reach: Reach) -> String {
    let mut flags = u32::from(rng.chance(0.12))
        | u32::from(rng.chance(0.7)) << 1
        | u32::from(rng.chance(0.4)) << 2;
    for (bit, p) in [(8, 0.2), (16, 0.15), (32, 0.25)] {
        if rng.chance(p) {
            flags |= bit;
        }
    }
    let be = match rng.below(10) {
        0..=5 => 0,
        6..=8 => rng.range(1, 4),
        _ => rng.range(5, 20),
    };
    let blur = match rng.below(10) {
        0..=4 => 0.0,
        5..=7 => rng.real(0.05, 2.0),
        _ => rng.real(2.0, 12.0),
    };
    let scale = |rng: &mut Rng| {
        if rng.chance(0.7) {
            1.0
        } else {
            rng.real(0.5, 2.0)
        }
    };
    let reals = [
        blur,
        rng.real(-6.0, 6.0),
        rng.real(-6.0, 6.0),
        scale(rng),
        scale(rng),
        scale(rng),
        scale(rng),
        if rng.chance(0.8) {
            1.0
        } else {
            rng.real(0.75, 1.33)
        },
    ];
    let mut text = format!("run {flags} {be} {} {}", hexes(&reals), glyphs.len());
    let rotation = |rng: &mut Rng| match rng.below(10) {
        0..=5 => 0.0,
        6..=8 => rng.real(-60.0, 60.0),
        _ if reach == Reach::Moderate => rng.real(-60.0, 60.0),
        _ => rng.real(-360.0, 360.0),
    };
    let (frx, fry, frz) = (rotation(rng), rotation(rng), rotation(rng));
    let shear = |rng: &mut Rng| {
        if rng.chance(0.7) {
            0.0
        } else {
            rng.real(-1.0, 1.0)
        }
    };
    let (fax, fay) = (shear(rng), shear(rng));
    let (scale_x, scale_y) = (scale(rng), scale(rng));
    let border = |rng: &mut Rng| {
        if rng.chance(0.15) {
            0.0
        } else {
            rng.real(0.0, 6.0)
        }
    };
    let (bord_x, bord_y) = (border(rng), border(rng));
    let scale_fix = if rng.chance(0.8) {
        1.0
    } else {
        rng.real(0.5, 2.0)
    };
    let mut pen = rng.range(0, 1600 * 64);
    let line = rng.range(0, 1000 * 64);
    let origin = (
        pen + rng.range(-200 * 64, 200 * 64),
        line + rng.range(-100 * 64, 100 * 64),
    );
    let (asc, desc) = ((em * 0.8) as i64, (em * 0.25) as i64);
    for glyph in glyphs {
        let advance = (em * rng.real(0.3, 0.8)) as i64;
        let spacing = if rng.chance(0.8) {
            0
        } else {
            rng.range(-128, 256)
        };
        let glyph_reals = [
            frx, fry, frz, fax, fay, scale_x, scale_y, scale_x, scale_y, 0.0, 0.0, bord_x, bord_y,
            scale_fix,
        ];
        let ints = [
            pen - origin.0,
            line - origin.1,
            asc,
            desc,
            pen,
            line,
            spacing,
            advance,
        ];
        let ints: Vec<String> = ints.iter().map(ToString::to_string).collect();
        write!(text, " {} {} {glyph}", hexes(&glyph_reals), ints.join(" ")).ok();
        pen += advance + spacing;
    }
    text
}

/// One set of bitmap cases: a blur, an addition, both multiplications, a
/// subtraction, a shift and a clip.
fn bitmap_cases(rng: &mut Rng) -> Vec<String> {
    let mut cases = Vec::new();
    let seed = rng.range(0, i64::MAX >> 1);
    let (w, h) = (rng.range(2, 160), rng.range(2, 160));
    let radius = |rng: &mut Rng| {
        if rng.chance(0.2) {
            0.0
        } else {
            rng.real(0.0, 1.0).powi(2) * 30.0
        }
    };
    // One blur in ten reaches beyond libass's `\blur` clamp, so far that the
    // blurred bitmap would pass libass's bound of 2^29 samples and stays
    // unblurred.
    let (rx, ry) = if rng.chance(0.1) {
        let r = rng.real(2e4, 1e6);
        (r, r * rng.real(1.0, 2.0))
    } else {
        let rx = radius(rng);
        (rx, if rng.chance(0.6) { rx } else { radius(rng) })
    };
    // The last tenth beyond `\be`'s clamp of 127 passes.
    let be = match rng.below(10) {
        0..=4 => 0,
        5..=7 => rng.range(1, 6),
        8 => rng.range(7, 40),
        _ => rng.range(128, 255),
    };
    cases.push(format!(
        "blur {w} {h} {seed} {} {} {be}",
        hex(rx * rx),
        hex(ry * ry)
    ));
    let (w1, h1) = (rng.range(1, 100), rng.range(1, 100));
    let (w2, h2) = (rng.range(1, w1), rng.range(1, h1));
    let (x, y) = (rng.range(0, w1 - w2), rng.range(0, h1 - h2));
    cases.push(format!("add {seed} {w1} {h1} {w2} {h2} {x} {y}"));
    cases.push(format!("mul {seed} {w1} {h1}"));
    cases.push(format!("imul {seed} {w1} {h1}"));
    let (l1, t1) = (rng.range(-40, 40), rng.range(-40, 40));
    let (l2, t2) = (rng.range(-40, 40), rng.range(-40, 40));
    cases.push(format!(
        "fix {seed} {w1} {h1} {l1} {t1} {w2} {h2} {l2} {t2}"
    ));
    let (sx, sy) = (rng.range(0, 63), rng.range(0, 63));
    cases.push(format!("shift {seed} {w1} {h1} {sx} {sy}"));
    let (fw, fh) = (rng.range(0, 400), rng.range(0, 400));
    let (w, h) = (rng.range(1, 64), rng.range(1, 64));
    let (left, top) = (rng.range(-20, 20), rng.range(-20, 20));
    let (dx, dy) = (rng.range(-40, fw.max(1)), rng.range(-40, fh.max(1)));
    let (bx, by) = (dx + left, dy + top);
    let split = bx + rng.range(-10, w + 10);
    let (x0, y0) = (bx + rng.range(-20, w), by + rng.range(-20, h));
    let (x1, y1) = (x0 + rng.range(-5, w + 20), y0 + rng.range(-5, h + 20));
    let inverse = rng.below(2);
    let (zx, zy) = (rng.range(-10, 40), rng.range(-10, 40));
    let (cx, cy) = (zx + rng.range(0, fw.max(1)), zy + rng.range(0, fh.max(1)));
    cases.push(format!(
        "clip {seed} {w} {h} {left} {top} {dx} {dy} {split} {x0} {y0} {x1} {y1} {inverse} {fw} {fh} {zx} {zy} {cx} {cy}"
    ));
    cases
}

/// The cases of one glyph case: per glyph its outline cases and a run of it
/// and the glyphs after it, over `count` glyphs of about `em` units.
fn glyph_cases(name: &str, count: usize, em: f64, reach: Reach) -> Vec<String> {
    let mut rng = Rng::named(name);
    let sources: Vec<String> = (0..count).map(|k| format!("g {k}")).collect();
    let mut cases = Vec::new();
    for (k, source) in sources.iter().enumerate() {
        cases.extend(outline_cases(&mut rng, source, em, &sources, reach));
        let length = 1 + rng.below(3) as usize;
        let run: Vec<&str> = (0..length)
            .map(|i| sources[(k + i) % count].as_str())
            .collect();
        cases.push(run_case(&mut rng, &run, em, reach));
    }
    cases
}

/// A file of random outline cases.
fn outline_file(name: &str, outlines: usize, reach: Reach) -> String {
    let mut rng = Rng::named(name);
    let mut lines = Vec::new();
    for _ in 0..outlines {
        let range = match rng.below(8) {
            0 => 64,
            1 => 1000,
            2..=4 => 20_000,
            5 => 1 << 20,
            6 => (1 << 28) - 1,
            _ => 1 << 28,
        };
        let source = random_outline(&mut rng, range);
        lines.extend(outline_cases(&mut rng, &source, range as f64, &[], reach));
        // Runs of large outlines in perspective can reach bitmaps of gigabytes.
        let run_range = if reach == Reach::Full { 20_000 } else { 1000 };
        if range <= run_range && rng.chance(0.5) {
            lines.push(run_case(&mut rng, &[&source], range as f64, reach));
        }
    }
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// A file of random bitmap cases.
fn bitmap_file(name: &str, sets: usize) -> String {
    let mut rng = Rng::named(name);
    (0..sets)
        .flat_map(|_| bitmap_cases(&mut rng))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// The line files of a load: name and contents.
fn line_files(load: Load, prefix: &str, reach: Reach) -> Vec<(String, String)> {
    match load {
        Load::Outlines { files, outlines } => (0..files)
            .map(|i| {
                let name = format!("outlines{prefix}-{i}");
                let text = outline_file(&name, outlines, reach);
                (name, text)
            })
            .collect(),
        Load::Bitmaps { files, sets } => (0..files)
            .map(|i| {
                let name = format!("bitmaps{prefix}-{i}");
                let text = bitmap_file(&name, sets);
                (name, text)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Every generated input file: name and contents.
fn inputs() -> Vec<(String, String)> {
    let mut files = line_files(REQUIRED_OUTLINES, "-required", Reach::Moderate);
    files.extend(line_files(REQUIRED_BITMAPS, "-required", Reach::Moderate));
    files.extend(line_files(SWEEP_OUTLINES, "", Reach::Full));
    files.extend(line_files(SWEEP_BITMAPS, "", Reach::Full));
    files
}

/// Writes the generated input files that differ from what is on disk.
pub fn write_inputs(root: &Path) -> R<()> {
    let directory = root.join(DIRECTORY).join(INPUTS);
    std::fs::create_dir_all(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    for (name, contents) in inputs() {
        let path = directory.join(format!("{name}.txt"));
        if std::fs::read(&path).ok().as_deref() != Some(contents.as_bytes()) {
            crate::fetch::publish(&path, contents.as_bytes())?;
        }
    }
    Ok(())
}

/// The glyph case rows: every corpus file the lane serves (or the required
/// ones), at every height (or the required two).
fn glyph_rows(root: &Path, all: bool, glyphs: usize) -> R<Vec<Value>> {
    let heights: &[i32] = if all { &HEIGHTS } else { &[853, 16384] };
    let mut rows = Vec::new();
    for (source, files) in crate::fonts::corpus(root)? {
        if !crate::fonts::serves(&source, "raster") {
            continue;
        }
        let rights = if source.get("generator").and_then(Value::as_str) == Some("authored-fonts") {
            vec![("rights", Value::str("authored-synthetic"))]
        } else {
            let licence = source
                .get("licence")
                .and_then(Value::as_str)
                .ok_or("corpus source without licence")?;
            vec![
                ("rights", Value::str("reviewed-licence")),
                ("licence", Value::str(licence)),
            ]
        };
        for file in files {
            let name = file.path.rsplit('/').next().unwrap_or(&file.path);
            if !all && !REQUIRED_FONTS.contains(&name) {
                continue;
            }
            let stem = name.replace('.', "-").to_lowercase();
            for &height in heights {
                let id = format!("{stem}-{height}");
                let mut arguments =
                    vec!["glyphs".to_owned(), height.to_string(), glyphs.to_string()];
                let reach = if all { Reach::Full } else { Reach::Moderate };
                arguments.extend(glyph_cases(&id, glyphs, f64::from(height), reach));
                let mut fields = vec![
                    ("id", Value::str(id)),
                    ("profile", Value::str(&file.format)),
                    ("file", Value::str(format!("{FONT_FILES}/{}", file.path))),
                    ("sha256", Value::str(&file.sha256)),
                ];
                fields.extend(rights.iter().cloned());
                fields.push(("arguments", Value::strings(arguments)));
                rows.push(Value::object(fields));
            }
        }
    }
    Ok(rows)
}

/// The rows of the line files of a load.
fn line_rows(load: Load, prefix: &str, profile: &str, reach: Reach) -> Vec<Value> {
    line_files(load, prefix, reach)
        .into_iter()
        .map(|(name, contents)| {
            Value::object([
                ("id", Value::str(&name)),
                ("profile", Value::str(profile)),
                ("file", Value::str(format!("{INPUTS}/{name}.txt"))),
                (
                    "sha256",
                    Value::str(crate::sha256::hex(contents.as_bytes())),
                ),
                ("rights", Value::str("authored-synthetic")),
                ("arguments", Value::strings(["lines"])),
            ])
        })
        .collect()
}

fn rows(root: &Path, id: &str) -> R<Vec<Value>> {
    Ok(match load(id)? {
        Load::Glyphs { all, glyphs } => glyph_rows(root, all, glyphs)?,
        load @ Load::Outlines { .. } => line_rows(load, "-required", "outlines", Reach::Moderate),
        load @ Load::Bitmaps { .. } => line_rows(load, "-required", "bitmaps", Reach::Moderate),
        Load::Simd => {
            let mut cases = rows(root, "raster-glyphs")?;
            cases.extend(rows(root, "raster-outlines")?);
            cases.extend(rows(root, "raster-bitmaps")?);
            cases
        }
        Load::Sweep => {
            let mut rows = glyph_rows(root, true, SWEEP_GLYPHS)?;
            rows.extend(line_rows(SWEEP_OUTLINES, "", "outlines", Reach::Full));
            rows.extend(line_rows(SWEEP_BITMAPS, "", "bitmaps", Reach::Full));
            rows
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_cases_are_deterministic_and_well_formed() {
        assert_eq!(
            glyph_cases("x", 4, 853.0, Reach::Full),
            glyph_cases("x", 4, 853.0, Reach::Full)
        );
        let mut rng = Rng(7);
        for _ in 0..100 {
            let source = random_outline(&mut rng, 1000);
            let fields: Vec<&str> = source.split(' ').collect();
            assert_eq!(fields[0], "O");
            let contours: usize = fields[1].parse().unwrap();
            let points: usize = fields[2 + contours].parse().unwrap();
            assert_eq!(fields.len(), 3 + contours + 3 * points);
        }
        let kinds = |text: &str| -> Vec<String> {
            let mut kinds: Vec<String> = text
                .lines()
                .map(|l| l.split(' ').next().unwrap().to_owned())
                .collect();
            kinds.sort();
            kinds.dedup();
            kinds
        };
        assert_eq!(
            kinds(&outline_file("outlines-required-0", 100, Reach::Moderate)),
            ["convert", "raster", "rotate", "run", "stroke", "transform"]
        );
        assert_eq!(
            kinds(&bitmap_file("bitmaps-required-0", 5)),
            ["add", "blur", "clip", "fix", "imul", "mul", "shift"]
        );
    }

    #[test]
    fn the_required_glyph_cases_cover_every_outline_format() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let rows = glyph_rows(root, false, 8).unwrap();
        let mut profiles: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("profile").and_then(Value::as_str))
            .collect();
        profiles.sort_unstable();
        profiles.dedup();
        assert_eq!(
            profiles,
            ["cff", "cff-cid", "cff2", "collection", "truetype"]
        );
        assert_eq!(rows.len(), 2 * REQUIRED_FONTS.len());
    }
}
