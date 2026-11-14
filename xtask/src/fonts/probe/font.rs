//! A TrueType font assembled from a short declarative description, for probing
//! how a font engine runs a face's programs.
//!
//! The description is line based; `#` starts a comment. Face settings:
//!
//! - `units-per-em N` (default 2048), `head-flags N` (default 0);
//! - `metrics ASCENDER DESCENDER`: `hhea`'s ascender and descender and the
//!   `OS/2` typographic and Windows ones (default four fifths of the em above
//!   the baseline and the rest below it, so the ascender to descender span is
//!   the em);
//! - `heights H...`: the 26.6 pixel heights a probe loads the face at, as
//!   libass sizes a face (default 16384);
//! - `wraps-at-32-bits`: the probe's results depend on the reference's
//!   integer width; its record, from 32-bit ARMv7, stands for a 64-bit
//!   reference that differs from it (a 32-bit reference must match it);
//! - `maxp KEY N...`: `maxp` fields, overriding the defaults and the values
//!   computed from the glyphs: `zones` (2), `twilight` (16), `storage` (64),
//!   `functions` (64), `instruction-defs` (0), `stack` (256), `points`,
//!   `contours`, `composite-points`, `composite-contours`,
//!   `instruction-size`, `components` and `depth`;
//! - `maxp-version 0.5`: a version 0.5 `maxp`, which holds only the glyph
//!   count, in place of version 1.0;
//! - `cvt V...`: control values in font units, appended in order;
//! - `gasp PPEM BEHAVIOUR`: one `gasp` range, in order;
//! - `fpgm` and `prep`: a program up to a line `end`, in the assembler's
//!   syntax (see `asm`); the table is written even when the program is empty;
//! - `from FILE PART...`: parts of the first face of the font file `FILE`
//!   (relative to the fonts directory the command is given): `fpgm`, `prep`
//!   and `cvt` as stored, `maxp` (its program limits), `head` (units per em
//!   and flags) and `metrics` (the ascender and descender libass sizes it
//!   by), so a probe can run a real face's programs.
//!
//! Glyphs follow in index order, each from `glyph [advance N] [lsb N]
//! [char C]...` to `end`, where `char` maps a character code to it:
//!
//! - `contour X Y, X Y off, ...`: a contour of on-curve and (`off`)
//!   off-curve points in font units;
//! - `component GLYPH (offset DX DY | anchor PARENT CHILD) [OPTION]...`: a
//!   component, its options `round` (`ROUND_XY_TO_GRID`), `use-metrics`,
//!   `overlap`, `scaled-offset`, `unscaled-offset`, `scale S`, `scale SX SY`,
//!   `matrix XX YX XY YY` (F2Dot14 values as decimals) and `flags N` (more
//!   flag bits, as stored);
//! - `bounds XMIN YMIN XMAX YMAX`: the header box, computed by default;
//! - `program` up to `end`: the glyph's instructions.
//!
//! A glyph has contours or components, or neither for an empty glyph; its
//! left side bearing defaults to its box's left edge.

use super::asm::{assemble, number};

const ON_CURVE: u8 = 0x01;

/// The `maxp` fields a description may set, in table order after `numGlyphs`.
const MAXP_FIELDS: [&str; 13] = [
    "points",
    "contours",
    "composite-points",
    "composite-contours",
    "zones",
    "twilight",
    "storage",
    "functions",
    "instruction-defs",
    "stack",
    "instruction-size",
    "components",
    "depth",
];

const ARGS_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const ROUND_XY_TO_GRID: u16 = 0x0004;
const WE_HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
const WE_HAVE_INSTRUCTIONS: u16 = 0x0100;
const USE_MY_METRICS: u16 = 0x0200;
const OVERLAP_COMPOUND: u16 = 0x0400;
const SCALED_COMPONENT_OFFSET: u16 = 0x0800;
const UNSCALED_COMPONENT_OFFSET: u16 = 0x1000;

/// A parsed description.
#[derive(Debug, Default)]
pub(super) struct Description {
    pub(super) heights: Vec<i32>,
    pub(super) wraps_at_32_bits: bool,
    units_per_em: u16,
    head_flags: u16,
    metrics: Option<(i16, i16)>,
    borrowed: Vec<(String, Vec<String>)>,
    maxp: Vec<(String, u16)>,
    /// Whether `maxp` is version 0.5.
    short_maxp: bool,
    cvt: Vec<i16>,
    gasp: Vec<(u16, u16)>,
    fpgm: Option<Vec<u8>>,
    prep: Option<Vec<u8>>,
    glyphs: Vec<Glyph>,
}

#[derive(Debug, Default)]
struct Glyph {
    advance: u16,
    lsb: Option<i16>,
    chars: Vec<u16>,
    contours: Vec<Vec<(i16, i16, bool)>>,
    components: Vec<Component>,
    bounds: Option<[i16; 4]>,
    program: Option<Vec<u8>>,
}

#[derive(Debug)]
struct Component {
    glyph: u16,
    /// An offset, or the parent and child points to match.
    args: (i32, i32),
    flags: u16,
    /// F2Dot14 `[xx, yx, xy, yy]`, when the component has one.
    matrix: Option<[i16; 4]>,
}

impl Description {
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let mut d = Self {
            heights: vec![16384],
            units_per_em: 2048,
            ..Self::default()
        };
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.split('#').next().unwrap_or("").trim()))
            .filter(|(_, l)| !l.is_empty());
        while let Some((line, text)) = lines.next() {
            let fail = |e: String| format!("line {line}: {e}");
            let (key, rest) = text.split_once(' ').unwrap_or((text, ""));
            let args: Vec<&str> = rest.split_whitespace().collect();
            match key {
                "units-per-em" => d.units_per_em = single(&args).map_err(fail)?,
                "head-flags" => d.head_flags = single(&args).map_err(fail)?,
                "metrics" => match numbers::<i16>(&args).map_err(fail)?[..] {
                    [ascender, descender] => d.metrics = Some((ascender, descender)),
                    _ => return Err(fail("metrics ASCENDER DESCENDER".into())),
                },
                "heights" => d.heights = numbers(&args).map_err(fail)?,
                "wraps-at-32-bits" if args.is_empty() => d.wraps_at_32_bits = true,
                "maxp" => {
                    if !args.len().is_multiple_of(2) {
                        return Err(fail("maxp KEY VALUE...".into()));
                    }
                    for pair in args.chunks(2) {
                        if !MAXP_FIELDS.contains(&pair[0]) {
                            return Err(fail(format!("unknown maxp field `{}`", pair[0])));
                        }
                        let value = single(&pair[1..]).map_err(fail)?;
                        d.maxp.push((pair[0].to_owned(), value));
                    }
                }
                "maxp-version" => match args[..] {
                    ["0.5"] => d.short_maxp = true,
                    ["1.0"] => d.short_maxp = false,
                    _ => return Err(fail("maxp-version 0.5 or 1.0".into())),
                },
                "cvt" => d.cvt.extend(numbers::<i16>(&args).map_err(fail)?),
                "gasp" => match numbers::<u16>(&args).map_err(fail)?[..] {
                    [ppem, behaviour] => d.gasp.push((ppem, behaviour)),
                    _ => return Err(fail("gasp PPEM BEHAVIOUR".into())),
                },
                "fpgm" | "prep" => {
                    let program = Some(program(&mut lines)?);
                    if key == "fpgm" {
                        d.fpgm = program;
                    } else {
                        d.prep = program;
                    }
                }
                "glyph" => d.glyphs.push(glyph(&args, &mut lines).map_err(fail)?),
                "from" => match &args[..] {
                    [file, parts @ ..] if !parts.is_empty() => {
                        let known = ["fpgm", "prep", "cvt", "maxp", "head", "metrics"];
                        if let Some(part) = parts.iter().find(|p| !known.contains(p)) {
                            return Err(fail(format!("unknown part `{part}`")));
                        }
                        let parts = parts.iter().map(|&p| p.to_owned()).collect();
                        d.borrowed.push(((*file).to_owned(), parts));
                    }
                    _ => return Err(fail("from FILE PART...".into())),
                },
                _ => return Err(fail(format!("unknown setting `{key}`"))),
            }
        }
        if d.glyphs.is_empty() {
            return Err("a description needs at least one glyph".into());
        }
        if d.units_per_em == 0 {
            return Err("units-per-em must not be zero".into());
        }
        if let Some(c) = d
            .glyphs
            .iter()
            .flat_map(|g| &g.components)
            .find(|c| usize::from(c.glyph) >= d.glyphs.len())
        {
            return Err(format!("component glyph {} is not in the face", c.glyph));
        }
        Ok(d)
    }

    /// Takes the parts `from` lines name out of their font files, found in
    /// `fonts`.
    pub(super) fn borrow_parts(&mut self, fonts: &std::path::Path) -> Result<(), String> {
        for (file, parts) in std::mem::take(&mut self.borrowed) {
            let path = fonts.join(&file);
            let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let table = |tag: &[u8; 4]| {
                sfnt_table(&data, tag)
                    .ok_or_else(|| format!("{file}: no {}", String::from_utf8_lossy(tag)))
            };
            for part in parts {
                match part.as_str() {
                    "fpgm" => self.fpgm = Some(table(b"fpgm")?.to_vec()),
                    "prep" => self.prep = Some(table(b"prep")?.to_vec()),
                    "cvt" => {
                        self.cvt = table(b"cvt ")?
                            .chunks_exact(2)
                            .map(|w| i16::from_be_bytes([w[0], w[1]]))
                            .collect();
                    }
                    "maxp" => {
                        let maxp = table(b"maxp")?;
                        let at = |i: usize| {
                            maxp.get(i..i + 2)
                                .map_or(0, |w| u16::from_be_bytes([w[0], w[1]]))
                        };
                        for (key, offset) in [
                            ("zones", 14),
                            ("twilight", 16),
                            ("storage", 18),
                            ("functions", 20),
                            ("instruction-defs", 22),
                            ("stack", 24),
                        ] {
                            self.maxp.push((key.to_owned(), at(offset)));
                        }
                    }
                    "head" => {
                        let head = table(b"head")?;
                        let at = |i: usize| {
                            head.get(i..i + 2)
                                .map_or(0, |w| u16::from_be_bytes([w[0], w[1]]))
                        };
                        self.head_flags = at(16);
                        self.units_per_em = at(18);
                    }
                    _ => {
                        // The Windows metrics libass sizes a face by, else `hhea`'s.
                        let os2 = sfnt_table(&data, b"OS/2");
                        let at = |t: &[u8], i: usize| {
                            t.get(i..i + 2)
                                .map_or(0, |w| u16::from_be_bytes([w[0], w[1]]))
                        };
                        self.metrics = Some(match os2 {
                            Some(o) if at(o, 74) != 0 || at(o, 76) != 0 => (
                                at(o, 74).cast_signed(),
                                at(o, 76).cast_signed().wrapping_neg(),
                            ),
                            _ => {
                                let hhea = table(b"hhea")?;
                                (at(hhea, 4).cast_signed(), at(hhea, 6).cast_signed())
                            }
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// The font file.
    pub(super) fn build(&self) -> Result<Vec<u8>, String> {
        let points = |g: usize| self.points(g, 0);
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        let mut boxes = Vec::new();
        for (index, glyph) in self.glyphs.iter().enumerate() {
            loca.extend((glyf.len() as u32).to_be_bytes());
            let bounds = match glyph.bounds {
                Some(b) => Some(b),
                None => bounds(&points(index)?)?,
            };
            boxes.push(bounds);
            if let Some(b) = bounds {
                glyf.extend(self.glyph_data(glyph, b)?);
                // Glyphs start at even offsets.
                if glyf.len() % 2 != 0 {
                    glyf.push(0);
                }
            }
        }
        loca.extend((glyf.len() as u32).to_be_bytes());
        let units = i32::from(self.units_per_em);
        let (ascender, descender) = self.metrics.unwrap_or_else(|| {
            let ascender = units - units / 5;
            (ascender as i16, (ascender - units) as i16)
        });
        let face_box = boxes
            .iter()
            .flatten()
            .fold(None, |acc: Option<[i16; 4]>, b| {
                Some(acc.map_or(*b, |a| {
                    [
                        a[0].min(b[0]),
                        a[1].min(b[1]),
                        a[2].max(b[2]),
                        a[3].max(b[3]),
                    ]
                }))
            });
        let face_box = face_box.unwrap_or_default();
        let mut tables: Vec<([u8; 4], Vec<u8>)> = vec![
            (*b"head", self.head(face_box)),
            (*b"hhea", self.hhea(ascender, descender, &boxes)),
            (*b"maxp", self.maxp()?),
            (*b"OS/2", os2(ascender, descender)),
            (*b"hmtx", self.hmtx(&boxes)),
            (*b"cmap", self.cmap()?),
            (*b"loca", loca),
            (*b"glyf", glyf),
            (*b"name", name()),
            (*b"post", post()),
        ];
        if !self.cvt.is_empty() {
            let cvt = self.cvt.iter().flat_map(|v| v.to_be_bytes()).collect();
            tables.push((*b"cvt ", cvt));
        }
        if let Some(fpgm) = &self.fpgm {
            tables.push((*b"fpgm", fpgm.clone()));
        }
        if let Some(prep) = &self.prep {
            tables.push((*b"prep", prep.clone()));
        }
        if !self.gasp.is_empty() {
            let mut gasp = vec![0, 1];
            gasp.extend((self.gasp.len() as u16).to_be_bytes());
            for &(ppem, behaviour) in &self.gasp {
                gasp.extend(ppem.to_be_bytes());
                gasp.extend(behaviour.to_be_bytes());
            }
            tables.push((*b"gasp", gasp));
        }
        Ok(sfnt(tables))
    }

    /// A glyph's points in font units, its components placed (rounded to
    /// whole units), for its box.
    fn points(&self, index: usize, depth: u32) -> Result<Vec<(i32, i32)>, String> {
        if depth > 16 {
            return Err(format!("glyph {index}: components nest too deeply"));
        }
        let glyph = &self.glyphs[index];
        let mut points: Vec<(i32, i32)> = glyph
            .contours
            .iter()
            .flatten()
            .map(|&(x, y, _)| (i32::from(x), i32::from(y)))
            .collect();
        for c in &glyph.components {
            let mut child = self.points(usize::from(c.glyph), depth + 1)?;
            if let Some(m) = c.matrix {
                let [xx, yx, xy, yy] = m.map(|v| f64::from(v) / 16384.0);
                for p in &mut child {
                    let (x, y) = (f64::from(p.0), f64::from(p.1));
                    *p = (
                        (xx * x + xy * y).round() as i32,
                        (yx * x + yy * y).round() as i32,
                    );
                }
            }
            let (dx, dy) = if c.flags & ARGS_ARE_XY_VALUES != 0 {
                c.args
            } else {
                let parent = usize::try_from(c.args.0).ok().and_then(|i| points.get(i));
                let own = usize::try_from(c.args.1).ok().and_then(|i| child.get(i));
                match (parent, own) {
                    (Some(p), Some(o)) => (p.0 - o.0, p.1 - o.1),
                    _ => return Err(format!("glyph {index}: anchor points out of range")),
                }
            };
            points.extend(child.iter().map(|&(x, y)| (x + dx, y + dy)));
        }
        Ok(points)
    }

    fn glyph_data(&self, glyph: &Glyph, b: [i16; 4]) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let program = glyph.program.as_deref().unwrap_or_default();
        let program_len = u16::try_from(program.len()).map_err(|_| "program too long")?;
        if glyph.components.is_empty() {
            let contours = i16::try_from(glyph.contours.len()).map_err(|_| "too many contours")?;
            out.extend(contours.to_be_bytes());
            out.extend(b.iter().flat_map(|v| v.to_be_bytes()));
            let mut end = 0_usize;
            for contour in &glyph.contours {
                end += contour.len();
                let last = u16::try_from(end - 1).map_err(|_| "too many points")?;
                out.extend(last.to_be_bytes());
            }
            out.extend(program_len.to_be_bytes());
            out.extend(program);
            let points: Vec<_> = glyph.contours.iter().flatten().collect();
            out.extend(
                points
                    .iter()
                    .map(|&&(_, _, on)| if on { ON_CURVE } else { 0 }),
            );
            for axis in 0..2 {
                let mut last = 0_i32;
                for &&(x, y, _) in &points {
                    let v = i32::from(if axis == 0 { x } else { y });
                    let delta = i16::try_from(v - last).map_err(|_| "coordinate step too large")?;
                    out.extend(delta.to_be_bytes());
                    last = v;
                }
            }
        } else {
            out.extend((-1_i16).to_be_bytes());
            out.extend(b.iter().flat_map(|v| v.to_be_bytes()));
            for (i, c) in glyph.components.iter().enumerate() {
                let last = i + 1 == glyph.components.len();
                let mut flags = c.flags | ARGS_ARE_WORDS;
                if !last {
                    flags |= MORE_COMPONENTS;
                } else if glyph.program.is_some() {
                    flags |= WE_HAVE_INSTRUCTIONS;
                }
                out.extend(flags.to_be_bytes());
                out.extend(c.glyph.to_be_bytes());
                for arg in [c.args.0, c.args.1] {
                    let word = if flags & ARGS_ARE_XY_VALUES != 0 {
                        i16::try_from(arg).map(i16::to_be_bytes)
                    } else {
                        u16::try_from(arg).map(u16::to_be_bytes)
                    };
                    out.extend(word.map_err(|_| "component argument out of range")?);
                }
                match c.matrix {
                    Some([s, 0, 0, t]) if flags & WE_HAVE_A_SCALE != 0 && s == t => {
                        out.extend(s.to_be_bytes());
                    }
                    Some([s, 0, 0, t]) if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 => {
                        out.extend(s.to_be_bytes());
                        out.extend(t.to_be_bytes());
                    }
                    Some(m) => out.extend(m.iter().flat_map(|v| v.to_be_bytes())),
                    None => {}
                }
            }
            if glyph.program.is_some() {
                out.extend(program_len.to_be_bytes());
                out.extend(program);
            }
        }
        Ok(out)
    }

    fn head(&self, b: [i16; 4]) -> Vec<u8> {
        let mut t = Vec::with_capacity(54);
        t.extend([0, 1, 0, 0]); // version 1.0
        t.extend(0x0001_0000_u32.to_be_bytes()); // fontRevision
        t.extend([0; 4]); // checksumAdjustment, set once the file is complete
        t.extend(0x5F0F_3CF5_u32.to_be_bytes());
        t.extend(self.head_flags.to_be_bytes());
        t.extend(self.units_per_em.to_be_bytes());
        t.extend([0; 16]); // created, modified
        t.extend(b.iter().flat_map(|v| v.to_be_bytes()));
        t.extend([0, 0]); // macStyle
        t.extend(8_u16.to_be_bytes()); // lowestRecPPEM
        t.extend(2_i16.to_be_bytes()); // fontDirectionHint
        t.extend(1_i16.to_be_bytes()); // indexToLocFormat: long offsets
        t.extend([0, 0]); // glyphDataFormat
        t
    }

    fn hhea(&self, ascender: i16, descender: i16, boxes: &[Option<[i16; 4]>]) -> Vec<u8> {
        let advance_max = self.glyphs.iter().map(|g| g.advance).max().unwrap_or(0);
        let mut t = Vec::with_capacity(36);
        t.extend([0, 1, 0, 0]);
        t.extend(ascender.to_be_bytes());
        t.extend(descender.to_be_bytes());
        t.extend([0, 0]); // lineGap
        t.extend(advance_max.to_be_bytes());
        let lsb = self.bearings(boxes).min().unwrap_or(0);
        t.extend(lsb.to_be_bytes()); // minLeftSideBearing
        t.extend([0; 4]); // minRightSideBearing, xMaxExtent
        t.extend(1_i16.to_be_bytes()); // caretSlopeRise
        t.extend([0; 14]); // caretSlopeRun, caretOffset, reserved, metricDataFormat
        t.extend((self.glyphs.len() as u16).to_be_bytes());
        t
    }

    fn bearings<'a>(&'a self, boxes: &'a [Option<[i16; 4]>]) -> impl Iterator<Item = i16> + 'a {
        self.glyphs
            .iter()
            .zip(boxes)
            .map(|(g, b)| g.lsb.unwrap_or_else(|| b.map_or(0, |b| b[0])))
    }

    fn hmtx(&self, boxes: &[Option<[i16; 4]>]) -> Vec<u8> {
        self.glyphs
            .iter()
            .zip(self.bearings(boxes))
            .flat_map(|(g, lsb)| [g.advance.to_be_bytes(), lsb.to_be_bytes()])
            .flatten()
            .collect()
    }

    fn maxp(&self) -> Result<Vec<u8>, String> {
        if self.short_maxp {
            let mut t = vec![0, 0, 0x50, 0];
            t.extend((self.glyphs.len() as u16).to_be_bytes());
            return Ok(t);
        }
        let simple = || self.glyphs.iter().filter(|g| g.components.is_empty());
        let composite_sizes = (0..self.glyphs.len())
            .filter(|&i| !self.glyphs[i].components.is_empty())
            .map(|i| self.points(i, 0).map(|p| p.len()))
            .collect::<Result<Vec<_>, _>>()?;
        let computed: [usize; 13] = [
            simple()
                .map(|g| g.contours.iter().map(Vec::len).sum())
                .max()
                .unwrap_or(0),
            simple().map(|g| g.contours.len()).max().unwrap_or(0),
            composite_sizes.iter().copied().max().unwrap_or(0),
            self.composite_contours(),
            2,
            16,
            64,
            64,
            0,
            256,
            self.glyphs
                .iter()
                .map(|g| g.program.as_ref().map_or(0, Vec::len))
                .max()
                .unwrap_or(0),
            self.glyphs
                .iter()
                .map(|g| g.components.len())
                .max()
                .unwrap_or(0),
            usize::from(self.glyphs.iter().any(|g| !g.components.is_empty())),
        ];
        let mut t = vec![0, 1, 0, 0];
        t.extend((self.glyphs.len() as u16).to_be_bytes());
        for (key, computed) in MAXP_FIELDS.into_iter().zip(computed) {
            let value = match self.maxp.iter().rev().find(|(k, _)| k == key) {
                Some(&(_, v)) => v,
                None => u16::try_from(computed)
                    .map_err(|_| format!("maxp {key} {computed} does not fit"))?,
            };
            t.extend(value.to_be_bytes());
        }
        Ok(t)
    }

    fn composite_contours(&self) -> usize {
        fn contours(d: &Description, index: usize, depth: u32) -> usize {
            let g = &d.glyphs[index];
            if depth > 16 {
                return 0;
            }
            g.contours.len()
                + g.components
                    .iter()
                    .map(|c| contours(d, usize::from(c.glyph), depth + 1))
                    .sum::<usize>()
        }
        (0..self.glyphs.len())
            .filter(|&i| !self.glyphs[i].components.is_empty())
            .map(|i| contours(self, i, 0))
            .max()
            .unwrap_or(0)
    }

    /// A Windows Unicode BMP `cmap` (format 4) with each mapped character in a
    /// segment of its own.
    fn cmap(&self) -> Result<Vec<u8>, String> {
        let mut map: Vec<(u16, u16)> = Vec::new();
        for (glyph, g) in self.glyphs.iter().enumerate() {
            map.extend(g.chars.iter().map(|&c| (c, glyph as u16)));
        }
        map.sort_unstable();
        if map.windows(2).any(|w| w[0].0 == w[1].0) || map.iter().any(|&(c, _)| c == 0xFFFF) {
            return Err("characters must map once, and U+FFFF not at all".into());
        }
        let segments = map.len() + 1;
        let mut search = 1_usize;
        let mut selector = 0_u16;
        while search * 2 <= segments {
            search *= 2;
            selector += 1;
        }
        let mut sub = Vec::new();
        sub.extend(4_u16.to_be_bytes());
        sub.extend(((16 + 8 * segments) as u16).to_be_bytes());
        sub.extend([0, 0]); // language
        sub.extend(((2 * segments) as u16).to_be_bytes());
        sub.extend(((2 * search) as u16).to_be_bytes());
        sub.extend(selector.to_be_bytes());
        sub.extend(((2 * (segments - search)) as u16).to_be_bytes());
        sub.extend(map.iter().flat_map(|&(c, _)| c.to_be_bytes()));
        sub.extend(0xFFFF_u16.to_be_bytes());
        sub.extend([0, 0]); // reservedPad
        sub.extend(map.iter().flat_map(|&(c, _)| c.to_be_bytes()));
        sub.extend(0xFFFF_u16.to_be_bytes());
        sub.extend(
            map.iter()
                .flat_map(|&(c, g)| g.wrapping_sub(c).to_be_bytes()),
        );
        sub.extend(1_u16.to_be_bytes());
        sub.extend(std::iter::repeat_n(0, 2 * segments)); // idRangeOffset
        let mut t = vec![0, 0, 0, 1, 0, 3, 0, 1, 0, 0, 0, 12];
        t.extend(sub);
        Ok(t)
    }
}

/// The table `tag` of the first face of the font file `data`.
fn sfnt_table<'d>(data: &'d [u8], tag: &[u8; 4]) -> Option<&'d [u8]> {
    let u32_at = |i: usize| {
        data.get(i..i + 4)
            .map(|w| u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
    };
    let directory = if data.get(..4) == Some(b"ttcf") {
        u32_at(12)? as usize
    } else {
        0
    };
    let count = usize::from(u16::from_be_bytes(
        data.get(directory + 4..directory + 6)?.try_into().ok()?,
    ));
    (0..count).find_map(|i| {
        let record = directory + 12 + 16 * i;
        (data.get(record..record + 4)? == tag).then(|| {
            let (offset, len) = (u32_at(record + 8)? as usize, u32_at(record + 12)? as usize);
            data.get(offset..offset.checked_add(len)?)
        })?
    })
}

/// The box of `points`, `None` without points.
fn bounds(points: &[(i32, i32)]) -> Result<Option<[i16; 4]>, String> {
    let Some(&(x, y)) = points.first() else {
        return Ok(None);
    };
    let b = points.iter().fold([x, y, x, y], |[a, b, c, d], &(x, y)| {
        [a.min(x), b.min(y), c.max(x), d.max(y)]
    });
    let narrow = |v: i32| i16::try_from(v).map_err(|_| format!("glyph box value {v} out of range"));
    Ok(Some([
        narrow(b[0])?,
        narrow(b[1])?,
        narrow(b[2])?,
        narrow(b[3])?,
    ]))
}

fn os2(ascender: i16, descender: i16) -> Vec<u8> {
    let mut t = Vec::with_capacity(96);
    t.extend(4_u16.to_be_bytes()); // version
    t.extend([0, 0]); // xAvgCharWidth
    t.extend(400_u16.to_be_bytes()); // usWeightClass
    t.extend(5_u16.to_be_bytes()); // usWidthClass
    t.extend([0; 24]); // fsType, sub- and superscripts, strikeout, sFamilyClass
    t.extend([0; 10]); // panose
    t.extend([0; 16]); // ulUnicodeRange1..4
    t.extend(*b"TRGN");
    t.extend(0x0040_u16.to_be_bytes()); // fsSelection: REGULAR
    t.extend([0, 0, 0xFF, 0xFF]); // usFirstCharIndex, usLastCharIndex
    t.extend(ascender.to_be_bytes());
    t.extend(descender.to_be_bytes());
    t.extend([0, 0]); // sTypoLineGap
    t.extend(ascender.cast_unsigned().to_be_bytes()); // usWinAscent
    t.extend(descender.wrapping_neg().cast_unsigned().to_be_bytes()); // usWinDescent
    t.extend([0; 8]); // ulCodePageRange1, 2
    t.extend([0; 10]); // sxHeight, sCapHeight, usDefaultChar, usBreakChar, usMaxContext
    t
}

fn name() -> Vec<u8> {
    let records: [(u16, &str); 4] = [
        (1, "Triergon Probe"),
        (2, "Regular"),
        (4, "Triergon Probe Regular"),
        (6, "TriergonProbe-Regular"),
    ];
    let mut strings = Vec::new();
    let mut t = Vec::new();
    t.extend([0, 0]);
    t.extend((records.len() as u16).to_be_bytes());
    t.extend(((6 + 12 * records.len()) as u16).to_be_bytes());
    for (id, text) in records {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        t.extend([0, 3, 0, 1, 0x04, 0x09]); // Windows, Unicode BMP, English (US)
        t.extend(id.to_be_bytes());
        t.extend((utf16.len() as u16).to_be_bytes());
        t.extend((strings.len() as u16).to_be_bytes());
        strings.extend(utf16);
    }
    t.extend(strings);
    t
}

fn post() -> Vec<u8> {
    let mut t = vec![0, 3, 0, 0]; // version 3.0: no glyph names
    t.extend([0; 4]); // italicAngle
    t.extend((-100_i16).to_be_bytes()); // underlinePosition
    t.extend(50_i16.to_be_bytes()); // underlineThickness
    t.extend([0; 20]); // isFixedPitch, memory hints
    t
}

/// The font file: the table directory, then each table padded to four bytes,
/// with every checksum and `head`'s checksum adjustment.
fn sfnt(mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|t| t.0);
    let count = tables.len();
    let mut search = 1_usize;
    let mut selector = 0_u16;
    while search * 2 <= count {
        search *= 2;
        selector += 1;
    }
    let mut out = Vec::new();
    out.extend(0x0001_0000_u32.to_be_bytes());
    out.extend((count as u16).to_be_bytes());
    out.extend(((16 * search) as u16).to_be_bytes());
    out.extend(selector.to_be_bytes());
    out.extend(((16 * (count - search)) as u16).to_be_bytes());
    let mut offset = 12 + 16 * count;
    let mut head_at = None;
    for (tag, data) in &tables {
        if tag == b"head" {
            head_at = Some(offset);
        }
        out.extend(tag);
        out.extend(checksum(data).to_be_bytes());
        out.extend((offset as u32).to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if let Some(at) = head_at {
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum(&out));
        out[at + 8..at + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    out
}

/// The sum of the big-endian 32-bit words of `data`, zero padded.
fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0_u32, |sum, chunk| {
        let mut word = [0; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

fn numbers<T: TryFrom<i32>>(args: &[&str]) -> Result<Vec<T>, String> {
    args.iter()
        .map(|a| number(a).and_then(|n| T::try_from(n).map_err(|_| format!("{a} is out of range"))))
        .collect()
}

fn single<T: TryFrom<i32>>(args: &[&str]) -> Result<T, String> {
    match numbers(args)?.pop() {
        Some(v) if args.len() == 1 => Ok(v),
        _ => Err("expected one number".into()),
    }
}

/// The lines of a program up to `end`, assembled.
fn program<'a>(lines: &mut impl Iterator<Item = (usize, &'a str)>) -> Result<Vec<u8>, String> {
    let mut source = Vec::new();
    for (line, text) in lines.by_ref() {
        if text == "end" {
            return assemble(&source);
        }
        source.push((line, text));
    }
    Err("a program runs to the end of the description without `end`".into())
}

fn glyph<'a>(
    args: &[&str],
    lines: &mut impl Iterator<Item = (usize, &'a str)>,
) -> Result<Glyph, String> {
    let mut g = Glyph::default();
    if !args.len().is_multiple_of(2) {
        return Err("glyph [advance N] [lsb N] [char C]...".into());
    }
    for pair in args.chunks(2) {
        let value = &pair[1..];
        match pair[0] {
            "advance" => g.advance = single(value)?,
            "lsb" => g.lsb = Some(single(value)?),
            "char" => g.chars.push(single(value)?),
            other => return Err(format!("unknown glyph setting `{other}`")),
        }
    }
    while let Some((line, text)) = lines.next() {
        let fail = |e: String| format!("line {line}: {e}");
        let (key, rest) = text.split_once(' ').unwrap_or((text, ""));
        match key {
            "end" => {
                if !g.contours.is_empty() && !g.components.is_empty() {
                    return Err(fail("a glyph has contours or components, not both".into()));
                }
                return Ok(g);
            }
            "contour" => g.contours.push(contour(rest).map_err(fail)?),
            "component" => g.components.push(component(rest).map_err(fail)?),
            "bounds" => match numbers::<i16>(&rest.split_whitespace().collect::<Vec<_>>())
                .map_err(fail)?[..]
            {
                [a, b, c, d] => g.bounds = Some([a, b, c, d]),
                _ => return Err(fail("bounds XMIN YMIN XMAX YMAX".into())),
            },
            "program" => g.program = Some(program(lines)?),
            _ => return Err(fail(format!("unknown glyph line `{key}`"))),
        }
    }
    Err("a glyph runs to the end of the description without `end`".into())
}

fn contour(text: &str) -> Result<Vec<(i16, i16, bool)>, String> {
    let points = text
        .split(',')
        .map(|point| {
            let words: Vec<&str> = point.split_whitespace().collect();
            let (coordinates, on) = match words[..] {
                [x, y] => ([x, y], true),
                [x, y, "off"] => ([x, y], false),
                _ => return Err(format!("a point is `X Y` or `X Y off`, not `{point}`")),
            };
            let [x, y] = numbers::<i16>(&coordinates)?[..] else {
                unreachable!("two coordinates")
            };
            Ok((x, y, on))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if points.is_empty() {
        return Err("a contour needs a point".into());
    }
    Ok(points)
}

fn component(text: &str) -> Result<Component, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let (glyph, placement, rest) = match &words[..] {
        [glyph, kind, a, b, rest @ ..] => (*glyph, (*kind, *a, *b), rest),
        _ => return Err("component GLYPH (offset DX DY | anchor PARENT CHILD)...".into()),
    };
    let glyph = single(&[glyph])?;
    let args = (number(placement.1)?, number(placement.2)?);
    let mut flags = match placement.0 {
        "offset" => ARGS_ARE_XY_VALUES,
        "anchor" => 0,
        other => return Err(format!("unknown component placement `{other}`")),
    };
    let f2dot14 = |text: &str| -> Result<i16, String> {
        let v: f64 = text
            .parse()
            .map_err(|_| format!("`{text}` is not a number"))?;
        let scaled = (v * 16384.0).round();
        if (-32768.0..=32767.0).contains(&scaled) {
            Ok(scaled as i16)
        } else {
            Err(format!("{text} is out of the F2Dot14 range"))
        }
    };
    let mut matrix = None;
    let mut i = 0;
    while i < rest.len() {
        let is_number = |at: usize| rest.get(at).is_some_and(|t| t.parse::<f64>().is_ok());
        match rest[i] {
            "round" => flags |= ROUND_XY_TO_GRID,
            "use-metrics" => flags |= USE_MY_METRICS,
            "overlap" => flags |= OVERLAP_COMPOUND,
            "scaled-offset" => flags |= SCALED_COMPONENT_OFFSET,
            "unscaled-offset" => flags |= UNSCALED_COMPONENT_OFFSET,
            "scale" if is_number(i + 2) => {
                let (x, y) = (f2dot14(rest[i + 1])?, f2dot14(rest[i + 2])?);
                matrix = Some([x, 0, 0, y]);
                flags |= WE_HAVE_AN_X_AND_Y_SCALE;
                i += 2;
            }
            "scale" if is_number(i + 1) => {
                let s = f2dot14(rest[i + 1])?;
                matrix = Some([s, 0, 0, s]);
                flags |= WE_HAVE_A_SCALE;
                i += 1;
            }
            "matrix" if (1..=4).all(|k| is_number(i + k)) => {
                let m: Vec<i16> = rest[i + 1..i + 5]
                    .iter()
                    .map(|t| f2dot14(t))
                    .collect::<Result<_, _>>()?;
                matrix = Some([m[0], m[1], m[2], m[3]]);
                flags |= WE_HAVE_A_TWO_BY_TWO;
                i += 4;
            }
            "flags" if i + 1 < rest.len() => {
                flags |= single::<u16>(&rest[i + 1..i + 2])?;
                i += 1;
            }
            other => return Err(format!("unknown component option `{other}`")),
        }
        i += 1;
    }
    Ok(Component {
        glyph,
        args,
        flags,
        matrix,
    })
}

#[cfg(test)]
mod tests {
    use super::{Description, checksum};

    fn u16_at(data: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([data[at], data[at + 1]])
    }

    fn u32_at(data: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    }

    /// The tables of a font file by tag, checking each checksum.
    fn tables(font: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let count = usize::from(u16_at(font, 4));
        (0..count)
            .map(|i| {
                let record = 12 + 16 * i;
                let tag: [u8; 4] = font[record..record + 4].try_into().unwrap();
                let offset = u32_at(font, record + 8) as usize;
                let len = u32_at(font, record + 12) as usize;
                let data = font[offset..offset + len].to_vec();
                if &tag != b"head" {
                    assert_eq!(checksum(&data), u32_at(font, record + 4), "{tag:?}");
                }
                (tag, data)
            })
            .collect()
    }

    fn table<'a>(tables: &'a [([u8; 4], Vec<u8>)], tag: &[u8; 4]) -> &'a [u8] {
        &tables.iter().find(|(t, _)| t == tag).unwrap().1
    }

    const SQUARE: &str = "
        # A square with a program, and a composite of it.
        units-per-em 1000
        heights 16384 640
        maxp twilight 4 storage 8
        cvt 100 -20
        gasp 65535 0x000F
        fpgm
          PUSH 0; FDEF; ENDF
        end
        prep
        end
        glyph advance 600 char 0x41
          contour 100 0, 100 700, 500 700 off, 500 0
          program
            SVTCA[1]; PUSH 0; MDAP[1]
          end
        end
        glyph advance 600 lsb 7
          component 0 offset 50 -10 round use-metrics
          component 0 offset 0 0 scale 0.5
          program
            IUP[0]
          end
        end
        glyph advance 300
        end
    ";

    #[test]
    fn a_description_builds_a_consistent_face() {
        let d = Description::parse(SQUARE).unwrap();
        assert_eq!(d.heights, [16384, 640]);
        let font = d.build().unwrap();
        assert_eq!(font.len() % 4, 0);
        assert_eq!(checksum(&font), 0xB1B0_AFBA);
        let t = tables(&font);
        let tags: Vec<[u8; 4]> = t.iter().map(|(tag, _)| *tag).collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted);
        for tag in [
            b"cvt ", b"fpgm", b"prep", b"gasp", b"glyf", b"loca", b"cmap",
        ] {
            assert!(tags.contains(tag), "{tag:?}");
        }
        assert_eq!(table(&t, b"cvt "), [0, 100, 0xFF, 0xEC]);
        assert_eq!(table(&t, b"fpgm"), [0xB0, 0, 0x2C, 0x2D]);
        assert!(table(&t, b"prep").is_empty());
        let maxp = table(&t, b"maxp");
        // numGlyphs, maxPoints, maxContours, maxCompositePoints, maxCompositeContours.
        assert_eq!(&maxp[4..14], [0, 3, 0, 4, 0, 1, 0, 8, 0, 2]);
        // maxTwilightPoints, maxStorage, then maxSizeOfInstructions.
        assert_eq!(u16_at(maxp, 16), 4);
        assert_eq!(u16_at(maxp, 18), 8);
        assert_eq!(u16_at(maxp, 26), 4);
        let head = table(&t, b"head");
        assert_eq!(u16_at(head, 18), 1000);
        // The face box covers the composite: x 50..550, y -10..700.
        assert_eq!(&head[36..44], [0, 50, 0xFF, 0xF6, 0x02, 0x26, 0x02, 0xBC]);
        let hmtx = table(&t, b"hmtx");
        assert_eq!(hmtx, [2, 0x58, 0, 100, 2, 0x58, 0, 7, 1, 0x2C, 0, 0]);
    }

    #[test]
    fn glyphs_store_points_programs_and_components() {
        let font = Description::parse(SQUARE).unwrap().build().unwrap();
        let t = tables(&font);
        let (loca, glyf) = (table(&t, b"loca"), table(&t, b"glyf"));
        let at = |i: usize| u32_at(loca, 4 * i) as usize;
        // The empty last glyph has no data.
        assert_eq!(at(2), at(3));
        let simple = &glyf[at(0)..at(1)];
        assert_eq!(u16_at(simple, 0), 1);
        assert_eq!(u16_at(simple, 10), 3); // last point
        assert_eq!(u16_at(simple, 12), 4); // instruction length
        assert_eq!(&simple[14..18], [0x01, 0xB0, 0, 0x2F]);
        assert_eq!(&simple[18..22], [1, 1, 0, 1]); // flags
        // x deltas 100, 0, 400, 0; y deltas 0, 700, 0, -700.
        assert_eq!(&simple[22..30], [0, 100, 0, 0, 1, 0x90, 0, 0]);
        assert_eq!(&simple[30..38], [0, 0, 2, 0xBC, 0, 0, 0xFD, 0x44]);
        let composite = &glyf[at(1)..at(2)];
        assert_eq!(u16_at(composite, 0), 0xFFFF);
        // ARGS_ARE_WORDS | ARGS_ARE_XY_VALUES | ROUND_XY_TO_GRID | MORE_COMPONENTS
        // | USE_MY_METRICS, glyph 0, 50, -10.
        assert_eq!(&composite[10..18], [0x02, 0x27, 0, 0, 0, 50, 0xFF, 0xF6]);
        // The last: a scale, and the composite's program.
        assert_eq!(&composite[18..26], [0x01, 0x0B, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&composite[26..28], [0x20, 0]);
        assert_eq!(&composite[28..31], [0, 1, 0x30]);
    }

    #[test]
    fn a_version_0_5_maxp_holds_only_the_glyph_count() {
        let text = format!("maxp-version 0.5\n{SQUARE}");
        let font = Description::parse(&text).unwrap().build().unwrap();
        assert_eq!(table(&tables(&font), b"maxp"), [0, 0, 0x50, 0, 0, 3]);
    }

    #[test]
    fn mistakes_are_reported_with_their_line() {
        let fail = |text: &str| Description::parse(text).unwrap_err();
        assert!(
            fail("glyph advance 10\n  contour 0 0\n  component 0 offset 0 0\nend")
                .contains("not both")
        );
        assert!(fail("units-per-em 0\nglyph\nend").contains("zero"));
        assert!(fail("glyph\n  component 3 offset 0 0\nend").contains("not in the face"));
        assert!(fail("fpgm\n  FOO\nend").starts_with("line 2"));
        assert!(fail("maxp bogus 1\nglyph\nend").starts_with("line 1"));
        assert!(fail("prep\n  ADD").contains("without `end`"));
        assert!(fail("maxp-version 2\nglyph\nend").starts_with("line 1"));
    }
}
