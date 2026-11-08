//! Mark positioning without GPOS: marks are placed around their base by
//! combining class and glyph extents, as HarfBuzz's fallback does.

use crate::buffer::{Buffer, Info, Position};
use crate::metrics::{Extents, Scaled};
use crate::tag::Direction;
use crate::unicode::GeneralCategory;

const ATTACHED_BELOW_LEFT: u8 = 200;
const ATTACHED_BELOW: u8 = 202;
const ATTACHED_ABOVE: u8 = 214;
const ATTACHED_ABOVE_RIGHT: u8 = 216;
const BELOW_LEFT: u8 = 218;
const BELOW: u8 = 220;
const BELOW_RIGHT: u8 = 222;
const ABOVE_LEFT: u8 = 228;
const ABOVE: u8 = 230;
const ABOVE_RIGHT: u8 = 232;
const DOUBLE_BELOW: u8 = 233;
const DOUBLE_ABOVE: u8 = 234;

/// The placement class of a mark: script-specific classes (Hebrew, Arabic, Thai,
/// Lao, Tibetan) mapped to the positional ones.
fn placement_class(c: u32, class: u8) -> u8 {
    if class >= 200 {
        return class;
    }
    let class = match (c & !0xff == 0x0e00, class, c) {
        (true, 0, 0x0E31 | 0x0E34..=0x0E37 | 0x0E47 | 0x0E4C..=0x0E4E) => ABOVE_RIGHT,
        (true, 0, 0x0EB1 | 0x0EB4..=0x0EB7 | 0x0EBB | 0x0ECC | 0x0ECD) => ABOVE,
        (true, 0, 0x0EBC) => BELOW,
        (true, c_class, 0x0E3A) if c_class != 0 => BELOW_RIGHT,
        _ => class,
    };
    match class {
        // Hebrew: sheva to qubuts and meteg below, rafe on top, the dots.
        15..=25 => BELOW,
        13 => ATTACHED_ABOVE,
        10 => ABOVE_RIGHT,
        11 | 14 => ABOVE_LEFT,
        26 => ABOVE,
        // Arabic and Syriac.
        28 | 29 | 31 | 32 | 27 | 34..=36 => ABOVE,
        30 | 33 => BELOW,
        // Thai, Lao, Tibetan.
        3 => BELOW_RIGHT,
        107 => ABOVE_RIGHT,
        118 => BELOW,
        122 => ABOVE,
        129 => BELOW,
        132 => ABOVE,
        131 => BELOW,
        class => class,
    }
}

/// Gives non-spacing marks their placement class before glyphs are mapped.
pub(crate) fn recategorize_marks(buffer: &mut Buffer) {
    for info in buffer.infos_mut() {
        if info.category() == GeneralCategory::NonSpacingMark {
            let class = placement_class(info.glyph, info.combining_class());
            info.set_combining_class(class);
        }
    }
}

/// Positions every mark cluster that GPOS did not.
pub(crate) fn position_marks(
    buffer: &mut Buffer,
    font: &Scaled<'_>,
    direction: Direction,
    plan_direction: Direction,
    adjust_offsets: bool,
) {
    let starts = |i: &Info| !i.is_unicode_mark() && !i.is_hidden() && !i.is_default_ignorable();
    let mut start = 0;
    for i in 1..buffer.len() {
        if starts(&buffer.infos()[i]) {
            position_cluster(
                buffer,
                font,
                direction,
                plan_direction,
                start,
                i,
                adjust_offsets,
            );
            start = i;
        }
    }
    let len = buffer.len();
    position_cluster(
        buffer,
        font,
        direction,
        plan_direction,
        start,
        len,
        adjust_offsets,
    );
}

fn position_cluster(
    buffer: &mut Buffer,
    font: &Scaled<'_>,
    direction: Direction,
    plan_direction: Direction,
    start: usize,
    end: usize,
    adjust_offsets: bool,
) {
    if end - start < 2 {
        return;
    }
    let mut i = start;
    while i < end {
        if buffer.infos()[i].is_unicode_mark() {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < end {
            let info = &buffer.infos()[j];
            if !info.is_hidden() && !info.is_default_ignorable() && !info.is_unicode_mark() {
                break;
            }
            j += 1;
        }
        position_around_base(
            buffer,
            font,
            direction,
            plan_direction,
            i,
            j,
            adjust_offsets,
        );
        i = j;
    }
}

fn position_around_base(
    buffer: &mut Buffer,
    font: &Scaled<'_>,
    direction: Direction,
    plan_direction: Direction,
    base: usize,
    end: usize,
    adjust_offsets: bool,
) {
    let base_info = buffer.infos()[base];
    let Some(mut base_extents) = font.extents(base_info.glyph) else {
        // Without extents marks only lose their advance.
        for k in base + 1..end {
            if buffer.infos()[k].category() == GeneralCategory::NonSpacingMark {
                let p = &mut buffer.pos.as_mut_slice()[k].position;
                if adjust_offsets {
                    p.x_offset = p.x_offset.saturating_sub(p.x_advance);
                    p.y_offset = p.y_offset.saturating_sub(p.y_advance);
                }
                p.x_advance = 0;
                p.y_advance = 0;
            }
        }
        return;
    };
    let base_pos = buffer.pos.as_slice()[base].position;
    base_extents.y_bearing = base_extents.y_bearing.saturating_add(base_pos.y_offset);
    // The mark goes on the advance box, not the ink.
    base_extents.x_bearing = 0;
    base_extents.width = font.advance(base_info.glyph);
    let lig_id = base_info.lig.id();
    let components = i64::from(base_info.lig_components());
    let (mut x_offset, mut y_offset) = if direction.is_backward() {
        (0, 0)
    } else {
        (
            base_pos.x_advance.saturating_neg(),
            base_pos.y_advance.saturating_neg(),
        )
    };
    let mut component_extents = base_extents;
    let mut last_component = -1_i64;
    let mut last_class = 255_u8;
    let mut cluster_extents = base_extents;
    for k in base + 1..end {
        let info = buffer.infos()[k];
        let class = info.combining_class();
        if class == 0 {
            let p = buffer.pos.as_slice()[k].position;
            if direction.is_backward() {
                x_offset = x_offset.saturating_add(p.x_advance);
                y_offset = y_offset.saturating_add(p.y_advance);
            } else {
                x_offset = x_offset.saturating_sub(p.x_advance);
                y_offset = y_offset.saturating_sub(p.y_advance);
            }
            continue;
        }
        if components > 1 {
            let mut component = i64::from(info.lig.component()) - 1;
            if lig_id == 0 || lig_id != info.lig.id() || component >= components {
                component = components - 1;
            }
            if last_component != component {
                last_component = component;
                last_class = 255;
                component_extents = base_extents;
                let width = i64::from(component_extents.width);
                let slot = if plan_direction == Direction::LeftToRight {
                    component
                } else {
                    components - 1 - component
                };
                component_extents.x_bearing =
                    clamp(i64::from(component_extents.x_bearing) + slot * width / components);
                component_extents.width = (width / components) as i32;
            }
        }
        if last_class != class {
            last_class = class;
            cluster_extents = component_extents;
        }
        let pos = &mut buffer.pos.as_mut_slice()[k].position;
        position_mark(
            font,
            info.glyph,
            direction,
            &mut cluster_extents,
            pos,
            class,
        );
        pos.x_advance = 0;
        pos.y_advance = 0;
        pos.x_offset = pos.x_offset.saturating_add(x_offset);
        pos.y_offset = pos.y_offset.saturating_add(y_offset);
    }
}

fn clamp(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Places one mark against `base` and grows `base` by it, so that stacked marks
/// of one class go outwards.
fn position_mark(
    font: &Scaled<'_>,
    glyph: u32,
    direction: Direction,
    base: &mut Extents,
    pos: &mut Position,
    class: u8,
) {
    let Some(mark) = font.extents(glyph) else {
        return;
    };
    let y_gap = font.scale.y / 16;
    pos.x_offset = 0;
    pos.y_offset = 0;
    let (bx, bw, mx, mw) = (
        i64::from(base.x_bearing),
        i64::from(base.width),
        i64::from(mark.x_bearing),
        i64::from(mark.width),
    );
    pos.x_offset = match class {
        DOUBLE_BELOW | DOUBLE_ABOVE if direction == Direction::LeftToRight => {
            clamp(bx + bw - mw / 2 - mx)
        }
        DOUBLE_BELOW | DOUBLE_ABOVE => clamp(bx - mw / 2 - mx),
        ATTACHED_BELOW_LEFT | BELOW_LEFT | ABOVE_LEFT => {
            base.x_bearing.saturating_sub(mark.x_bearing)
        }
        ATTACHED_ABOVE_RIGHT | BELOW_RIGHT | ABOVE_RIGHT => clamp(bx + bw - mw - mx),
        _ => clamp(bx + (bw - mw) / 2 - mx),
    };
    match class {
        DOUBLE_BELOW | BELOW_LEFT | BELOW | BELOW_RIGHT | ATTACHED_BELOW_LEFT | ATTACHED_BELOW => {
            if !matches!(class, ATTACHED_BELOW_LEFT | ATTACHED_BELOW) {
                base.height = base.height.saturating_sub(y_gap);
            }
            pos.y_offset = clamp(
                i64::from(base.y_bearing) + i64::from(base.height) - i64::from(mark.y_bearing),
            );
            if (y_gap > 0) == (pos.y_offset > 0) {
                base.height = base.height.saturating_sub(pos.y_offset);
                pos.y_offset = 0;
            }
            base.height = base.height.saturating_add(mark.height);
        }
        DOUBLE_ABOVE | ABOVE_LEFT | ABOVE | ABOVE_RIGHT | ATTACHED_ABOVE | ATTACHED_ABOVE_RIGHT => {
            if !matches!(class, ATTACHED_ABOVE | ATTACHED_ABOVE_RIGHT) {
                base.y_bearing = base.y_bearing.saturating_add(y_gap);
                base.height = base.height.saturating_sub(y_gap);
            }
            pos.y_offset = clamp(
                i64::from(base.y_bearing) - i64::from(mark.y_bearing) - i64::from(mark.height),
            );
            if (y_gap > 0) != (pos.y_offset > 0) {
                let correction = -(pos.y_offset / 2);
                base.y_bearing = base.y_bearing.saturating_add(correction);
                base.height = base.height.saturating_sub(correction);
                pos.y_offset = pos.y_offset.saturating_add(correction);
            }
            base.y_bearing = base.y_bearing.saturating_sub(mark.height);
            base.height = base.height.saturating_add(mark.height);
        }
        _ => {}
    }
}
