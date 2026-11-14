//! The H.264 decoded picture buffer sizes of the levels (A.3.1, Table A-1).

/// MaxDpbMbs of Table A-1 for `level_idc`, level 1b being `level_idc` 11 with
/// `constraint_set3_flag` in the Baseline, Main and Extended profiles (66,
/// 77, 88) or `level_idc` 9 in the others. `None` for a level the table does
/// not list.
#[must_use]
pub fn max_dpb_mbs(profile_idc: u8, constraint_set3: bool, level_idc: u8) -> Option<u32> {
    let level_1b = level_idc == 9
        || (level_idc == 11 && constraint_set3 && matches!(profile_idc, 66 | 77 | 88));
    if level_1b {
        return Some(396);
    }
    Some(match level_idc {
        10 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        60..=62 => 696320,
        _ => return None,
    })
}

/// MaxDpbFrames (A.3.1 item h): MaxDpbMbs in frames of `frame_mbs`
/// macroblocks (PicWidthInMbs * FrameHeightInMbs), at most 16. `None` for a
/// level the table does not list.
#[must_use]
pub fn max_dpb_frames(
    profile_idc: u8,
    constraint_set3: bool,
    level_idc: u8,
    frame_mbs: u32,
) -> Option<u32> {
    let mbs = max_dpb_mbs(profile_idc, constraint_set3, level_idc)?;
    Some((mbs / frame_mbs.max(1)).min(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_1b_is_named_two_ways() {
        assert_eq!(max_dpb_mbs(66, true, 11), Some(396));
        assert_eq!(max_dpb_mbs(66, false, 11), Some(900));
        assert_eq!(max_dpb_mbs(100, true, 11), Some(900));
        assert_eq!(max_dpb_mbs(100, false, 9), Some(396));
        assert_eq!(max_dpb_mbs(100, false, 7), None);
    }

    #[test]
    fn frames_are_at_most_16() {
        // 352x288 (396 macroblocks) at level 1.0: one frame; 64x48 (12): 16.
        assert_eq!(max_dpb_frames(100, false, 10, 396), Some(1));
        assert_eq!(max_dpb_frames(100, false, 10, 12), Some(16));
        assert_eq!(max_dpb_frames(100, false, 30, 1620), Some(5));
    }
}
