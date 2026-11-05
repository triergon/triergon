//! Picture order count (8.2.1), picture numbers (8.2.4.1), reference list initialisation
//! (8.2.4.2) and modification (8.2.4.3), and decoded reference picture marking (8.2.5).

use crate::DecodeError;
use crate::picture::*;
use crate::ps::Sps;
use crate::slice::{SliceHeader, SliceType};

/// POC decoding state carried between pictures.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PocState {
    pub prev_poc_msb: i32,
    pub prev_poc_lsb: i32,
    pub prev_frame_num_offset: i32,
    pub prev_frame_num: u32,
    pub prev_ref_frame_num: u32,
    /// The previous picture in decoding order had mmco5.
    pub prev_mmco5: bool,
}

/// Compute TopFieldOrderCnt / BottomFieldOrderCnt for the current picture (8.2.1).
/// Returns ([top, bottom], frame_num_offset).
pub(crate) fn compute_poc(sps: &Sps, sh: &SliceHeader, st: &PocState) -> ([i32; 2], i32) {
    let max_frame_num = sps.max_frame_num() as i32;
    let fnum = sh.frame_num as i32;
    match sps.poc_type {
        0 => {
            let max_lsb = 1i32 << sps.log2_max_poc_lsb;
            let (prev_msb, prev_lsb) = if sh.idr {
                (0, 0)
            } else {
                (st.prev_poc_msb, st.prev_poc_lsb)
            };
            let lsb = sh.poc_lsb as i32;
            let msb = if lsb < prev_lsb && (prev_lsb - lsb) >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && (lsb - prev_lsb) > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            };
            let mut poc = [0; 2];
            if !sh.field_pic {
                poc[0] = msb + lsb;
                poc[1] = poc[0] + sh.delta_poc_bottom;
            } else if !sh.bottom_field {
                poc[0] = msb + lsb;
                poc[1] = poc[0];
            } else {
                poc[1] = msb + lsb;
                poc[0] = poc[1];
            }
            (poc, msb)
        }
        _ => {
            let offset = if sh.idr {
                0
            } else if st.prev_frame_num as i32 > fnum {
                st.prev_frame_num_offset + max_frame_num
            } else {
                st.prev_frame_num_offset
            };
            let nonref = sh.nal.nal_ref_idc == 0;
            if sps.poc_type == 1 {
                let n = sps.offset_for_ref_frame.len() as i32;
                let mut abs = if n != 0 { offset + fnum } else { 0 };
                if nonref && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i32;
                if abs > 0 {
                    let cycle = (abs - 1) / n;
                    let in_cycle = (abs - 1) % n;
                    let delta_cycle: i32 = sps.offset_for_ref_frame.iter().sum();
                    expected = cycle * delta_cycle;
                    expected += sps.offset_for_ref_frame[..=in_cycle as usize]
                        .iter()
                        .sum::<i32>();
                }
                if nonref {
                    expected += sps.offset_for_non_ref_pic;
                }
                let mut poc = [0; 2];
                if !sh.field_pic {
                    poc[0] = expected + sh.delta_poc[0];
                    poc[1] = poc[0] + sps.offset_for_top_to_bottom_field + sh.delta_poc[1];
                } else if !sh.bottom_field {
                    poc[0] = expected + sh.delta_poc[0];
                    poc[1] = poc[0];
                } else {
                    poc[1] = expected + sps.offset_for_top_to_bottom_field + sh.delta_poc[0];
                    poc[0] = poc[1];
                }
                (poc, offset)
            } else {
                let temp = if sh.idr {
                    0
                } else if nonref {
                    2 * (offset + fnum) - 1
                } else {
                    2 * (offset + fnum)
                };
                ([temp, temp], offset)
            }
        }
    }
}

/// Picture number helpers for the current picture.
pub(crate) struct PicNums {
    pub frame_num: u32,
    pub max_frame_num: u32,
    /// 0 frame, 1 top, 2 bottom.
    pub structure: u8,
}

impl PicNums {
    pub(crate) fn curr_pic_num(&self) -> i32 {
        if self.structure == 0 {
            self.frame_num as i32
        } else {
            2 * self.frame_num as i32 + 1
        }
    }
    pub(crate) fn max_pic_num(&self) -> i32 {
        if self.structure == 0 {
            self.max_frame_num as i32
        } else {
            2 * self.max_frame_num as i32
        }
    }
    pub(crate) fn frame_num_wrap(&self, fs: &FrameStore) -> i32 {
        if fs.frame_num > self.frame_num {
            fs.frame_num as i32 - self.max_frame_num as i32
        } else {
            fs.frame_num as i32
        }
    }
    /// PicNum of field `par` (1 top 2 bottom) or frame (0) of a store.
    pub(crate) fn pic_num(&self, fs: &FrameStore, par: u8) -> i32 {
        let w = self.frame_num_wrap(fs);
        if self.structure == 0 {
            w
        } else if par == self.structure {
            2 * w + 1
        } else {
            2 * w
        }
    }
    pub(crate) fn long_term_pic_num(&self, fs: &FrameStore, par: u8) -> i32 {
        let i = fs.long_term_frame_idx as i32;
        if self.structure == 0 {
            i
        } else if par == self.structure {
            2 * i + 1
        } else {
            2 * i
        }
    }
}

fn field_ref(dpb: &[FrameStore], idx: usize, par: u8) -> RefPic {
    let fs = &dpb[idx];
    let p = (par - 1) as usize;
    RefPic {
        idx,
        parity: par,
        poc: fs.poc[p],
        long: fs.mark[p] == RefMark::Long,
        id: ref_id(fs.uid, par),
    }
}

fn frame_ref(dpb: &[FrameStore], idx: usize) -> RefPic {
    let fs = &dpb[idx];
    RefPic {
        idx,
        parity: 0,
        poc: fs.frame_poc(),
        long: fs.is_long_frame(),
        id: ref_id(fs.uid, 0),
    }
}

/// 8.2.4.2.5 alternating-parity field list from ordered frame entries.
fn fields_from_frames(
    dpb: &[FrameStore],
    frames: &[usize],
    cur_par: u8,
    kind: RefMark,
    out: &mut Vec<RefPic>,
) {
    let has = |i: usize, par: u8| -> bool {
        let fs = &dpb[i];
        fs.fields & par != 0 && fs.mark[(par - 1) as usize] == kind
    };
    let mut same: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| has(i, cur_par))
        .collect();
    let mut opp: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| has(i, 3 - cur_par))
        .collect();
    same.reverse();
    opp.reverse();
    let mut take_same = true;
    loop {
        if take_same {
            if let Some(i) = same.pop() {
                out.push(field_ref(dpb, i, cur_par));
            } else if let Some(i) = opp.pop() {
                out.push(field_ref(dpb, i, 3 - cur_par));
                while let Some(i) = opp.pop() {
                    out.push(field_ref(dpb, i, 3 - cur_par));
                }
                break;
            } else {
                break;
            }
        } else if let Some(i) = opp.pop() {
            out.push(field_ref(dpb, i, 3 - cur_par));
        } else {
            while let Some(i) = same.pop() {
                out.push(field_ref(dpb, i, cur_par));
            }
            break;
        }
        take_same = !take_same;
    }
}

/// Initial reference picture lists (8.2.4.2). `cur` is the DPB index of the current picture
/// (its first field may be referenced when decoding the second field).
pub(crate) fn init_ref_lists(
    dpb: &[FrameStore],
    cur: usize,
    nums: &PicNums,
    st: SliceType,
    cur_poc: i32,
) -> [Vec<RefPic>; 2] {
    let mut l0 = Vec::new();
    let mut l1 = Vec::new();
    let is_b = st == SliceType::B;
    let field = nums.structure != 0;
    let usable = |i: usize| -> bool {
        let fs = &dpb[i];
        fs.in_use && (i != cur || field)
    };
    if !field {
        let short: Vec<usize> = (0..dpb.len())
            .filter(|&i| usable(i) && dpb[i].is_short_frame() && !dpb[i].non_existing_bframe(is_b))
            .collect();
        let mut long: Vec<usize> = (0..dpb.len())
            .filter(|&i| usable(i) && dpb[i].is_long_frame())
            .collect();
        long.sort_by_key(|&i| dpb[i].long_term_frame_idx);
        if !is_b {
            let mut s = short.clone();
            s.sort_by_key(|&i| -nums.pic_num(&dpb[i], 0));
            l0.extend(s.iter().map(|&i| frame_ref(dpb, i)));
            l0.extend(long.iter().map(|&i| frame_ref(dpb, i)));
        } else {
            let mut before: Vec<usize> = short
                .iter()
                .copied()
                .filter(|&i| dpb[i].frame_poc() < cur_poc)
                .collect();
            let mut after: Vec<usize> = short
                .iter()
                .copied()
                .filter(|&i| dpb[i].frame_poc() > cur_poc)
                .collect();
            before.sort_by_key(|&i| -dpb[i].frame_poc());
            after.sort_by_key(|&i| dpb[i].frame_poc());
            l0.extend(
                before
                    .iter()
                    .chain(after.iter())
                    .map(|&i| frame_ref(dpb, i)),
            );
            l0.extend(long.iter().map(|&i| frame_ref(dpb, i)));
            l1.extend(
                after
                    .iter()
                    .chain(before.iter())
                    .map(|&i| frame_ref(dpb, i)),
            );
            l1.extend(long.iter().map(|&i| frame_ref(dpb, i)));
        }
    } else {
        let cur_par = nums.structure;
        let short_any = |i: usize| -> bool {
            let fs = &dpb[i];
            (fs.mark[0] == RefMark::Short && fs.fields & 1 != 0)
                || (fs.mark[1] == RefMark::Short && fs.fields & 2 != 0)
        };
        let long_any = |i: usize| -> bool {
            let fs = &dpb[i];
            (fs.mark[0] == RefMark::Long && fs.fields & 1 != 0)
                || (fs.mark[1] == RefMark::Long && fs.fields & 2 != 0)
        };
        let short: Vec<usize> = (0..dpb.len())
            .filter(|&i| usable(i) && short_any(i) && !dpb[i].non_existing_bframe(is_b))
            .collect();
        let mut long: Vec<usize> = (0..dpb.len())
            .filter(|&i| usable(i) && long_any(i))
            .collect();
        long.sort_by_key(|&i| dpb[i].long_term_frame_idx);
        let entry_poc = |i: usize| -> i32 {
            let fs = &dpb[i];
            let mut v = i32::MAX;
            for p in 0..2 {
                if fs.fields & (1 << p) != 0 && fs.mark[p] == RefMark::Short {
                    v = v.min(fs.poc[p]);
                }
            }
            v
        };
        if !is_b {
            let mut s = short.clone();
            s.sort_by_key(|&i| -nums.frame_num_wrap(&dpb[i]));
            fields_from_frames(dpb, &s, cur_par, RefMark::Short, &mut l0);
            fields_from_frames(dpb, &long, cur_par, RefMark::Long, &mut l0);
        } else {
            let mut before: Vec<usize> = short
                .iter()
                .copied()
                .filter(|&i| entry_poc(i) <= cur_poc)
                .collect();
            let mut after: Vec<usize> = short
                .iter()
                .copied()
                .filter(|&i| entry_poc(i) > cur_poc)
                .collect();
            before.sort_by_key(|&i| -entry_poc(i));
            after.sort_by_key(|&i| entry_poc(i));
            let f0: Vec<usize> = before.iter().chain(after.iter()).copied().collect();
            let f1: Vec<usize> = after.iter().chain(before.iter()).copied().collect();
            fields_from_frames(dpb, &f0, cur_par, RefMark::Short, &mut l0);
            fields_from_frames(dpb, &long, cur_par, RefMark::Long, &mut l0);
            fields_from_frames(dpb, &f1, cur_par, RefMark::Short, &mut l1);
            fields_from_frames(dpb, &long, cur_par, RefMark::Long, &mut l1);
        }
    }
    if is_b && l1.len() > 1 && l0 == l1 {
        l1.swap(0, 1);
    }
    [l0, l1]
}

impl FrameStore {
    /// Non-existing frames (frame_num gaps) are excluded from B-slice lists when POC is unknown.
    fn non_existing_bframe(&self, is_b: bool) -> bool {
        is_b && self.non_existing
    }
}

/// Apply ref_pic_list_modification (8.2.4.3) and truncate to num_ref_idx_active.
pub(crate) fn modify_ref_list(
    dpb: &[FrameStore],
    cur: usize,
    nums: &PicNums,
    list: &mut Vec<RefPic>,
    mods: &[(u32, u32)],
    num_active: usize,
) -> Result<(), DecodeError> {
    let field = nums.structure != 0;
    // Find a picture by PicNum / LongTermPicNum among stored references.
    let find_short = |pn: i32| -> Option<RefPic> {
        for i in 0..dpb.len() {
            let fs = &dpb[i];
            if !fs.in_use || (i == cur && !field) {
                continue;
            }
            if !field {
                if fs.is_short_frame() && nums.pic_num(fs, 0) == pn {
                    return Some(frame_ref(dpb, i));
                }
            } else {
                for par in 1..=2u8 {
                    if fs.fields & par != 0
                        && fs.mark[(par - 1) as usize] == RefMark::Short
                        && nums.pic_num(fs, par) == pn
                    {
                        return Some(field_ref(dpb, i, par));
                    }
                }
            }
        }
        None
    };
    let find_long = |pn: i32| -> Option<RefPic> {
        for i in 0..dpb.len() {
            let fs = &dpb[i];
            if !fs.in_use || (i == cur && !field) {
                continue;
            }
            if !field {
                if fs.is_long_frame() && nums.long_term_pic_num(fs, 0) == pn {
                    return Some(frame_ref(dpb, i));
                }
            } else {
                for par in 1..=2u8 {
                    if fs.fields & par != 0
                        && fs.mark[(par - 1) as usize] == RefMark::Long
                        && nums.long_term_pic_num(fs, par) == pn
                    {
                        return Some(field_ref(dpb, i, par));
                    }
                }
            }
        }
        None
    };
    if !mods.is_empty() {
        let curr = nums.curr_pic_num();
        let maxp = nums.max_pic_num();
        let mut pred = curr;
        let mut ref_idx = 0usize;
        list.truncate(num_active);
        for &(idc, v) in mods {
            let pic = match idc {
                0 | 1 => {
                    let abs = v as i32 + 1;
                    let no_wrap = if idc == 0 {
                        if pred - abs < 0 {
                            pred - abs + maxp
                        } else {
                            pred - abs
                        }
                    } else if pred + abs >= maxp {
                        pred + abs - maxp
                    } else {
                        pred + abs
                    };
                    pred = no_wrap;
                    let pn = if no_wrap > curr {
                        no_wrap - maxp
                    } else {
                        no_wrap
                    };
                    find_short(pn).ok_or(DecodeError::InvalidData(
                        "ref_pic_list_modification: missing short-term picture",
                    ))?
                }
                _ => find_long(v as i32).ok_or(DecodeError::InvalidData(
                    "ref_pic_list_modification: missing long-term picture",
                ))?,
            };
            if ref_idx > list.len() {
                return Err(DecodeError::InvalidData("ref_pic_list_modification index"));
            }
            list.insert(ref_idx, pic);
            ref_idx += 1;
            // Remove the later duplicate (8-37 / 8-38).
            let mut n = ref_idx;
            let mut c = ref_idx;
            while c < list.len() {
                if list[c].id != pic.id {
                    list[n] = list[c];
                    n += 1;
                }
                c += 1;
            }
            list.truncate(n.max(ref_idx));
            list.truncate(num_active.max(ref_idx));
        }
    }
    list.truncate(num_active);
    Ok(())
}

/// Sliding window marking (8.2.5.3).
pub(crate) fn sliding_window(dpb: &mut [FrameStore], cur: usize, nums: &PicNums, max_refs: u32) {
    let max = max_refs.max(1) as usize;
    loop {
        let refs: Vec<usize> = (0..dpb.len())
            .filter(|&i| i != cur && dpb[i].in_use && dpb[i].is_ref())
            .collect();
        let n_short = refs
            .iter()
            .filter(|&&i| dpb[i].mark.contains(&RefMark::Short))
            .count();
        let n_long = refs
            .iter()
            .filter(|&&i| !dpb[i].mark.contains(&RefMark::Short))
            .count();
        if n_short + n_long < max || n_short == 0 {
            break;
        }
        // Smallest FrameNumWrap among short-term.
        let victim = refs
            .iter()
            .copied()
            .filter(|&i| dpb[i].mark.contains(&RefMark::Short))
            .min_by_key(|&i| nums.frame_num_wrap(&dpb[i]))
            .unwrap();
        for mk in dpb[victim]
            .mark
            .iter_mut()
            .filter(|m| **m == RefMark::Short)
        {
            *mk = RefMark::None;
        }
    }
}

/// Adaptive memory control (8.2.5.4). Returns true when mmco 5 was executed.
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
pub(crate) fn apply_mmcos(
    dpb: &mut [FrameStore],
    cur: usize,
    nums: &PicNums,
    sh: &SliceHeader,
    max_lt_idx: &mut Option<u32>,
) -> bool {
    let field = nums.structure != 0;
    let curr = nums.curr_pic_num();
    let mut mmco5 = false;
    for m in &sh.mmcos {
        match m.op {
            1 | 3 => {
                let pn = curr - (m.diff_pic_nums_minus1 as i32 + 1);
                // Locate the short-term picture/field with PicNum pn.
                let mut found = None;
                for i in 0..dpb.len() {
                    let fs = &dpb[i];
                    if !fs.in_use {
                        continue;
                    }
                    if !field {
                        if i != cur && fs.is_short_frame() && nums.pic_num(fs, 0) == pn {
                            found = Some((i, 0u8));
                        }
                    } else {
                        for par in 1..=2u8 {
                            if fs.fields & par != 0
                                && fs.mark[(par - 1) as usize] == RefMark::Short
                                && nums.pic_num(fs, par) == pn
                                && !(i == cur && par == nums.structure)
                            {
                                found = Some((i, par));
                            }
                        }
                    }
                }
                let Some((i, par)) = found else { continue };
                if m.op == 1 {
                    if par == 0 {
                        dpb[i].mark = [RefMark::None; 2];
                    } else {
                        dpb[i].mark[(par - 1) as usize] = RefMark::None;
                    }
                } else {
                    let idx = m.long_term_frame_idx;
                    // Free any other frame using this LongTermFrameIdx.
                    for j in 0..dpb.len() {
                        if j != i
                            && dpb[j].in_use
                            && dpb[j].mark.contains(&RefMark::Long)
                            && dpb[j].long_term_frame_idx == idx
                        {
                            for mk in dpb[j].mark.iter_mut().filter(|m| **m == RefMark::Long) {
                                *mk = RefMark::None;
                            }
                        }
                    }
                    // A frame whose other field is long-term with a different index loses it.
                    if par != 0 {
                        let other = (2 - par) as usize;
                        if dpb[i].mark[other] == RefMark::Long && dpb[i].long_term_frame_idx != idx
                        {
                            dpb[i].mark[other] = RefMark::None;
                        }
                        dpb[i].mark[(par - 1) as usize] = RefMark::Long;
                    } else {
                        dpb[i].mark = [RefMark::Long; 2];
                    }
                    dpb[i].long_term_frame_idx = idx;
                }
            }
            2 => {
                let lpn = m.long_term_pic_num as i32;
                for i in 0..dpb.len() {
                    let fs = &dpb[i];
                    if !fs.in_use {
                        continue;
                    }
                    if !field {
                        if i != cur && fs.is_long_frame() && nums.long_term_pic_num(fs, 0) == lpn {
                            dpb[i].mark = [RefMark::None; 2];
                        }
                    } else {
                        for par in 1..=2u8 {
                            let fs = &dpb[i];
                            if fs.fields & par != 0
                                && fs.mark[(par - 1) as usize] == RefMark::Long
                                && nums.long_term_pic_num(fs, par) == lpn
                                && !(i == cur && par == nums.structure)
                            {
                                dpb[i].mark[(par - 1) as usize] = RefMark::None;
                            }
                        }
                    }
                }
            }
            4 => {
                *max_lt_idx = if m.max_long_term_frame_idx_plus1 == 0 {
                    None
                } else {
                    Some(m.max_long_term_frame_idx_plus1 - 1)
                };
                for i in 0..dpb.len() {
                    if i == cur || !dpb[i].in_use {
                        continue;
                    }
                    let over = match *max_lt_idx {
                        None => true,
                        Some(mx) => dpb[i].long_term_frame_idx > mx,
                    };
                    if over {
                        for mk in dpb[i].mark.iter_mut().filter(|m| **m == RefMark::Long) {
                            *mk = RefMark::None;
                        }
                    }
                }
            }
            5 => {
                for i in 0..dpb.len() {
                    if i != cur {
                        dpb[i].mark = [RefMark::None; 2];
                    }
                }
                *max_lt_idx = None;
                mmco5 = true;
            }
            6 => {
                let idx = m.long_term_frame_idx;
                for j in 0..dpb.len() {
                    if j != cur
                        && dpb[j].in_use
                        && dpb[j].mark.contains(&RefMark::Long)
                        && dpb[j].long_term_frame_idx == idx
                    {
                        for mk in dpb[j].mark.iter_mut().filter(|m| **m == RefMark::Long) {
                            *mk = RefMark::None;
                        }
                    }
                }
                let fs = &mut dpb[cur];
                if field {
                    let other = (2 - nums.structure) as usize;
                    if fs.mark[other] == RefMark::Long && fs.long_term_frame_idx != idx {
                        fs.mark[other] = RefMark::None;
                    }
                    fs.mark[(nums.structure - 1) as usize] = RefMark::Long;
                } else {
                    fs.mark = [RefMark::Long; 2];
                }
                fs.long_term_frame_idx = idx;
            }
            _ => {}
        }
    }
    mmco5
}
