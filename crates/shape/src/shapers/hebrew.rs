//! Hebrew: presentation-form composition for fonts without mark positioning, and
//! the order of a vowel before meteg.

use crate::buffer::Buffer;
use crate::plan::Plan;
use triergon_core::Result;

/// Letters with dagesh, U+05D0..=U+05EA; zero where Unicode has none.
const DAGESH_FORMS: [u32; 27] = [
    0xFB30, 0xFB31, 0xFB32, 0xFB33, 0xFB34, 0xFB35, 0xFB36, 0, 0xFB38, 0xFB39, 0xFB3A, 0xFB3B,
    0xFB3C, 0, 0xFB3E, 0, 0xFB40, 0xFB41, 0, 0xFB43, 0xFB44, 0, 0xFB46, 0xFB47, 0xFB48, 0xFB49,
    0xFB4A,
];

/// Unicode composition, then, when the font cannot position marks with GPOS, the
/// Alphabetic Presentation Forms block.
pub(super) fn compose(plan: &Plan, a: u32, b: u32) -> Option<u32> {
    if let Some(ab) = crate::unicode::code_point::compose(a, b) {
        return Some(ab);
    }
    if plan.has_gpos_mark {
        return None;
    }
    let form = match (b, a) {
        (0x05B4, 0x05D9) => 0xFB1D,
        (0x05B7, 0x05F2) => 0xFB1F,
        (0x05B7, 0x05D0) => 0xFB2E,
        (0x05B8, 0x05D0) => 0xFB2F,
        (0x05B9, 0x05D5) => 0xFB4B,
        (0x05BC, 0x05D0..=0x05EA) => DAGESH_FORMS[(a - 0x05D0) as usize],
        (0x05BC, 0xFB2A) => 0xFB2C,
        (0x05BC, 0xFB2B) => 0xFB2D,
        (0x05BF, 0x05D1) => 0xFB4C,
        (0x05BF, 0x05DB) => 0xFB4D,
        (0x05BF, 0x05E4) => 0xFB4E,
        (0x05C1, 0x05E9) => 0xFB2A,
        (0x05C1, 0xFB49) => 0xFB2C,
        (0x05C2, 0x05E9) => 0xFB2B,
        (0x05C2, 0xFB49) => 0xFB2D,
        _ => 0,
    };
    (form != 0).then_some(form)
}

/// Puts meteg (or a below mark) before a sheva or hiriq that follows patah or
/// qamats, once per mark sequence.
pub(super) fn reorder_marks(buffer: &mut Buffer, start: usize, end: usize) -> Result<()> {
    for i in start + 2..end {
        let infos = buffer.infos();
        let classes = [i - 2, i - 1, i].map(|j| infos[j].combining_class());
        if matches!(classes[0], 20 | 21)
            && matches!(classes[1], 22 | 23)
            && matches!(classes[2], 25 | 220)
        {
            buffer.merge_clusters(i - 1, i + 1)?;
            buffer.infos_mut().swap(i - 1, i);
            break;
        }
    }
    Ok(())
}
