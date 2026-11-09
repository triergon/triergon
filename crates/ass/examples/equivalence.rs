//! Renders the `ass-render` fixtures and scripts made from them and from a
//! seed, frame by frame, and records what every frame gave, so two builds of
//! the renderer can be compared (`cargo xtask ass equivalence`).
//!
//! ```text
//! equivalence FONTS FIXTURES SEED RANDOM MUTATIONS JOBS OUTPUT
//! ```
//!
//! The cases are, for each fixture at 1920×1080 over 1280×720 storage and at
//! 1280×720 over 720×576: the fixture at its own frames, as the `ass-render`
//! workload renders it; the fixture every 40 ms; and MUTATIONS mutations of
//! it (override tags inserted, lines duplicated, bytes changed or removed,
//! text appended) with varied settings and style overrides. Then RANDOM
//! scripts generated from SEED: styles, events, override tags with edge
//! values, text in many scripts, drawings, karaoke, clips, `\t` and
//! scrolling. Each case's line of OUTPUT is followed by one line per frame:
//! its change, its images' digest (kind, place, size, colour and pixels),
//! and the work it spent with the `work-probe` feature, or its error; then
//! the peak of the case's budget.

#[path = "../../ass-worker/src/fixture.rs"]
#[allow(dead_code, reason = "the worker's directives; rendering reads some")]
mod fixture;

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use fixture::{Command, Fixture};
use triergon_ass::{
    CacheLimits, Change, FrameMargins, Image, Options, Renderer, Script, Settings, Size,
};
use triergon_core::{Budget, Bytes};

const USAGE: &str = "usage: equivalence FONTS FIXTURES SEED RANDOM MUTATIONS JOBS OUTPUT";

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [fonts, fixtures, seed, random, mutations, jobs, output] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let number = |text: &str| text.parse::<u64>().map_err(|_| USAGE.to_string());
    let fonts = Path::new(fonts);
    let mut rng = Rng(number(seed)?);
    let mut cases = fixture_cases(fonts, Path::new(fixtures), number(mutations)?, &mut rng)?;
    cases.extend(random_cases(fonts, number(random)?, &mut rng));
    let records: Vec<Mutex<String>> = cases.iter().map(|_| Mutex::default()).collect();
    let next = AtomicUsize::new(0);
    std::panic::set_hook(Box::new(|_| {}));
    std::thread::scope(|scope| {
        for _ in 0..number(jobs)?.max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(i) else { break };
                    let record = case.record();
                    if let Ok(mut slot) = records[i].lock() {
                        *slot = record;
                    }
                }
            });
        }
        Ok::<(), String>(())
    })?;
    let mut text = String::new();
    for record in &records {
        text.push_str(&record.lock().map_err(|e| e.to_string())?);
    }
    std::fs::write(output, text).map_err(|e| format!("{output}: {e}"))
}

/// A seeded generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }

    fn float(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() >> 11) as f64 / (1_u64 << 53) as f64 * (hi - lo)
    }
}

/// How a case's renderer is set up.
#[derive(Clone, Debug)]
struct Setup {
    frame: (u32, u32),
    storage: Option<(u32, u32)>,
    margins: FrameMargins,
    use_margins: bool,
    pixel_aspect: Option<f64>,
    font_scale: f64,
    line_spacing: f64,
    line_position: f64,
    /// Outlines, megabytes and font descriptions kept.
    cache: (usize, usize, usize),
    work: u64,
    style_overrides: Vec<Vec<u8>>,
}

/// The two sizes the `ass-render` workloads render at.
const SIZES: [((u32, u32), (u32, u32)); 2] =
    [((1920, 1080), (1280, 720)), ((1280, 720), (720, 576))];

impl Setup {
    /// A player's setup, as the `ass-render` workloads use.
    fn player((frame, storage): ((u32, u32), (u32, u32))) -> Self {
        Self {
            frame,
            storage: Some(storage),
            margins: FrameMargins::default(),
            use_margins: false,
            pixel_aspect: None,
            font_scale: 1.0,
            line_spacing: 0.0,
            line_position: 0.0,
            cache: (1000, 24, CacheLimits::FONTS),
            work: Settings::DEFAULT_WORK,
            style_overrides: Vec::new(),
        }
    }

    /// The player's setup with one thing varied, or none.
    fn varied(rng: &mut Rng, size: ((u32, u32), (u32, u32))) -> Self {
        let mut s = Self::player(size);
        if rng.chance(30) {
            match rng.below(9) {
                0 => {
                    s.margins = FrameMargins {
                        top: rng.range(0, 100) as i32,
                        bottom: rng.range(0, 100) as i32,
                        left: rng.range(0, 60) as i32,
                        right: rng.range(0, 60) as i32,
                    };
                    s.use_margins = rng.chance(50);
                }
                1 => s.pixel_aspect = Some(rng.pick(&[0.0, 1.0, 1.333, 0.75, -1.0])),
                2 => s.font_scale = rng.float(0.5, 2.0),
                3 => s.line_spacing = rng.float(-5.0, 20.0),
                4 => s.line_position = rng.float(0.0, 100.0),
                5 => s.cache = (rng.below(8), rng.below(3), rng.below(4)),
                6 => s.work = 1 << rng.range(8, 22),
                7 => s.storage = None,
                _ => {
                    s.margins.top = 40;
                    s.margins.bottom = 40;
                    s.use_margins = true;
                    s.pixel_aspect = Some(1.2);
                }
            }
        }
        if rng.chance(25) {
            // Justification apart from alignment, which only an override sets.
            let target = rng.pick(&["", "Default.", "Alt."]);
            let value = rng.range(0, 4);
            s.style_overrides
                .push(format!("{target}Justify={value}").into_bytes());
        }
        s
    }

    fn settings(&self) -> Settings {
        let mut settings = Settings::new(Size::new(self.frame.0, self.frame.1));
        settings.storage = self.storage.map(|(w, h)| Size::new(w, h));
        settings.margins = self.margins;
        settings.use_margins = self.use_margins;
        settings.pixel_aspect = self.pixel_aspect;
        settings.font_scale = self.font_scale;
        settings.line_spacing = self.line_spacing;
        settings.line_position = self.line_position;
        settings.cache = CacheLimits::new(self.cache.0, self.cache.1);
        settings.cache.fonts = self.cache.2;
        settings.work = self.work;
        settings
    }
}

/// One operation: a script or packet stream, its fonts, setup and times.
#[derive(Clone)]
struct Case {
    name: String,
    data: Vec<u8>,
    stream: bool,
    attach: Vec<PathBuf>,
    default_family: Option<Vec<u8>>,
    default_font: Option<PathBuf>,
    setup: Setup,
    /// A whole script's frame times; a stream names its own.
    times: Vec<i64>,
}

impl Case {
    /// The case's line, a line per frame, and the peak of its budget.
    fn record(&self) -> String {
        let mut out = format!("case {}\n", self.name);
        let budget = Budget::new(1 << 34).ok();
        let run = catch_unwind(AssertUnwindSafe(|| {
            if let Some(budget) = &budget
                && let Err(error) = self.render(budget, &mut out)
            {
                let _ = writeln!(out, "failed {error}");
            }
        }));
        if run.is_err() {
            out.push_str("panic\n");
        }
        let peak = budget.map_or(0, |b| b.peak());
        let _ = writeln!(out, "peak {peak}");
        out
    }

    fn render(&self, budget: &Budget, out: &mut String) -> Result<(), String> {
        let read =
            |path: &Path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
        let bytes = |data: &[u8]| Bytes::copy_from_slice(budget, data).map_err(|e| e.to_string());
        let mut attached = Vec::new();
        for path in &self.attach {
            attached.push(bytes(&read(path)?)?);
        }
        let default_font = match &self.default_font {
            // libass goes without a default font file it cannot read.
            Some(path) => read(path).ok().map(|data| bytes(&data)).transpose()?,
            None => None,
        };
        let fonts = triergon_ass::Fonts {
            attached,
            default_family: self.default_family.clone(),
            default_font,
        };
        let mut renderer =
            Renderer::new(budget, &self.setup.settings(), &fonts).map_err(|e| format!("{e:?}"))?;
        let overrides: Vec<&[u8]> = self
            .setup
            .style_overrides
            .iter()
            .map(Vec::as_slice)
            .collect();
        let options = Options {
            extract_fonts: true,
            style_overrides: &overrides,
            ..Options::default()
        };
        if !self.stream {
            let mut script =
                Script::parse(budget, &self.data, &options).map_err(|e| format!("{e:?}"))?;
            for &now in &self.times {
                frame(&mut renderer, &mut script, now, out);
            }
            return Ok(());
        }
        let fixture = Fixture::read(&self.data, true);
        let mut script = Script::new(budget, &options).map_err(|e| format!("{e:?}"))?;
        for command in &fixture.commands {
            let done = match command {
                Command::Private(text) => script.process_codec_private(text),
                Command::Data(text) => script.process_data(text),
                Command::Styles(text) => script.process_styles(text),
                Command::Chunk {
                    start,
                    duration,
                    packet,
                } => script.process_chunk(packet, *start, *duration),
                Command::Flush => {
                    script.flush_events();
                    Ok(())
                }
                Command::Prune(deadline) => {
                    script.prune_events(*deadline);
                    Ok(())
                }
                Command::CheckReadOrder(check) => {
                    script.set_check_read_order(*check);
                    Ok(())
                }
                Command::Frames(frames) => {
                    for now in frames.times().take(200) {
                        frame(&mut renderer, &mut script, now, out);
                    }
                    Ok(())
                }
                Command::Dump => Ok(()),
            };
            if let Err(error) = done {
                let _ = writeln!(out, "command {error:?}");
            }
        }
        Ok(())
    }
}

/// Renders one frame and records it.
fn frame(renderer: &mut Renderer, script: &mut Script, now: i64, out: &mut String) {
    let outcome = match renderer.render(script, now) {
        Ok(frame) => {
            let change = match frame.change {
                Change::None => 0,
                Change::Positions => 1,
                Change::Content => 2,
            };
            let (count, digest) = (frame.images.len(), digest(frame.images));
            format!("change {change} images {count} digest {digest:016x}")
        }
        Err(error) => format!("error {error:?}"),
    };
    let work = work_spent(renderer);
    let _ = writeln!(out, "frame {now} {outcome} work {work}");
}

/// The work the frame spent, with the `work-probe` feature.
fn work_spent(renderer: &Renderer) -> String {
    #[cfg(feature = "work-probe")]
    return renderer.work_spent().to_string();
    #[cfg(not(feature = "work-probe"))]
    {
        let _ = renderer;
        "-".into()
    }
}

/// FNV-1a over every image's kind, place, size, colour and pixel rows.
fn digest(images: &[Image]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut add = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    };
    for image in images {
        add(format!("{:?}", image.kind).as_bytes());
        for value in [
            i64::from(image.x),
            i64::from(image.y),
            image.width() as i64,
            image.height() as i64,
            i64::from(image.colour.0),
        ] {
            add(&value.to_le_bytes());
        }
        for row in image.rows() {
            add(row);
        }
    }
    hash
}

/// Each fixture's cases: at its frames, every 40 ms, and mutated.
fn fixture_cases(
    fonts: &Path,
    fixtures: &Path,
    mutations: u64,
    rng: &mut Rng,
) -> Result<Vec<Case>, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixtures)
        .map_err(|e| format!("{}: {e}", fixtures.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    paths.sort();
    let mut cases = Vec::new();
    for path in &paths {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let stream = path.extension().is_some_and(|e| e == "chunks");
        let fixture = Fixture::read(&data, stream);
        let font = |p: &[u8]| fonts.join(String::from_utf8_lossy(p).as_ref());
        let listed: Vec<i64> = fixture.frames.iter().flat_map(|f| f.times()).collect();
        let end = listed.iter().copied().max().unwrap_or(2000);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().replace('.', "-"))
            .unwrap_or_default();
        for size in SIZES {
            let base = Case {
                name: format!("{name}-{}p", size.0.1),
                data: data.clone(),
                stream,
                attach: fixture.attach.iter().map(|p| font(p)).collect(),
                default_family: fixture.default_family.map(<[u8]>::to_vec),
                default_font: fixture.default_font.map(font),
                setup: Setup::player(size),
                times: listed.clone(),
            };
            let every = Case {
                name: format!("{}~every-40ms", base.name),
                times: (0..=end / 40).map(|k| k * 40).collect(),
                ..base.clone()
            };
            let mutated: Vec<Case> = (0..mutations)
                .map(|m| Case {
                    name: format!("{}~{m}", base.name),
                    data: if !stream || rng.chance(50) {
                        mutate(rng, &data)
                    } else {
                        data.clone()
                    },
                    setup: Setup::varied(rng, size),
                    times: listed
                        .iter()
                        .copied()
                        .chain((0..8).map(|_| rng.range(0, end + 500)))
                        .collect(),
                    ..base.clone()
                })
                .collect();
            cases.push(base);
            cases.push(every);
            cases.extend(mutated);
        }
    }
    Ok(cases)
}

/// The fonts generated scripts draw with: families of the fixtures' fonts.
const ATTACHED: [&str; 11] = [
    "croscorefonts-1.31.0/Arimo-Regular.ttf",
    "croscorefonts-1.31.0/Arimo-Bold.ttf",
    "croscorefonts-1.31.0/Arimo-Italic.ttf",
    "croscorefonts-1.31.0/Arimo-BoldItalic.ttf",
    "croscorefonts-1.31.0/Tinos-Regular.ttf",
    "croscorefonts-1.31.0/Cousine-Regular.ttf",
    "dejavu-fonts-ttf-2.37/ttf/DejaVuSans.ttf",
    "NotoSansArabic-Regular.ttf",
    "NotoSansHebrew-Regular.ttf",
    "NotoSansThai-Regular.ttf",
    "NotoSansDevanagari-Regular.ttf",
];

fn random_cases(fonts: &Path, count: u64, rng: &mut Rng) -> Vec<Case> {
    let attach: Vec<PathBuf> = ATTACHED.iter().map(|p| fonts.join(p)).collect();
    (0..count)
        .map(|r| {
            let (data, end) = random_script(rng);
            let size = rng.pick(&SIZES);
            let step = rng.pick(&[40, 100, 250]);
            Case {
                name: format!("random-{r}"),
                data,
                stream: false,
                attach: if rng.chance(85) {
                    attach.clone()
                } else {
                    attach[..2].to_vec()
                },
                default_family: rng.chance(80).then(|| b"Arimo".to_vec()),
                default_font: rng
                    .chance(80)
                    .then(|| fonts.join("dejavu-fonts-ttf-2.37/ttf/DejaVuSans.ttf")),
                setup: Setup::varied(rng, size),
                times: (0..=(end + 200) / step).map(|k| k * step).collect(),
            }
        })
        .collect()
}

const WORDS: &[&str] = &[
    "Hello",
    "world",
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "fi",
    "ffl",
    "AVATAR",
    "Wy",
    "To",
    "office",
    "naïve",
    "café",
    "e\u{301}",
    "a\u{308}\u{301}",
    "x",
    "مرحبا",
    "بالعالم",
    "لا",
    "سلام",
    "שלום",
    "עולם",
    "สวัสดี",
    "ภาษาไทย",
    "नमस्ते",
    "क्षत्रिय",
    "日本語",
    "漢字",
    "かな",
    "한국어",
    "Ελληνικά",
    "Кириллица",
    "😀",
    "👍🏽",
    "a\u{200d}b",
    "a\u{200c}b",
    "soft\u{ad}hyphen",
    "\u{feff}",
    "\u{200b}",
    "1234",
    "...",
    "!",
    "\u{2028}",
    "\u{fffd}",
    "ﷺ",
    "\u{200f}",
    "\u{200e}",
    "ـ",
];

const FAMILIES: &[&str] = &[
    "Arimo",
    "Tinos",
    "Cousine",
    "DejaVu Sans",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    "Noto Sans Thai",
    "Noto Sans Devanagari",
    "Missing Family",
    "@Arimo",
    "",
];

fn number(rng: &mut Rng) -> String {
    match rng.below(10) {
        0 => "0".into(),
        1 => format!("-{}", rng.below(50)),
        2 => format!("{}", rng.below(5000)),
        3 => format!("{:.2}", rng.float(-3.0, 3.0)),
        4 => "1e9".into(),
        _ => format!("{}", rng.below(80)),
    }
}

fn colour(rng: &mut Rng) -> String {
    let mask = if rng.chance(70) {
        0x00ff_ffff
    } else {
        0xffff_ffff
    };
    format!("&H{:08X}&", rng.next() as u32 & mask)
}

fn drawing(rng: &mut Rng) -> String {
    let mut d = String::from("m 0 0");
    for _ in 0..rng.range(1, 6) {
        let command = rng.pick(&["l", "b", "s", "n", "m", "c", "p"]);
        let points = if command == "b" { 3 } else { 1 };
        let _ = write!(d, " {command}");
        for _ in 0..points {
            let _ = write!(d, " {} {}", rng.range(-50, 200), rng.range(-50, 200));
        }
    }
    d
}

fn tag(rng: &mut Rng) -> String {
    let n = number(rng);
    match rng.below(46) {
        0 => format!("\\b{}", rng.pick(&["0", "1", "700", "300"])),
        1 => format!("\\i{}", rng.below(2)),
        2 => format!("\\u{}", rng.below(2)),
        3 => format!("\\s{}", rng.below(2)),
        4 => format!("\\bord{n}"),
        5 => format!("\\xbord{n}"),
        6 => format!("\\ybord{n}"),
        7 => format!("\\shad{n}"),
        8 => format!("\\xshad{n}"),
        9 => format!("\\yshad{n}"),
        10 => format!("\\be{}", rng.below(4)),
        11 => format!("\\blur{}", rng.float(0.0, 4.0)),
        12 => format!("\\fn{}", rng.pick(FAMILIES)),
        13 => format!("\\fs{}", rng.pick(&["0", "8", "20", "48", "90", "200"])),
        14 => format!("\\fscx{}", rng.pick(&["0", "50", "100", "150", "-20"])),
        15 => format!("\\fscy{}", rng.pick(&["0", "50", "100", "150"])),
        16 => format!("\\fsp{}", rng.pick(&["0", "2", "-3", "10"])),
        17 => format!("\\frx{n}"),
        18 => format!("\\fry{n}"),
        19 => format!("\\frz{n}"),
        20 => format!("\\fax{:.2}", rng.float(-1.0, 1.0)),
        21 => format!("\\fay{:.2}", rng.float(-1.0, 1.0)),
        22 => format!("\\fe{}", rng.pick(&["-1", "0", "1", "177", "178"])),
        23 => format!("\\c{}", colour(rng)),
        24 => format!("\\{}c{}", rng.range(1, 4), colour(rng)),
        25 => format!("\\alpha&H{:02X}&", rng.below(256)),
        26 => format!(
            "\\{}a&H{:02X}&",
            rng.range(1, 4),
            rng.pick(&[0, 0x80, 0xff, 0x10])
        ),
        27 => format!("\\an{}", rng.range(1, 9)),
        // Legacy alignment, with the values whose horizontal bits are 0.
        28 => format!(
            "\\a{}",
            rng.pick(&[
                "1", "2", "3", "5", "6", "7", "9", "10", "11", "0", "4", "8", "12"
            ])
        ),
        29 => format!(
            "\\{}{}",
            rng.pick(&["k", "K", "kf", "ko"]),
            rng.range(-20, 150)
        ),
        30 => format!("\\kt{}", rng.below(200)),
        31 => format!("\\q{}", rng.below(4)),
        32 => format!("\\r{}", rng.pick(&["", "Default", "Alt", "Nope"])),
        33 => format!("\\pos({},{})", rng.range(-100, 2000), rng.range(-100, 1200)),
        34 => {
            let timed = if rng.chance(50) {
                format!(",{},{}", rng.below(500), rng.below(2000))
            } else {
                String::new()
            };
            format!(
                "\\move({},{},{},{}{timed})",
                rng.range(0, 1900),
                rng.range(0, 1000),
                rng.range(0, 1900),
                rng.range(0, 1000),
            )
        }
        35 => format!("\\org({},{})", rng.range(-500, 2000), rng.range(-500, 1500)),
        36 => format!("\\fad({},{})", rng.below(800), rng.below(800)),
        37 => format!(
            "\\fade({},{},{},{},{},{},{})",
            rng.below(256),
            rng.below(256),
            rng.below(256),
            rng.below(500),
            rng.below(1000),
            rng.below(1500),
            rng.below(2500)
        ),
        38 => {
            let inner: String = (0..rng.range(1, 3))
                .map(|_| tag(rng))
                .filter(|t| !t.starts_with("\\t"))
                .collect();
            if rng.chance(50) {
                format!("\\t({inner})")
            } else {
                let (t1, t2, accel) = (rng.below(1000), rng.below(3000), rng.float(0.2, 3.0));
                format!("\\t({t1},{t2},{accel:.1},{inner})")
            }
        }
        39 => format!(
            "\\{}clip({},{},{},{})",
            if rng.chance(30) { "i" } else { "" },
            rng.range(-50, 1500),
            rng.range(-50, 800),
            rng.range(0, 2000),
            rng.range(0, 1200)
        ),
        40 => format!(
            "\\{}clip({}{})",
            if rng.chance(30) { "i" } else { "" },
            if rng.chance(30) { "2," } else { "" },
            drawing(rng)
        ),
        41 => format!("\\pbo{}", rng.range(-20, 40)),
        42 => format!("\\fr{n}"),
        43 => "\\fs+5".into(),
        44 => format!(
            "\\bord{:.1}\\shad{:.1}",
            rng.float(0.0, 5.0),
            rng.float(-3.0, 5.0)
        ),
        _ => format!(
            "\\{}",
            rng.pick(&["N", "h", "n", "fnArimo", "b1\\i1", "u1\\s1"])
        ),
    }
}

fn text(rng: &mut Rng) -> String {
    let mut t = String::new();
    for _ in 0..rng.range(1, 14) {
        match rng.below(12) {
            0..=2 => {
                t.push('{');
                for _ in 0..rng.range(1, 4) {
                    t.push_str(&tag(rng));
                }
                t.push('}');
            }
            3 => t.push_str(rng.pick(&["\\N", "\\n", "\\h", "  ", " ", "\\N\\N"])),
            4 => {
                let _ = write!(t, "{{\\p{}}}{}{{\\p0}}", rng.range(1, 4), drawing(rng));
            }
            _ => {
                t.push_str(rng.pick(WORDS));
                if rng.chance(70) {
                    t.push(' ');
                }
            }
        }
    }
    t
}

fn style(rng: &mut Rng, name: &str) -> String {
    let fields = [
        rng.pick(FAMILIES).to_string(),
        rng.pick(&["20", "48", "64", "0", "120", "9"]).into(),
        colour(rng),
        colour(rng),
        colour(rng),
        colour(rng),
        rng.pick(&["0", "-1", "1"]).into(),
        rng.pick(&["0", "-1"]).into(),
        rng.pick(&["0", "-1"]).into(),
        rng.pick(&["0", "-1"]).into(),
        rng.pick(&["100", "80", "150", "0"]).into(),
        rng.pick(&["100", "80", "150"]).into(),
        rng.pick(&["0", "3", "-2"]).into(),
        rng.pick(&["0", "10", "-30", "180"]).into(),
        rng.pick(&["1", "3", "4"]).into(),
        rng.pick(&["0", "1", "2.5", "6"]).into(),
        rng.pick(&["0", "1", "3", "-2"]).into(),
        // Numeric-keypad alignment, with 0 for no horizontal alignment.
        rng.pick(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "10", "-2"])
            .into(),
        rng.range(0, 80).to_string(),
        rng.range(0, 80).to_string(),
        rng.range(0, 120).to_string(),
        rng.pick(&["1", "0", "-1", "177"]).into(),
    ];
    format!("Style: {name},{}\n", fields.join(","))
}

/// A generated script and its last event's end in milliseconds.
fn random_script(rng: &mut Rng) -> (Vec<u8>, i64) {
    let play = rng.pick(&[(1920, 1080), (640, 480), (384, 288), (1280, 720), (0, 0)]);
    let mut s = String::from("[Script Info]\nScriptType: v4.00+\n");
    if play.0 != 0 {
        let _ = write!(s, "PlayResX: {}\nPlayResY: {}\n", play.0, play.1);
    }
    if rng.chance(20) {
        let (x, y) = (rng.pick(&[640, 1920]), rng.pick(&[360, 1080, 480]));
        let _ = write!(s, "LayoutResX: {x}\nLayoutResY: {y}\n");
    }
    let _ = writeln!(s, "WrapStyle: {}", rng.below(4));
    let _ = writeln!(s, "ScaledBorderAndShadow: {}", rng.pick(&["yes", "no"]));
    if rng.chance(20) {
        let _ = writeln!(s, "Kerning: {}", rng.pick(&["yes", "no"]));
    }
    if rng.chance(15) {
        let _ = writeln!(s, "Language: {}", rng.pick(&["en", "ar", "sr", "x"]));
    }
    s.push_str(
        "\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, \
         OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, \
         Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, \
         Encoding\n",
    );
    s.push_str(&style(rng, "Default"));
    if rng.chance(60) {
        s.push_str(&style(rng, "Alt"));
    }
    s.push_str(
        "\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, \
         Effect, Text\n",
    );
    let mut last_end = 0;
    let time = |ms: i64| format!("0:00:{:02}.{:02}", ms / 1000, (ms % 1000) / 10);
    for _ in 0..rng.range(1, 14) {
        let start = rng.range(0, 1500);
        let end = start + rng.range(0, 2500);
        last_end = last_end.max(end);
        let effect = match rng.below(10) {
            0 => format!(
                "Banner;{};{};{}",
                rng.below(30),
                rng.below(2),
                rng.below(100)
            ),
            1 => format!(
                "Scroll {};{};{};{}",
                rng.pick(&["up", "down"]),
                rng.below(600),
                rng.below(900),
                rng.below(30)
            ),
            _ => String::new(),
        };
        let _ = writeln!(
            s,
            "Dialogue: {},{},{},{},,{},{},{},{effect},{}",
            rng.pick(&[0, 0, 0, 1, -1, 5]),
            time(start),
            time(end),
            rng.pick(&["Default", "Default", "Alt", "Missing"]),
            rng.pick(&[0, 0, 10, 300]),
            rng.pick(&[0, 0, 10, 300]),
            rng.pick(&[0, 0, 10, 500]),
            text(rng)
        );
    }
    (s.into_bytes(), last_end)
}

/// `data` with a few of its dialogue lines changed.
fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = data.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
    for _ in 0..rng.range(1, 6) {
        let dialogues: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with(b"Dialogue:"))
            .map(|(i, _)| i)
            .collect();
        if dialogues.is_empty() {
            break;
        }
        let at = rng.pick(&dialogues);
        match rng.below(7) {
            0..=2 => {
                // An override tag inserted into the text.
                let line = &mut lines[at];
                let text_start = line
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b',')
                    .map(|(i, _)| i)
                    .nth(8)
                    .unwrap_or(line.len() - 1);
                let pos = rng.range(text_start as i64 + 1, line.len() as i64) as usize;
                let tag = format!("{{{}}}", tag(rng));
                line.splice(pos..pos, tag.into_bytes());
            }
            3 => {
                let copy = lines[at].clone();
                lines.insert(at, copy);
            }
            4 => {
                let line = &mut lines[at];
                if line.len() > 12 {
                    let pos = rng.range(10, line.len() as i64 - 1) as usize;
                    line[pos] = rng.pick(b" \\{}N0-9,()&Ha");
                }
            }
            5 => {
                let words = text(rng);
                lines[at].extend_from_slice(words.as_bytes());
            }
            _ => {
                let line = &mut lines[at];
                if line.len() > 12 {
                    let pos = rng.range(10, line.len() as i64 - 1) as usize;
                    line.remove(pos);
                }
            }
        }
    }
    lines.join(&b'\n')
}
