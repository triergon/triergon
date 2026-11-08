//! Glyph positioning subtables, and the passes before and after them that clear
//! and resolve attachments.

use super::apply::{Apply, Values};
use super::context;
use super::data::{Data, bsearch};
use super::layout::{LookupFlags, LookupType, class, coverage};
use crate::buffer::{Attachment, Buffer, Info, Pos, Position};
use crate::metrics::Scaled;
use crate::tag::Direction;
use triergon_core::Result;

/// Applies one subtable of type `kind` at the current glyph.
pub(crate) fn apply<'a>(ctx: &mut Apply<'a, '_>, kind: LookupType, data: Data<'a>) -> Result<bool> {
    match kind {
        LookupType::Single => Ok(single(ctx, data)),
        LookupType::Pair => pair(ctx, data),
        LookupType::Cursive => cursive(ctx, data),
        LookupType::MarkToBase => mark_to_base(ctx, data, false),
        LookupType::MarkToLigature => mark_to_base(ctx, data, true),
        LookupType::MarkToMark => mark_to_mark(ctx, data),
        LookupType::Context => context::apply(ctx, data, false),
        LookupType::ChainContext => context::apply(ctx, data, true),
        _ => Ok(false),
    }
}

/// A value record format: which adjustments a record holds.
#[derive(Clone, Copy)]
struct ValueFormat(u16);

impl ValueFormat {
    fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Adds the record at `record` (offsets of device tables from `base`) to `pos`.
    fn apply(self, font: &Scaled<'_>, base: Data<'_>, record: Data<'_>, pos: &mut Position) {
        let mut at = 0;
        let mut next = |present: bool| {
            let value = present.then(|| record.i16(at));
            if present {
                at += 2;
            }
            value
        };
        let format = self.0;
        if let Some(v) = next(format & 0x01 != 0) {
            pos.x_offset = pos.x_offset.saturating_add(font.em_scale_x(i32::from(v)));
        }
        if let Some(v) = next(format & 0x02 != 0) {
            pos.y_offset = pos.y_offset.saturating_add(font.em_scale_y(i32::from(v)));
        }
        if let Some(v) = next(format & 0x04 != 0) {
            pos.x_advance = pos.x_advance.saturating_add(font.em_scale_x(i32::from(v)));
        }
        let _y_advance = next(format & 0x08 != 0);
        if format & 0xf0 == 0 {
            return;
        }
        let (use_x, use_y) = (font.scale.x_ppem != 0, font.scale.y_ppem != 0);
        let mut device = |present: bool| {
            let device = present.then(|| base.at(record.u16(at) as usize));
            if present {
                at += 2;
            }
            device
        };
        if let Some(d) = device(format & 0x10 != 0).filter(|_| use_x) {
            pos.x_offset =
                pos.x_offset
                    .saturating_add(device_delta(d, font.scale.x_ppem, font.scale.x));
        }
        if let Some(d) = device(format & 0x20 != 0).filter(|_| use_y) {
            pos.y_offset =
                pos.y_offset
                    .saturating_add(device_delta(d, font.scale.y_ppem, font.scale.y));
        }
        if let Some(d) = device(format & 0x40 != 0).filter(|_| use_x) {
            pos.x_advance =
                pos.x_advance
                    .saturating_add(device_delta(d, font.scale.x_ppem, font.scale.x));
        }
    }
}

/// A hinting device table's correction at `ppem`, in scaled units. Variation
/// device tables are zero at the default instance.
fn device_delta(device: Data<'_>, ppem: u32, scale: i32) -> i32 {
    let (start, end, format) = (
        u32::from(device.u16(0)),
        u32::from(device.u16(2)),
        u32::from(device.u16(4)),
    );
    if ppem == 0 || !(1..=3).contains(&format) || ppem < start || ppem > end {
        return 0;
    }
    let s = ppem - start;
    let word = u32::from(device.u16(6 + 2 * (s >> (4 - format)) as usize));
    let bits = word >> (16 - (((s & ((1 << (4 - format)) - 1)) + 1) << format));
    let mask = 0xffff_u32 >> (16 - (1 << format));
    let mut delta = (bits & mask) as i32;
    if delta as u32 >= (mask + 1) >> 1 {
        delta -= (mask + 1) as i32;
    }
    (i64::from(delta) * i64::from(scale) / i64::from(ppem)) as i32
}

/// An anchor point in scaled units, unrounded; (0, 0) for an anchor of an
/// unknown format.
fn anchor(ctx: &Apply<'_, '_>, anchor: Data<'_>, glyph: u32) -> (f32, f32) {
    let font = ctx.font;
    let scale = font.scale;
    let format = anchor.u16(0);
    if !(1..=3).contains(&format) {
        return (0.0, 0.0);
    }
    let mut x = font.em_fscale_x(i32::from(anchor.i16(2)));
    let mut y = font.em_fscale_y(i32::from(anchor.i16(4)));
    match format {
        2 if scale.x_ppem != 0 || scale.y_ppem != 0 => {
            if let Some((cx, cy)) = font.contour_point(glyph, anchor.u16(6)) {
                if scale.x_ppem != 0 {
                    x = cx as f32;
                }
                if scale.y_ppem != 0 {
                    y = cy as f32;
                }
            }
        }
        3 => {
            if scale.x_ppem != 0 {
                x += device_delta(anchor.offset16(6), scale.x_ppem, scale.x) as f32;
            }
            if scale.y_ppem != 0 {
                y += device_delta(anchor.offset16(8), scale.y_ppem, scale.y) as f32;
            }
        }
        _ => {}
    }
    (x, y)
}

fn round(v: f32) -> i32 {
    v.round() as i32
}

fn single(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> bool {
    let Some(index) = coverage(data.offset16(2), ctx.buffer.cur().glyph) else {
        return false;
    };
    let format = ValueFormat(data.u16(4));
    let record = match data.u16(0) {
        1 => data.at(6),
        2 if (index as usize) < data.count(6, 8, 2 * format.len()) => {
            data.at(8 + 2 * format.len() * index as usize)
        }
        _ => return false,
    };
    let idx = ctx.buffer.idx;
    format.apply(
        ctx.font,
        data,
        record,
        &mut ctx.buffer.pos.as_mut_slice()[idx].position,
    );
    ctx.buffer.idx += 1;
    true
}

fn pair(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> Result<bool> {
    let idx = ctx.buffer.idx;
    let format = data.u16(0);
    if !matches!(format, 1 | 2) {
        return Ok(false);
    }
    let Some(index) = coverage(data.offset16(2), ctx.buffer.cur().glyph) else {
        return Ok(false);
    };
    let m = ctx.matcher(false, Values::Any);
    let Some(second) = ctx.next(&m, idx, 0)? else {
        return Ok(false);
    };
    let (format1, format2) = (ValueFormat(data.u16(4)), ValueFormat(data.u16(6)));
    let (len1, len2) = (format1.len(), format2.len());
    let second_glyph = ctx.buffer.infos()[second].glyph;
    let record = if format == 1 {
        let sets = data.count(8, 10, 2);
        if index as usize >= sets {
            return Ok(false);
        }
        let set = data.offset16(10 + 2 * index as usize);
        let size = 2 + 2 * (len1 + len2);
        let count = set.count(0, 2, size);
        let Some(i) = bsearch(count, |i| {
            u32::from(set.u16(2 + size * i)).cmp(&second_glyph)
        }) else {
            return Ok(false);
        };
        (set, set.at(2 + size * i + 2))
    } else {
        let class1 = u32::from(class(data.offset16(8), ctx.buffer.cur().glyph));
        let class2 = u32::from(class(data.offset16(10), second_glyph));
        let (count1, count2) = (u32::from(data.u16(12)), u32::from(data.u16(14)));
        if class1 >= count1 || class2 >= count2 {
            return Ok(false);
        }
        let index = (class1 * count2 + class2) as usize;
        (data, data.at(16 + 2 * (len1 + len2) * index))
    };
    let (base, values) = record;
    let pos = ctx.buffer.pos.as_mut_slice();
    if len1 != 0 {
        format1.apply(ctx.font, base, values, &mut pos[idx].position);
    }
    if len2 != 0 {
        format2.apply(
            ctx.font,
            base,
            values.at(2 * len1),
            &mut pos[second].position,
        );
    }
    ctx.buffer.idx = if len2 != 0 { second + 1 } else { second };
    Ok(true)
}

/// Shifts the cross-stream offset of a cursive chain that `i` starts, reversing the
/// chain so that `new_parent` becomes its root.
fn reverse_cursive_minor_offset(buffer: &mut Buffer, i: usize, new_parent: usize, depth: u8) {
    if depth > 64 {
        return;
    }
    let pos = buffer.pos.as_mut_slice();
    let Attachment::Cursive(chain) = pos[i].attachment else {
        return;
    };
    pos[i].attachment = Attachment::None;
    let Some(j) = i
        .checked_add_signed(isize::from(chain))
        .filter(|&j| j < pos.len())
    else {
        return;
    };
    if j == new_parent || chain == i16::MIN {
        return;
    }
    reverse_cursive_minor_offset(buffer, j, new_parent, depth + 1);
    let pos = buffer.pos.as_mut_slice();
    pos[j].position.y_offset = pos[i].position.y_offset.saturating_neg();
    pos[j].attachment = Attachment::Cursive(-chain);
}

fn cursive(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> Result<bool> {
    if data.u16(0) != 1 {
        return Ok(false);
    }
    let records = data.count(4, 6, 4);
    let record = |glyph: u32| coverage(data.offset16(2), glyph).filter(|&i| (i as usize) < records);
    let anchor_of = |index: u32, which: usize| data.offset16(6 + 4 * index as usize + which);
    let j = ctx.buffer.idx;
    let Some(this) = record(ctx.buffer.cur().glyph) else {
        return Ok(false);
    };
    let entry = anchor_of(this, 0);
    if entry.is_empty() {
        return Ok(false);
    }
    let m = ctx.matcher(false, Values::Any);
    let Some(i) = ctx.prev(&m, j, 0)? else {
        return Ok(false);
    };
    let exit = record(ctx.buffer.infos()[i].glyph).map(|r| anchor_of(r, 2));
    let Some(exit) = exit.filter(|e| !e.is_empty()) else {
        return Ok(false);
    };
    let (exit_x, exit_y) = anchor(ctx, exit, ctx.buffer.infos()[i].glyph);
    let (entry_x, entry_y) = anchor(ctx, entry, ctx.buffer.infos()[j].glyph);
    {
        let pos = ctx.buffer.pos.as_mut_slice();
        match ctx.direction {
            Direction::LeftToRight => {
                pos[i].position.x_advance = round(exit_x).saturating_add(pos[i].position.x_offset);
                let d = round(entry_x).saturating_add(pos[j].position.x_offset);
                pos[j].position.x_advance = pos[j].position.x_advance.saturating_sub(d);
                pos[j].position.x_offset = pos[j].position.x_offset.saturating_sub(d);
            }
            Direction::RightToLeft => {
                let d = round(exit_x).saturating_add(pos[i].position.x_offset);
                pos[i].position.x_advance = pos[i].position.x_advance.saturating_sub(d);
                pos[i].position.x_offset = pos[i].position.x_offset.saturating_sub(d);
                pos[j].position.x_advance = round(entry_x).saturating_add(pos[j].position.x_offset);
            }
        }
    }
    // The glyph that does not name the lookup's direction hangs off the other.
    let (child, parent, y_offset) = if ctx.lookup_flags.right_to_left() {
        (i, j, round(entry_y - exit_y))
    } else {
        (j, i, -round(entry_y - exit_y))
    };
    reverse_cursive_minor_offset(ctx.buffer, child, parent, 0);
    let pos = ctx.buffer.pos.as_mut_slice();
    if let Ok(chain) = i16::try_from(parent as isize - child as isize) {
        pos[child].attachment = Attachment::Cursive(chain);
        pos[child].position.y_offset = y_offset;
        // A chain that pointed back would loop: break it.
        if pos[parent].attachment.chain() == chain.wrapping_neg() {
            pos[parent].attachment = Attachment::None;
            pos[parent].position.y_offset = 0;
        }
    } else {
        pos[child].attachment = Attachment::None;
    }
    ctx.buffer.idx += 1;
    Ok(true)
}

/// Whether a glyph may take marks: not a later component of a multiple
/// substitution (those leave marks to the first component), unless it is a mark
/// base the lookup lists explicitly.
fn accepts_marks(infos: &[Info], idx: usize, ligature: bool) -> bool {
    let info = &infos[idx];
    if !info.multiplied() || info.lig.component() == 0 {
        return true;
    }
    if ligature {
        return false;
    }
    idx == 0 || {
        let prev = &infos[idx - 1];
        prev.is_mark()
            || !prev.multiplied()
            || info.lig.id() != prev.lig.id()
            || info.lig.component() != prev.lig.component().wrapping_add(1)
    }
}

/// The marks of a mark attachment subtable: each one's class and anchor.
#[derive(Clone, Copy)]
struct MarkArray<'a>(Data<'a>);

impl<'a> MarkArray<'a> {
    /// The class and anchor of mark `index`; class zero and no anchor past the
    /// marks the array holds, as HarfBuzz reads its null record.
    fn record(self, index: u32) -> (u32, Data<'a>) {
        let index = index as usize;
        if index >= self.0.count(0, 2, 4) {
            return (0, Data::EMPTY);
        }
        let at = 2 + 4 * index;
        (u32::from(self.0.u16(at)), self.0.offset16(at + 2))
    }
}

/// The anchors marks attach to on base glyphs, ligature components or marks:
/// a row per glyph or component, a column per mark class.
#[derive(Clone, Copy)]
struct AnchorMatrix<'a> {
    data: Data<'a>,
    rows: u32,
    columns: u32,
}

impl<'a> AnchorMatrix<'a> {
    /// A matrix at `data` whose row count comes first.
    fn counted(data: Data<'a>, columns: u32) -> Self {
        Self {
            data,
            rows: u32::from(data.u16(0)),
            columns,
        }
    }

    /// The anchor at `row` and `column`, if the matrix has one there.
    fn anchor(self, row: u32, column: u32) -> Option<Data<'a>> {
        if row >= self.rows || column >= self.columns {
            return None;
        }
        let anchor = self
            .data
            .offset16(2 + 2 * (row * self.columns + column) as usize);
        (!anchor.is_empty()).then_some(anchor)
    }
}

/// Mark-to-base (`ligature` false) and mark-to-ligature attachment.
fn mark_to_base(ctx: &mut Apply<'_, '_>, data: Data<'_>, ligature: bool) -> Result<bool> {
    if data.u16(0) != 1 {
        return Ok(false);
    }
    let idx = ctx.buffer.idx;
    let Some(mark_index) = coverage(data.offset16(2), ctx.buffer.cur().glyph) else {
        return Ok(false);
    };
    let bases = data.offset16(4);
    let m = ctx.matcher_with(LookupFlags::IGNORE_MARKS, false, Values::Any);
    if ctx.last_base_until > idx {
        ctx.last_base_until = 0;
        ctx.last_base = None;
    }
    let mut j = idx;
    while j > ctx.last_base_until {
        ctx.buffer.charge(1)?;
        let info = ctx.buffer.infos()[j - 1];
        if ctx.outcome_is_match(&m, &info)
            && (accepts_marks(ctx.buffer.infos(), j - 1, ligature)
                || coverage(bases, info.glyph).is_some())
        {
            ctx.last_base = Some(j - 1);
            break;
        }
        j -= 1;
    }
    ctx.last_base_until = idx;
    let Some(base) = ctx.last_base else {
        return Ok(false);
    };
    let Some(base_index) = coverage(bases, ctx.buffer.infos()[base].glyph) else {
        return Ok(false);
    };
    let classes = u32::from(data.u16(6));
    let marks = MarkArray(data.offset16(8));
    let attach = data.offset16(10);
    if !ligature {
        let anchors = AnchorMatrix::counted(attach, classes);
        return attach_mark(ctx, marks, mark_index, anchors, base_index, base);
    }
    if base_index as usize >= attach.count(0, 2, 2) {
        return Ok(false);
    }
    let components = AnchorMatrix::counted(attach.offset16(2 + 2 * base_index as usize), classes);
    if components.rows == 0 {
        return Ok(false);
    }
    // The component the mark follows, or the last.
    let lig_id = ctx.buffer.infos()[base].lig.id();
    let cur = ctx.buffer.cur().lig;
    let component = if lig_id != 0 && lig_id == cur.id() && cur.component() > 0 {
        components.rows.min(u32::from(cur.component())) - 1
    } else {
        components.rows - 1
    };
    attach_mark(ctx, marks, mark_index, components, component, base)
}

fn mark_to_mark(ctx: &mut Apply<'_, '_>, data: Data<'_>) -> Result<bool> {
    if data.u16(0) != 1 {
        return Ok(false);
    }
    let idx = ctx.buffer.idx;
    let Some(mark1) = coverage(data.offset16(2), ctx.buffer.cur().glyph) else {
        return Ok(false);
    };
    let m = ctx.matcher_with(ctx.lookup_flags.skipping_no_class(), false, Values::Any);
    let Some(j) = ctx.prev(&m, idx, 0)? else {
        return Ok(false);
    };
    let infos = ctx.buffer.infos();
    if !infos[j].is_mark() {
        return Ok(false);
    }
    let (a, b) = (infos[idx].lig, infos[j].lig);
    let same_base = if a.id() == b.id() {
        a.id() == 0 || a.component() == b.component()
    } else {
        // A mark of a ligature against a mark of its component: allow if either
        // belongs to the ligature as a whole.
        (a.id() > 0 && a.component() == 0) || (b.id() > 0 && b.component() == 0)
    };
    if !same_base {
        return Ok(false);
    }
    let Some(mark2) = coverage(data.offset16(4), infos[j].glyph) else {
        return Ok(false);
    };
    let anchors = AnchorMatrix::counted(data.offset16(10), u32::from(data.u16(6)));
    attach_mark(ctx, MarkArray(data.offset16(8)), mark1, anchors, mark2, j)
}

/// Attaches the current mark, `mark` of `marks`, to the glyph at `target`
/// through the anchor of its class in `row` of `anchors`.
fn attach_mark(
    ctx: &mut Apply<'_, '_>,
    marks: MarkArray<'_>,
    mark: u32,
    anchors: AnchorMatrix<'_>,
    row: u32,
    target: usize,
) -> Result<bool> {
    let (mark_class, mark_anchor) = marks.record(mark);
    let Some(target_anchor) = anchors.anchor(row, mark_class) else {
        return Ok(false);
    };
    let idx = ctx.buffer.idx;
    let (mark_x, mark_y) = anchor(ctx, mark_anchor, ctx.buffer.infos()[idx].glyph);
    let (base_x, base_y) = anchor(ctx, target_anchor, ctx.buffer.infos()[target].glyph);
    let base_offset = cross_offset(ctx.buffer, target)?;
    let pos = ctx.buffer.pos.as_mut_slice();
    if let Ok(chain) = i16::try_from(target as isize - idx as isize) {
        let mark = &mut pos[idx];
        mark.attachment = Attachment::Mark(chain);
        mark.position.x_offset = round(base_x - mark_x);
        mark.position.y_offset = round(base_y - mark_y).saturating_add(base_offset);
    } else {
        pos[idx].attachment = Attachment::None;
    }
    ctx.buffer.idx += 1;
    Ok(true)
}

/// The cross-stream offset a glyph inherits from the cursive chain it hangs on.
/// Each link followed is charged; attachments never form a cycle, but a chain
/// longer than the buffer would be one, and stops there.
fn cross_offset(buffer: &mut Buffer, mut i: usize) -> Result<i32> {
    let mut offset = buffer.pos.as_slice()[i].position.y_offset;
    for _ in 0..buffer.len() {
        buffer.charge(1)?;
        let pos = buffer.pos.as_slice();
        let Attachment::Cursive(chain) = pos[i].attachment else {
            break;
        };
        let Some(parent) = i
            .checked_add_signed(isize::from(chain))
            .filter(|&p| p < pos.len())
        else {
            break;
        };
        if chain == 0 {
            break;
        }
        i = parent;
        offset = offset.saturating_add(pos[i].position.y_offset);
    }
    Ok(offset)
}

/// Clears attachments before positioning.
pub(crate) fn start(buffer: &mut Buffer) {
    for pos in buffer.pos.as_mut_slice() {
        pos.attachment = Attachment::None;
    }
}

/// Resolves attachments into offsets: a mark moves with what it is attached to,
/// less the advances between them. Each glyph is resolved once and adds up the
/// advances its attachment spans, which are charged first.
pub(crate) fn finish_offsets(buffer: &mut Buffer, direction: Direction) -> Result<()> {
    let spans = buffer
        .pos
        .as_slice()
        .iter()
        .map(|p| usize::from(p.attachment.chain().unsigned_abs()))
        .fold(buffer.len(), usize::saturating_add);
    buffer.charge(spans)?;
    let pos = buffer.pos.as_mut_slice();
    let len = pos.len();
    let mut propagate_at = |i: usize| {
        if pos[i].attachment.chain() != 0 {
            propagate(pos, i, direction, 64);
        }
    };
    if direction.is_backward() {
        (0..len).rev().for_each(&mut propagate_at);
    } else {
        (0..len).for_each(&mut propagate_at);
    }
    Ok(())
}

fn propagate(pos: &mut [Pos], i: usize, direction: Direction, depth: u8) {
    let attachment = pos[i].attachment;
    pos[i].attachment = Attachment::None;
    let Some(j) = i
        .checked_add_signed(isize::from(attachment.chain()))
        .filter(|&j| j < pos.len())
    else {
        return;
    };
    if depth == 0 {
        return;
    }
    if pos[j].attachment.chain() != 0 {
        propagate(pos, j, direction, depth - 1);
    }
    let parent = pos[j].position;
    let p = &mut pos[i].position;
    if let Attachment::Cursive(_) = attachment {
        p.y_offset = p.y_offset.saturating_add(parent.y_offset);
        return;
    }
    p.x_offset = p.x_offset.saturating_add(parent.x_offset);
    let (from, to, add) = match (j < i, direction.is_backward()) {
        (true, false) => (j, i, false),
        (true, true) => (j + 1, i + 1, true),
        (false, false) => (i, j, true),
        (false, true) => (i + 1, j + 1, false),
    };
    for k in from..to {
        let advance = pos[k].position;
        let p = &mut pos[i].position;
        if add {
            p.x_offset = p.x_offset.saturating_add(advance.x_advance);
            p.y_offset = p.y_offset.saturating_add(advance.y_advance);
        } else {
            p.x_offset = p.x_offset.saturating_sub(advance.x_advance);
            p.y_offset = p.y_offset.saturating_sub(advance.y_advance);
        }
    }
}
