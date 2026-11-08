//! Glyph substitution subtables.

use super::apply::{Apply, Values};
use super::context;
use super::data::Data;
use super::layout::{LookupType, coverage};
use crate::buffer::{GlyphClass, LigProps};
use triergon_core::Result;

/// The largest value of a feature with several values (`rand` uses it).
pub(crate) const MAX_VALUE: u32 = 255;

/// Applies one subtable of type `kind` at the current glyph.
pub(crate) fn apply<'a>(ctx: &mut Apply<'a, '_>, kind: LookupType, data: Data<'a>) -> Result<bool> {
    match kind {
        LookupType::Single => single(ctx, data),
        LookupType::Multiple => multiple(ctx, data),
        LookupType::Alternate => alternate(ctx, data),
        LookupType::Ligature => ligature(ctx, data),
        LookupType::Context => context::apply(ctx, data, false),
        LookupType::ChainContext => context::apply(ctx, data, true),
        LookupType::ReverseChain => reverse_chain(ctx, data),
        _ => Ok(false),
    }
}

/// The coverage index of the current glyph in the subtable's coverage at offset 2.
fn covered(ctx: &Apply<'_, '_>, data: Data<'_>) -> Option<usize> {
    coverage(data.offset16(2), ctx.buffer.cur().glyph).map(|i| i as usize)
}

/// The `index`th of the counted array of 16-bit offsets at `at`.
fn nth_offset(data: Data<'_>, at: usize, index: usize) -> Option<Data<'_>> {
    (index < data.count(at, at + 2, 2)).then(|| data.offset16(at + 2 + 2 * index))
}

fn single(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> Result<bool> {
    let Some(index) = covered(ctx, data) else {
        return Ok(false);
    };
    let glyph = match data.u16(0) {
        1 => (ctx.buffer.cur().glyph as u16).wrapping_add(data.u16(4)),
        2 if index < data.count(4, 6, 2) => data.u16(6 + 2 * index),
        _ => return Ok(false),
    };
    ctx.replace_glyph(u32::from(glyph))?;
    Ok(true)
}

fn multiple(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> Result<bool> {
    let Some(index) = covered(ctx, data) else {
        return Ok(false);
    };
    if data.u16(0) != 1 {
        return Ok(false);
    }
    // As in HarfBuzz, a missing sequence is an empty one, which deletes the glyph.
    let sequence = nth_offset(data, 4, index).unwrap_or(Data::EMPTY);
    let count = sequence.count(0, 2, 2);
    match count {
        0 => ctx.buffer.delete_glyph()?,
        1 => ctx.replace_glyph(u32::from(sequence.u16(2)))?,
        _ => {
            ctx.buffer.charge(count)?;
            let class_guess = ctx.buffer.cur().is_ligature().then_some(GlyphClass::Base);
            // A glyph that is part of a ligature keeps its ligature identity.
            let attached = ctx.buffer.cur().lig.id() != 0;
            for i in 0..count {
                if !attached {
                    ctx.buffer.cur_mut().lig = LigProps::mark(0, i as u8);
                }
                ctx.output_glyph_for_component(u32::from(sequence.u16(2 + 2 * i)), class_guess)?;
            }
            ctx.buffer.skip_glyph();
        }
    }
    Ok(true)
}

fn alternate(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> Result<bool> {
    let Some(index) = covered(ctx, data) else {
        return Ok(false);
    };
    let Some(set) = nth_offset(data, 4, index).filter(|_| data.u16(0) == 1) else {
        return Ok(false);
    };
    let count = set.count(0, 2, 2);
    if count == 0 {
        return Ok(false);
    }
    let mask = ctx.lookup_mask;
    let mut choice = (mask & ctx.buffer.cur().mask) >> mask.trailing_zeros();
    if choice == MAX_VALUE && ctx.random {
        choice = ctx.buffer.random_number() % count as u32 + 1;
    }
    if choice == 0 || choice as usize > count {
        return Ok(false);
    }
    ctx.replace_glyph(u32::from(set.u16(2 * choice as usize)))?;
    Ok(true)
}

fn ligature<'a>(ctx: &mut Apply<'a, '_>, data: Data<'a>) -> Result<bool> {
    let Some(index) = covered(ctx, data) else {
        return Ok(false);
    };
    let Some(set) = nth_offset(data, 4, index).filter(|_| data.u16(0) == 1) else {
        return Ok(false);
    };
    for l in 0..set.count(0, 2, 2) {
        ctx.buffer.charge(1)?;
        let ligature = set.offset16(2 + 2 * l);
        let glyph = u32::from(ligature.u16(0));
        let count = ligature.headless_count(2, 4, 2);
        if count == 0 {
            continue;
        }
        if count == 1 {
            ctx.replace_glyph(glyph)?;
            return Ok(true);
        }
        let Some((match_end, total)) = ctx.match_input(count, Values::Glyphs(ligature.at(4)))?
        else {
            continue;
        };
        ctx.ligate(count, match_end, glyph, total)?;
        return Ok(true);
    }
    Ok(false)
}

fn reverse_chain<'a>(ctx: &mut Apply<'a, '_>, data: Data<'a>) -> Result<bool> {
    if !ctx.is_outermost() || data.u16(0) != 1 {
        return Ok(false);
    }
    let Some(index) = covered(ctx, data) else {
        return Ok(false);
    };
    let backtracks = data.count(4, 6, 2);
    let lookahead_at = 6 + 2 * backtracks;
    let lookaheads = data.count(lookahead_at, lookahead_at + 2, 2);
    let substitutes_at = lookahead_at + 2 + 2 * lookaheads;
    if index >= data.count(substitutes_at, substitutes_at + 2, 2) {
        return Ok(false);
    }
    let idx = ctx.buffer.idx;
    if ctx.match_backtrack(backtracks, Values::Coverages(data.at(6), data))?
        && ctx.match_lookahead(
            lookaheads,
            Values::Coverages(data.at(lookahead_at + 2), data),
            idx + 1,
        )?
    {
        let glyph = data.u16(substitutes_at + 2 + 2 * index);
        // The main loop moves on; the glyph is replaced where it is.
        ctx.replace_glyph_inplace(u32::from(glyph));
        return Ok(true);
    }
    Ok(false)
}
