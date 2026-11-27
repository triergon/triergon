use std::cell::RefCell;
use std::ffi::OsString;

use triergon::render::font::{Font, GlyphId, Limits, compat::libass};
use triergon::render::shape::{
    Metrics,
    compat::libass::{FaceRotations, GlyphRotations, SizedMetrics, rotates},
};
use triergon::{Budget, Bytes, WorkBudget};

use super::{Operation, row};

#[derive(Clone, Copy)]
enum First {
    Lower,
    Upper,
    Advance,
    Extents,
}

pub(super) fn prepare<'a>(
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [size, pair, first] = arguments else {
        return Err("expected font size, alias pair and first lookup".into());
    };
    let size: i32 = super::number(size.to_str())?;
    if !(1..=128).contains(&size) {
        return Err("font size out of range".into());
    }
    let (lower, upper) = match pair.to_str() {
        Some("capital") => ('Ə', 'Ә'),
        Some("small") => ('ə', 'ә'),
        _ => return Err("unknown alias pair".into()),
    };
    let first = match first.to_str() {
        Some("lower") => First::Lower,
        Some("upper") => First::Upper,
        Some("advance") => First::Advance,
        Some("extents") => First::Extents,
        _ => return Err("unknown first lookup".into()),
    };
    Ok(Operation {
        name: "libass-rotations",
        run: Box::new(move |work, mut trace| {
            let budget = Budget::new(16 << 20).map_err(|error| error.to_string())?;
            let data = Bytes::copy_from_slice(&budget, input).map_err(|error| error.to_string())?;
            let font = Font::new(data, 0, &budget, Limits::default())
                .map_err(|error| error.to_string())?;
            let sized = libass::at_size(&font, size * 64).map_err(|error| error.to_string())?;
            let mut budgeted_work = WorkBudget::new(1 << 22);
            let metrics =
                SizedMetrics::new(sized, &mut budgeted_work).map_err(|error| error.to_string())?;
            let glyph = metrics.nominal_glyph(lower).ok_or("missing font alias")?;
            let upper_glyph = metrics.nominal_glyph(upper);
            for code in [
                0,
                u32::from(lower),
                0x2f0,
                0x2f1,
                u32::from(upper),
                0x10ffff,
            ] {
                row(
                    &mut trace,
                    work,
                    3,
                    &[0, u64::from(code), u64::from(rotates(code))],
                    &[],
                )?;
            }
            let glyph_word = |glyph: Option<GlyphId>| glyph.map_or(0, |id| u64::from(id.0));
            row(
                &mut trace,
                work,
                3,
                &[1, u64::from(glyph.0), glyph_word(upper_glyph)],
                &[],
            )?;

            let rotations = RefCell::new(GlyphRotations::new(&budget));
            let run_face = |key, first, work: &mut _, trace: &mut _| -> Result<(), String> {
                let face = FaceRotations::new(&rotations, key);
                let mut budgeted_work = WorkBudget::new(1 << 22);
                let metrics = SizedMetrics::new(sized, &mut budgeted_work)
                    .map_err(|error| error.to_string())?
                    .rotating(&face);
                let nominal = |code, work: &mut _, trace: &mut _| {
                    let found = metrics.nominal_glyph(code);
                    row(
                        trace,
                        work,
                        3,
                        &[2, u64::from(u32::from(code)), glyph_word(found)],
                        &[],
                    )
                };
                let advance = |work: &mut _, trace: &mut _| {
                    let advance = metrics
                        .h_advance(glyph)
                        .map_err(|error| error.to_string())?;
                    row(trace, work, 3, &[3, advance as u64], &[])
                };
                let extents = |work: &mut _, trace: &mut _| {
                    let extent = metrics.extents(glyph).map_err(|error| error.to_string())?;
                    let words = extent.map_or_else(
                        || vec![4, 0],
                        |e| {
                            vec![
                                4,
                                1,
                                e.x_bearing as u64,
                                e.y_bearing as u64,
                                e.width as u64,
                                e.height as u64,
                            ]
                        },
                    );
                    row(trace, work, 3, &words, &[])
                };
                match first {
                    First::Lower => nominal(lower, work, trace)?,
                    First::Upper => nominal(upper, work, trace)?,
                    First::Advance => advance(work, trace)?,
                    First::Extents => extents(work, trace)?,
                }
                nominal(upper, work, trace)?;
                nominal(lower, work, trace)?;
                advance(work, trace)?;
                extents(work, trace)?;
                advance(work, trace)?;
                metrics.finish().map_err(|error| error.to_string())?;
                row(trace, work, 3, &[5], &[])
            };
            run_face(0_u8, first, work, &mut trace)?;
            // A new handle to the same key must keep its earlier decision, and
            // a second key can start with a different one.
            run_face(0, First::Upper, work, &mut trace)?;
            run_face(1, First::Upper, work, &mut trace)
        }),
    })
}
