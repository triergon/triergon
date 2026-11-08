//! Glyph substitution subtables.

use super::apply::{Apply, Values};
use super::context::{apply_chain_context, apply_context};
use super::data::Data;
use super::layout::coverage;
use crate::buffer::{LigProps, glyph_props};
use triergon_core::Result;

const SINGLE: u16 = 1;
const MULTIPLE: u16 = 2;
const ALTERNATE: u16 = 3;
const LIGATURE: u16 = 4;
const CONTEXT: u16 = 5;
const CHAIN_CONTEXT: u16 = 6;
const EXTENSION: u16 = 7;
pub(crate) const REVERSE_CHAIN: u16 = 8;

/// The largest value of a feature with several values (`rand` uses it).
pub(crate) const MAX_VALUE: u32 = 255;

/// Applies one subtable of lookup type `kind` at the current glyph.
pub(crate) fn apply<'a>(ctx: &mut Apply<'a, '_>, kind: u16, data: Data<'a>) -> Result<bool> {
    match kind {
        SINGLE => single(ctx, data),
        MULTIPLE => multiple(ctx, data),
        ALTERNATE => alternate(ctx, data),
        LIGATURE => ligature(ctx, data),
        CONTEXT => apply_context(ctx, data),
        CHAIN_CONTEXT => apply_chain_context(ctx, data),
        EXTENSION if data.u16(0) == 1 && data.u16(2) != EXTENSION => {
            apply(ctx, data.u16(2), data.offset32(4))
        }
        REVERSE_CHAIN => reverse_chain(ctx, data),
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
            let class_guess = if ctx.buffer.cur().is_ligature() {
                glyph_props::BASE_GLYPH
            } else {
                0
            };
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
        let count = usize::from(ligature.u16(2));
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
    let backtracks = usize::from(data.u16(4));
    let lookahead_at = 6 + 2 * backtracks;
    let lookaheads = usize::from(data.u16(lookahead_at));
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
