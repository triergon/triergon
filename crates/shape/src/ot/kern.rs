//! The legacy `kern` table, applied the way HarfBuzz applies it when GPOS has no
//! kerning: format 0 pair subtables over adjacent non-mark glyphs in visual
//! order, each kerning value split between the two glyphs.

use super::apply::{Apply, Next, Values};
use super::data::{Data, bsearch};
use super::layout::LookupFlags;
use triergon_core::{Error, Result};

/// The OpenType (Microsoft) `kern` table.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Kern<'a> {
    data: Data<'a>,
    /// Apple's version 1 table, which this crate does not apply.
    apple: bool,
}

const HORIZONTAL: u16 = 0x01;
const CROSS_STREAM: u16 = 0x04;

impl<'a> Kern<'a> {
    pub(crate) fn new(table: Option<&'a [u8]>) -> Self {
        let data = Data(table.unwrap_or_default());
        Self {
            data,
            apple: data.u16(0) == 1,
        }
    }

    /// HarfBuzz reads a table with a nonzero first 32-bit word as kerning.
    pub(crate) fn has_data(&self) -> bool {
        self.data.u32(0) != 0
    }

    /// OpenType `kern` subtables are never state machines.
    pub(crate) fn has_state_machine(&self) -> bool {
        self.apple
    }

    pub(crate) fn has_cross_stream(&self) -> bool {
        !self.apple
            && self
                .subtables()
                .any(|(coverage, _, _)| coverage & CROSS_STREAM != 0)
    }

    /// (coverage, format, subtable) of each OpenType subtable.
    fn subtables(&self) -> impl Iterator<Item = (u16, u8, Data<'a>)> + 'a {
        let data = self.data;
        let count = usize::from(data.u16(2));
        let mut at = 4;
        (0..count).map_while(move |_| {
            let length = usize::from(data.u16(at + 2));
            let coverage = data.u16(at + 4);
            let subtable = Data(data.0.get(at..)?);
            at = at.checked_add(length.max(6))?;
            Some((coverage & 0xff, (coverage >> 8) as u8, subtable))
        })
    }

    /// Applies every horizontal subtable to the positioned buffer.
    pub(crate) fn apply(&self, ctx: &mut Apply<'a, '_>, kern_mask: u32) -> Result<()> {
        if self.apple {
            return Err(Error::Unsupported("Apple kern table"));
        }
        let backward = ctx.direction.is_backward();
        let mut reversed = false;
        for (coverage, format, subtable) in self.subtables() {
            if coverage & HORIZONTAL == 0 {
                continue;
            }
            if coverage & CROSS_STREAM != 0 || format != 0 {
                return Err(Error::Unsupported("kern subtable"));
            }
            // Pairs are kerned in visual order.
            if backward != reversed {
                ctx.buffer.reverse();
                reversed = backward;
            }
            if kern_mask != 0 {
                kern_pairs(ctx, subtable, kern_mask)?;
            }
        }
        if reversed {
            ctx.buffer.reverse();
        }
        Ok(())
    }
}

/// The kerning value of the pair in a format 0 subtable.
fn pair_value(subtable: Data<'_>, left: u32, right: u32) -> i32 {
    let count = subtable.count(6, 14, 6);
    let key = (left << 16) | right;
    bsearch(count, |i| subtable.u32(14 + 6 * i).cmp(&key))
        .map_or(0, |i| i32::from(subtable.i16(14 + 6 * i + 4)))
}

fn kern_pairs(ctx: &mut Apply<'_, '_>, subtable: Data<'_>, kern_mask: u32) -> Result<()> {
    kern_machine(ctx, kern_mask, true, |left, right| {
        pair_value(subtable, left, right)
    })
}

/// HarfBuzz's kerning machine: each glyph under `kern_mask` and the next non-mark
/// glyph are kerned by `value` (scaled from font units when `scale`), half of it on
/// each side.
pub(crate) fn kern_machine(
    ctx: &mut Apply<'_, '_>,
    kern_mask: u32,
    scale: bool,
    value: impl Fn(u32, u32) -> i32,
) -> Result<()> {
    ctx.set_lookup_mask(kern_mask);
    ctx.set_lookup_flags(LookupFlags::IGNORE_MARKS);
    let m = ctx.matcher(false, Values::Any);
    let count = ctx.buffer.len();
    let mut idx = 0;
    while idx < count {
        ctx.buffer.charge(1)?;
        if ctx.buffer.infos()[idx].mask & kern_mask == 0 {
            idx += 1;
            continue;
        }
        let j = match ctx.next_or_stop(&m, idx, 0)? {
            Next::Match(j) => j,
            Next::Stop(stop) => {
                idx = stop;
                continue;
            }
        };
        let infos = ctx.buffer.infos();
        let kern = value(infos[idx].glyph, infos[j].glyph);
        if kern != 0 {
            let kern = if scale {
                ctx.font.em_scale_x(kern)
            } else {
                kern
            };
            let (first, second) = (kern >> 1, kern - (kern >> 1));
            let pos = ctx.buffer.pos.as_mut_slice();
            pos[idx].position.x_advance = pos[idx].position.x_advance.saturating_add(first);
            pos[j].position.x_advance = pos[j].position.x_advance.saturating_add(second);
            pos[j].position.x_offset = pos[j].position.x_offset.saturating_add(second);
        }
        idx = j;
    }
    Ok(())
}
