//! Arabic shaping for fonts without Arabic GSUB features: HarfBuzz builds single
//! and ligature substitutions from the font's Arabic Presentation Forms glyphs and
//! applies them under the joining-form and `rlig` masks. The lookups are built
//! here as OpenType data and applied like the font's own.

use crate::buffer::Buffer;
use crate::face::Face;
use crate::metrics::Scaled;
use crate::ot::apply::{Apply, Stage};
use crate::ot::data::Data;
use crate::ot::digest::Digest;
use crate::ot::layout::Lookup;
use crate::plan::Plan;
use crate::unicode::{
    ArabicForm, ArabicLigatureSet, arabic_ligatures, arabic_ligatures_3, arabic_mark_ligatures,
    arabic_presentation_forms,
};
use triergon_core::{Error, Result};

/// The IgnoreMarks lookup flag, as the synthesised lookups are written.
const IGNORE_MARKS: u16 = 0x0008;

/// A lookup under construction, big-endian, in a fixed buffer: the synthesized
/// lookups are bounded by the static Arabic tables.
struct Writer {
    data: [u8; 4096],
    len: usize,
}

impl Writer {
    const fn new() -> Self {
        Self {
            data: [0; 4096],
            len: 0,
        }
    }

    fn u16(&mut self, value: impl TryInto<u16>) -> Result<()> {
        let value: u16 = value.try_into().map_err(|_| Error::Overflow)?;
        let end = self.len + 2;
        self.data
            .get_mut(self.len..end)
            .ok_or(Error::Overflow)?
            .copy_from_slice(&value.to_be_bytes());
        self.len = end;
        Ok(())
    }

    /// Writes `value` at an earlier position `at`.
    fn patch(&mut self, at: usize, value: usize) -> Result<()> {
        let value = u16::try_from(value).map_err(|_| Error::Overflow)?;
        self.data[at..at + 2].copy_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// A lookup header of `kind` with one subtable, which follows it.
    fn lookup_header(&mut self, kind: u16, flags: u16) -> Result<()> {
        self.u16(kind)?;
        self.u16(flags)?;
        self.u16(1_u16)?;
        self.u16(8_u16)
    }

    fn coverage(&mut self, glyphs: impl ExactSizeIterator<Item = u32>) -> Result<()> {
        self.u16(1_u16)?;
        self.u16(glyphs.len())?;
        for glyph in glyphs {
            self.u16(glyph)?;
        }
        Ok(())
    }
}

/// A single substitution from each character to its `form`, for the characters
/// whose both glyphs the font has.
fn single_lookup(font: &Scaled<'_>, form: ArabicForm) -> Result<Option<Writer>> {
    let glyph = |c: u32| font.glyph(c);
    let mut pairs = [(0_u32, 0_u32); 128];
    let mut count = 0;
    for (u, s) in arabic_presentation_forms(form) {
        let (Some(u_glyph), Some(s_glyph)) = (glyph(u), glyph(s)) else {
            continue;
        };
        if u_glyph == s_glyph || u_glyph > 0xffff || s_glyph > 0xffff {
            continue;
        }
        *pairs.get_mut(count).ok_or(Error::Overflow)? = (u_glyph, s_glyph);
        count += 1;
    }
    if count == 0 {
        return Ok(None);
    }
    let pairs = &mut pairs[..count];
    pairs.sort_by_key(|&(u, _)| u);
    let mut w = Writer::new();
    w.lookup_header(1, IGNORE_MARKS)?;
    let subtable = w.len;
    w.u16(2_u16)?;
    let coverage_at = w.len;
    w.u16(0_u16)?;
    w.u16(count)?;
    for &(_, s) in pairs.iter() {
        w.u16(s)?;
    }
    w.patch(coverage_at, w.len - subtable)?;
    w.coverage(pairs.iter().map(|&(u, _)| u))?;
    Ok(Some(w))
}

/// A ligature substitution from one of HarfBuzz's Arabic ligature tables, for
/// the ligatures whose glyphs the font has.
fn ligature_lookup<const N: usize>(
    font: &Scaled<'_>,
    table: &[ArabicLigatureSet<N>],
    flags: u16,
) -> Result<Option<Writer>> {
    let glyph = |c: u32| font.glyph(c);
    // (first glyph, index of its set) in glyph order, stably.
    let mut firsts = [(0_u32, 0_usize); 32];
    let mut first_count = 0;
    for (i, set) in table.iter().enumerate() {
        if let Some(g) = glyph(set.first) {
            *firsts.get_mut(first_count).ok_or(Error::Overflow)? = (g, i);
            first_count += 1;
        }
    }
    let firsts = &mut firsts[..first_count];
    firsts.sort_by_key(|&(g, _)| g);
    // The ligatures of each first glyph that the font supports.
    let supported = |set: &ArabicLigatureSet<N>| {
        set.ligatures.iter().filter_map(move |l| {
            let ligature = glyph(l.ligature)?;
            let mut components = [0_u32; N];
            for (c, &u) in components.iter_mut().zip(&l.components) {
                *c = if u == 0 { None } else { glyph(u) }?;
            }
            Some((ligature, components))
        })
    };
    if firsts
        .iter()
        .all(|&(_, i)| supported(&table[i]).next().is_none())
    {
        return Ok(None);
    }
    let mut w = Writer::new();
    w.lookup_header(4, flags)?;
    let subtable = w.len;
    w.u16(1_u16)?;
    let coverage_at = w.len;
    w.u16(0_u16)?;
    w.u16(firsts.len())?;
    let sets_at = w.len;
    for _ in firsts.iter() {
        w.u16(0_u16)?;
    }
    for (k, &(_, i)) in firsts.iter().enumerate() {
        let set = w.len;
        w.patch(sets_at + 2 * k, set - subtable)?;
        let count = supported(&table[i]).count();
        w.u16(count)?;
        let offsets = w.len;
        for _ in 0..count {
            w.u16(0_u16)?;
        }
        for (n, (ligature, components)) in supported(&table[i]).enumerate() {
            w.patch(offsets + 2 * n, w.len - set)?;
            w.u16(ligature)?;
            w.u16(N + 1)?;
            for c in components {
                w.u16(c)?;
            }
        }
    }
    w.patch(coverage_at, w.len - subtable)?;
    w.coverage(firsts.iter().map(|&(g, _)| g))?;
    Ok(Some(w))
}

/// Whether the font looks like an old Windows-1256 Arabic font, for which
/// HarfBuzz uses fixed lookups this crate does not carry.
fn is_win1256(font: &Scaled<'_>) -> bool {
    [
        (0x0627, 199),
        (0x0644, 225),
        (0x0649, 236),
        (0x064A, 237),
        (0x0652, 250),
    ]
    .iter()
    .all(|&(c, g)| font.glyph(c) == Some(g))
}

/// Shapes with synthesized lookups: `init`, `medi`, `fina` and `isol` forms,
/// then three-, two-part and mark ligatures under `rlig`.
pub(super) fn shape(
    plan: &Plan,
    face: &Face<'_>,
    buffer: &mut Buffer,
    font: &Scaled<'_>,
) -> Result<()> {
    let forms = [
        (b"init", ArabicForm::Initial),
        (b"medi", ArabicForm::Medial),
        (b"fina", ArabicForm::Final),
        (b"isol", ArabicForm::Isolated),
    ];
    let rlig = plan.one_mask(b"rlig");
    let mut lookups: [Option<(u32, Writer)>; 7] = Default::default();
    for (slot, &(tag, form)) in lookups.iter_mut().zip(&forms) {
        let mask = plan.one_mask(tag);
        if mask != 0 {
            *slot = single_lookup(font, form)?.map(|w| (mask, w));
        }
    }
    if rlig != 0 {
        lookups[4] = ligature_lookup(font, arabic_ligatures_3(), IGNORE_MARKS)?.map(|w| (rlig, w));
        lookups[5] = ligature_lookup(font, arabic_ligatures(), IGNORE_MARKS)?.map(|w| (rlig, w));
        lookups[6] = ligature_lookup(font, arabic_mark_ligatures(), 0)?.map(|w| (rlig, w));
    }
    if lookups.iter().all(Option::is_none) {
        if is_win1256(font) {
            return Err(Error::Unsupported("Windows-1256 Arabic fallback shaping"));
        }
        return Ok(());
    }
    let mut ctx = Apply::new(
        Stage::Substitute,
        face.gsub,
        &face.gdef,
        buffer,
        font,
        plan.direction,
    );
    for (mask, lookup) in lookups.iter().flatten() {
        let lookup = Lookup::new(Stage::Substitute, Data(lookup.bytes()));
        ctx.auto_zwj = true;
        ctx.auto_zwnj = true;
        let digest = Digest::of_lookup(&lookup, ctx.buffer.work())?;
        crate::shape::apply_lookup_masked(&mut ctx, &lookup, *mask, digest)?;
    }
    Ok(())
}
