//! Picture order count (8.2.1), picture numbers (8.2.4.1), reference list initialisation
//! (8.2.4.2) and modification (8.2.4.3), and decoded reference picture marking (8.2.5) of
//! the frame stores in the decoded picture buffer, which keeps their marking.

use crate::DecodeError;
use crate::picture::*;
use crate::ps::Sps;
use crate::slice::{SliceHeader, SliceType};
use triergon_framing::picture_output::{Entry, Marking, View};

/// A stored frame store and its marking, read for reference lists and marking.
pub(crate) trait Stored {
    fn store(&self) -> &FrameStore;
    /// The marking of field `par`.
    fn mark(&self, par: Structure) -> Marking;
    fn is_ref(&self) -> bool {
        self.mark(Structure::Top) != Marking::Unused
            || self.mark(Structure::Bottom) != Marking::Unused
    }
    /// Either field is marked `marking`.
    fn has(&self, marking: Marking) -> bool {
        self.mark(Structure::Top) == marking || self.mark(Structure::Bottom) == marking
    }
    fn is_short_frame(&self) -> bool {
        self.mark(Structure::Top) == Marking::ShortTerm
            && self.mark(Structure::Bottom) == Marking::ShortTerm
    }
    fn is_long_frame(&self) -> bool {
        self.mark(Structure::Top) == Marking::LongTerm
            && self.mark(Structure::Bottom) == Marking::LongTerm
    }
    /// Non-existing frames (frame_num gaps) are excluded from B-slice lists when POC is
    /// unknown.
    fn non_existing_bframe(&self, is_b: bool) -> bool {
        is_b && self.store().non_existing
    }
}

/// The stored frame stores `stored` in frame buffer number order: the order reference list
/// initialisation and marking visit them in, which settles ties between pictures with equal
/// picture numbers or order counts.
pub(crate) fn by_number<T: Stored>(mut stored: Vec<T>) -> Vec<T> {
    stored.sort_unstable_by_key(|s| s.store().number);
    stored
}

impl Stored for View<'_, FrameStore> {
    fn store(&self) -> &FrameStore {
        self.picture()
    }
    fn mark(&self, par: Structure) -> Marking {
        self.marking(par.parity())
    }
}

impl Stored for Entry<'_, FrameStore> {
    fn store(&self) -> &FrameStore {
        self.picture()
    }
    fn mark(&self, par: Structure) -> Marking {
        self.marking(par.parity())
    }
}

/// POC decoding state carried between pictures.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PocState {
    pub(crate) prev_poc_msb: i32,
    pub(crate) prev_poc_lsb: i32,
    pub(crate) prev_frame_num_offset: i32,
    pub(crate) prev_frame_num: u32,
    pub(crate) prev_ref_frame_num: u32,
    /// The previous picture in decoding order had mmco5.
    pub(crate) prev_mmco5: bool,
}

/// Compute TopFieldOrderCnt / BottomFieldOrderCnt for the current picture (8.2.1).
/// Returns ([top, bottom], frame_num_offset). The offsets and counts come from
/// unbounded Exp-Golomb values, so the sums wrap as two's complement.
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
                prev_msb.wrapping_add(max_lsb)
            } else if lsb > prev_lsb && (lsb - prev_lsb) > max_lsb / 2 {
                prev_msb.wrapping_sub(max_lsb)
            } else {
                prev_msb
            };
            let mut poc = [0; 2];
            if !sh.field_pic {
                poc[0] = msb.wrapping_add(lsb);
                poc[1] = poc[0].wrapping_add(sh.delta_poc_bottom);
            } else if !sh.bottom_field {
                poc[0] = msb.wrapping_add(lsb);
                poc[1] = poc[0];
            } else {
                poc[1] = msb.wrapping_add(lsb);
                poc[0] = poc[1];
            }
            (poc, msb)
        }
        _ => {
            let offset = if sh.idr {
                0
            } else if st.prev_frame_num as i32 > fnum {
                st.prev_frame_num_offset.wrapping_add(max_frame_num)
            } else {
                st.prev_frame_num_offset
            };
            let nonref = sh.nal.nal_ref_idc == 0;
            if sps.poc_type == 1 {
                let n = sps.offset_for_ref_frame.len() as i32;
                let mut abs = if n != 0 { offset.wrapping_add(fnum) } else { 0 };
                if nonref && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i32;
                if abs > 0 {
                    let cycle = (abs - 1) / n;
                    let in_cycle = (abs - 1) % n;
                    let sum = |o: &[i32]| o.iter().fold(0i32, |s, &v| s.wrapping_add(v));
                    let delta_cycle = sum(&sps.offset_for_ref_frame);
                    expected = cycle.wrapping_mul(delta_cycle);
                    expected =
                        expected.wrapping_add(sum(&sps.offset_for_ref_frame[..=in_cycle as usize]));
                }
                if nonref {
                    expected = expected.wrapping_add(sps.offset_for_non_ref_pic);
                }
                let to_bottom = sps.offset_for_top_to_bottom_field;
                let mut poc = [0; 2];
                if !sh.field_pic {
                    poc[0] = expected.wrapping_add(sh.delta_poc[0]);
                    poc[1] = poc[0].wrapping_add(to_bottom).wrapping_add(sh.delta_poc[1]);
                } else if !sh.bottom_field {
                    poc[0] = expected.wrapping_add(sh.delta_poc[0]);
                    poc[1] = poc[0];
                } else {
                    poc[1] = expected
                        .wrapping_add(to_bottom)
                        .wrapping_add(sh.delta_poc[0]);
                    poc[0] = poc[1];
                }
                (poc, offset)
            } else {
                let temp = if sh.idr {
                    0
                } else if nonref {
                    offset.wrapping_add(fnum).wrapping_mul(2).wrapping_sub(1)
                } else {
                    offset.wrapping_add(fnum).wrapping_mul(2)
                };
                ([temp, temp], offset)
            }
        }
    }
}

/// Picture number helpers for the current picture.
pub(crate) struct PicNums {
    pub(crate) frame_num: u32,
    pub(crate) max_frame_num: u32,
    pub(crate) structure: Structure,
}

impl PicNums {
    pub(crate) fn curr_pic_num(&self) -> i32 {
        if self.structure == Structure::Frame {
            self.frame_num as i32
        } else {
            2 * self.frame_num as i32 + 1
        }
    }
    pub(crate) fn max_pic_num(&self) -> i32 {
        if self.structure == Structure::Frame {
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
    /// PicNum of field or frame `par` of a store.
    pub(crate) fn pic_num(&self, fs: &FrameStore, par: Structure) -> i32 {
        let w = self.frame_num_wrap(fs);
        if self.structure == Structure::Frame {
            w
        } else if par == self.structure {
            2 * w + 1
        } else {
            2 * w
        }
    }
    pub(crate) fn long_term_pic_num(&self, fs: &FrameStore, par: Structure) -> i32 {
        // long_term_frame_idx is not range checked: the field numbers wrap.
        let i = fs.long_term_frame_idx as i32;
        if self.structure == Structure::Frame {
            i
        } else if par == self.structure {
            i.wrapping_mul(2).wrapping_add(1)
        } else {
            i.wrapping_mul(2)
        }
    }
}

fn field_ref(dpb: &[View<'_, FrameStore>], idx: usize, par: Structure) -> RefPic {
    let fs = dpb[idx].store();
    RefPic {
        idx,
        parity: par as u8,
        poc: fs.poc[par.index()],
        long: dpb[idx].mark(par) == Marking::LongTerm,
        id: ref_id(fs.uid, par as u8),
    }
}

fn frame_ref(dpb: &[View<'_, FrameStore>], idx: usize) -> RefPic {
    let fs = dpb[idx].store();
    RefPic {
        idx,
        parity: Structure::Frame as u8,
        poc: fs.frame_poc(),
        long: dpb[idx].is_long_frame(),
        id: ref_id(fs.uid, Structure::Frame as u8),
    }
}

/// 8.2.4.2.5 alternating-parity field list from ordered frame entries.
fn fields_from_frames(
    dpb: &[View<'_, FrameStore>],
    frames: &[usize],
    cur_par: Structure,
    kind: Marking,
    out: &mut Vec<RefPic>,
) {
    let has = |i: usize, par: Structure| -> bool {
        dpb[i].store().has_field(par) && dpb[i].mark(par) == kind
    };
    let mut same: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| has(i, cur_par))
        .collect();
    let mut opp: Vec<usize> = frames
        .iter()
        .copied()
        .filter(|&i| has(i, cur_par.opposite()))
        .collect();
    same.reverse();
    opp.reverse();
    let mut take_same = true;
    loop {
        if take_same {
            if let Some(i) = same.pop() {
                out.push(field_ref(dpb, i, cur_par));
            } else if let Some(i) = opp.pop() {
                out.push(field_ref(dpb, i, cur_par.opposite()));
                while let Some(i) = opp.pop() {
                    out.push(field_ref(dpb, i, cur_par.opposite()));
                }
                break;
            } else {
                break;
            }
        } else if let Some(i) = opp.pop() {
            out.push(field_ref(dpb, i, cur_par.opposite()));
        } else {
            while let Some(i) = same.pop() {
                out.push(field_ref(dpb, i, cur_par));
            }
            break;
        }
        take_same = !take_same;
    }
}

/// Initial reference picture lists (8.2.4.2) from the stored pictures `dpb`, in storage
/// order. The picture being decoded is not stored, but the first field of its frame is
/// when it is a second field, and is then available to it.
pub(crate) fn init_ref_lists(
    dpb: &[View<'_, FrameStore>],
    nums: &PicNums,
    st: SliceType,
    cur_poc: i32,
) -> [Vec<RefPic>; 2] {
    let mut l0 = Vec::new();
    let mut l1 = Vec::new();
    let is_b = st == SliceType::B;
    let field = nums.structure.is_field();
    if !field {
        let short: Vec<usize> = (0..dpb.len())
            .filter(|&i| dpb[i].is_short_frame() && !dpb[i].non_existing_bframe(is_b))
            .collect();
        let mut long: Vec<usize> = (0..dpb.len()).filter(|&i| dpb[i].is_long_frame()).collect();
        long.sort_by_key(|&i| dpb[i].store().long_term_frame_idx);
        if !is_b {
            let mut s = short.clone();
            s.sort_by_key(|&i| -nums.pic_num(dpb[i].store(), Structure::Frame));
            l0.extend(s.iter().map(|&i| frame_ref(dpb, i)));
            l0.extend(long.iter().map(|&i| frame_ref(dpb, i)));
        } else {
            let frame_poc = |i: usize| dpb[i].store().frame_poc();
            let mut before: Vec<usize> = short
                .iter()
                .copied()
                .filter(|&i| frame_poc(i) < cur_poc)
                .collect();
            let mut after: Vec<usize> = short
                .iter()
                .copied()
                .filter(|&i| frame_poc(i) > cur_poc)
                .collect();
            before.sort_by_key(|&i| -frame_poc(i));
            after.sort_by_key(|&i| frame_poc(i));
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
        // Either decoded field is marked `kind`.
        let any = |i: usize, kind: Marking| -> bool {
            let d = &dpb[i];
            Structure::FIELDS
                .into_iter()
                .any(|par| d.store().has_field(par) && d.mark(par) == kind)
        };
        let short: Vec<usize> = (0..dpb.len())
            .filter(|&i| any(i, Marking::ShortTerm) && !dpb[i].non_existing_bframe(is_b))
            .collect();
        let mut long: Vec<usize> = (0..dpb.len())
            .filter(|&i| any(i, Marking::LongTerm))
            .collect();
        long.sort_by_key(|&i| dpb[i].store().long_term_frame_idx);
        let entry_poc = |i: usize| -> i32 {
            let d = &dpb[i];
            let mut v = i32::MAX;
            for par in Structure::FIELDS {
                if d.store().has_field(par) && d.mark(par) == Marking::ShortTerm {
                    v = v.min(d.store().poc[par.index()]);
                }
            }
            v
        };
        if !is_b {
            let mut s = short.clone();
            s.sort_by_key(|&i| -nums.frame_num_wrap(dpb[i].store()));
            fields_from_frames(dpb, &s, cur_par, Marking::ShortTerm, &mut l0);
            fields_from_frames(dpb, &long, cur_par, Marking::LongTerm, &mut l0);
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
            fields_from_frames(dpb, &f0, cur_par, Marking::ShortTerm, &mut l0);
            fields_from_frames(dpb, &long, cur_par, Marking::LongTerm, &mut l0);
            fields_from_frames(dpb, &f1, cur_par, Marking::ShortTerm, &mut l1);
            fields_from_frames(dpb, &long, cur_par, Marking::LongTerm, &mut l1);
        }
    }
    if is_b && l1.len() > 1 && l0 == l1 {
        l1.swap(0, 1);
    }
    [l0, l1]
}

/// Apply ref_pic_list_modification (8.2.4.3) and truncate to num_ref_idx_active.
pub(crate) fn modify_ref_list(
    dpb: &[View<'_, FrameStore>],
    nums: &PicNums,
    list: &mut Vec<RefPic>,
    mods: &[(u32, u32)],
    num_active: usize,
) -> Result<(), DecodeError> {
    let field = nums.structure.is_field();
    // Find a picture by PicNum (`kind` short-term) or LongTermPicNum among stored references.
    let find = |pn: i32, kind: Marking| -> Option<RefPic> {
        let num = |fs: &FrameStore, par: Structure| {
            if kind == Marking::LongTerm {
                nums.long_term_pic_num(fs, par)
            } else {
                nums.pic_num(fs, par)
            }
        };
        for (i, d) in dpb.iter().enumerate() {
            let fs = d.store();
            if !field {
                if d.mark(Structure::Top) == kind
                    && d.mark(Structure::Bottom) == kind
                    && num(fs, Structure::Frame) == pn
                {
                    return Some(frame_ref(dpb, i));
                }
            } else {
                for par in Structure::FIELDS {
                    if fs.has_field(par) && d.mark(par) == kind && num(fs, par) == pn {
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
                    // abs_diff_pic_num_minus1 is not range checked: the sums wrap.
                    let abs = (v as i32).wrapping_add(1);
                    let no_wrap = if idc == 0 {
                        let d = pred.wrapping_sub(abs);
                        if d < 0 { d.wrapping_add(maxp) } else { d }
                    } else {
                        let s = pred.wrapping_add(abs);
                        if s >= maxp { s.wrapping_sub(maxp) } else { s }
                    };
                    pred = no_wrap;
                    let pn = if no_wrap > curr {
                        no_wrap - maxp
                    } else {
                        no_wrap
                    };
                    find(pn, Marking::ShortTerm).ok_or(DecodeError::Invalid(
                        "ref_pic_list_modification: missing short-term picture",
                    ))?
                }
                _ => find(v as i32, Marking::LongTerm).ok_or(DecodeError::Invalid(
                    "ref_pic_list_modification: missing long-term picture",
                ))?,
            };
            if ref_idx > list.len() {
                return Err(DecodeError::Invalid("ref_pic_list_modification index"));
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

/// Unmarks every field of `entry` marked `marking`.
fn unmark(entry: &mut Entry<'_, FrameStore>, marking: Marking) {
    for par in Structure::FIELDS {
        if entry.mark(par) == marking {
            entry.set_marking(Some(par.parity()), Marking::Unused);
        }
    }
}

/// Sliding window marking (8.2.5.3) of the stored pictures `dpb`. `first` is the position
/// of the current frame's first field when the current picture is its second field.
pub(crate) fn sliding_window(
    dpb: &mut [Entry<'_, FrameStore>],
    first: Option<usize>,
    nums: &PicNums,
    max_refs: u32,
) {
    let max = max_refs.max(1) as usize;
    loop {
        let refs: Vec<usize> = (0..dpb.len())
            .filter(|&i| Some(i) != first && dpb[i].is_ref())
            .collect();
        let n_short = refs
            .iter()
            .filter(|&&i| dpb[i].has(Marking::ShortTerm))
            .count();
        let n_long = refs.len() - n_short;
        if n_short + n_long < max || n_short == 0 {
            break;
        }
        // Smallest FrameNumWrap among short-term.
        let Some(victim) = refs
            .iter()
            .copied()
            .filter(|&i| dpb[i].has(Marking::ShortTerm))
            .min_by_key(|&i| nums.frame_num_wrap(dpb[i].store()))
        else {
            break;
        };
        unmark(&mut dpb[victim], Marking::ShortTerm);
    }
}

/// The marking of the current picture that adaptive marking (8.2.5.4) decides when the
/// picture is a frame or a first field, which is not stored yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NewMarking {
    /// The frame's, or the field's, marking.
    pub(crate) marking: Marking,
    /// LongTermFrameIdx, once one is assigned.
    pub(crate) long_term_frame_idx: Option<u32>,
}

/// The current picture's place in marking: not stored yet, or the second field of the
/// frame store at a position of the stored pictures, which holds its first field.
#[derive(Debug)]
pub(crate) enum Current<'a> {
    New(&'a mut NewMarking),
    Stored(usize),
}

impl Current<'_> {
    fn is(&self, i: usize) -> bool {
        matches!(*self, Current::Stored(at) if at == i)
    }

    /// Marks the current picture, of structure `structure`, and gives it
    /// `long_term_frame_idx` when that is some.
    pub(crate) fn mark(
        &mut self,
        dpb: &mut [Entry<'_, FrameStore>],
        structure: Structure,
        marking: Marking,
        long_term_frame_idx: Option<u32>,
    ) {
        match self {
            Current::New(new) => {
                new.marking = marking;
                if long_term_frame_idx.is_some() {
                    new.long_term_frame_idx = long_term_frame_idx;
                }
            }
            Current::Stored(at) => {
                let d = &mut dpb[*at];
                d.set_marking(Some(structure.parity()), marking);
                if let Some(idx) = long_term_frame_idx {
                    d.picture_mut().long_term_frame_idx = idx;
                }
            }
        }
    }
}

/// Adaptive memory control (8.2.5.4) of the stored pictures `dpb` and the current picture.
/// Returns true when mmco 5 was executed.
pub(crate) fn apply_mmcos(
    dpb: &mut [Entry<'_, FrameStore>],
    cur: &mut Current<'_>,
    nums: &PicNums,
    sh: &SliceHeader,
    max_lt_idx: &mut Option<u32>,
) -> bool {
    let field = nums.structure.is_field();
    let curr = nums.curr_pic_num();
    let mut mmco5 = false;
    for m in &sh.mmcos {
        match m.op {
            1 | 3 => {
                let pn = curr.wrapping_sub((m.diff_pic_nums_minus1 as i32).wrapping_add(1));
                // Locate the short-term picture/field with PicNum pn.
                let mut found = None;
                for (i, d) in dpb.iter().enumerate() {
                    let fs = d.store();
                    if !field {
                        if !cur.is(i)
                            && d.is_short_frame()
                            && nums.pic_num(fs, Structure::Frame) == pn
                        {
                            found = Some((i, Structure::Frame));
                        }
                    } else {
                        for par in Structure::FIELDS {
                            if fs.has_field(par)
                                && d.mark(par) == Marking::ShortTerm
                                && nums.pic_num(fs, par) == pn
                                && !(cur.is(i) && par == nums.structure)
                            {
                                found = Some((i, par));
                            }
                        }
                    }
                }
                let Some((i, par)) = found else { continue };
                if m.op == 1 {
                    if par == Structure::Frame {
                        dpb[i].set_marking(None, Marking::Unused);
                    } else {
                        dpb[i].set_marking(Some(par.parity()), Marking::Unused);
                    }
                } else {
                    let idx = m.long_term_frame_idx;
                    // Free any other frame using this LongTermFrameIdx.
                    for (j, d) in dpb.iter_mut().enumerate() {
                        if j != i
                            && d.has(Marking::LongTerm)
                            && d.store().long_term_frame_idx == idx
                        {
                            unmark(d, Marking::LongTerm);
                        }
                    }
                    if let Current::New(new) = cur
                        && new.marking == Marking::LongTerm
                        && new.long_term_frame_idx == Some(idx)
                    {
                        new.marking = Marking::Unused;
                    }
                    // A frame whose other field is long-term with a different index loses it.
                    let d = &mut dpb[i];
                    if par.is_field() {
                        let other = par.opposite();
                        if d.mark(other) == Marking::LongTerm
                            && d.store().long_term_frame_idx != idx
                        {
                            d.set_marking(Some(other.parity()), Marking::Unused);
                        }
                        d.set_marking(Some(par.parity()), Marking::LongTerm);
                    } else {
                        d.set_marking(None, Marking::LongTerm);
                    }
                    d.picture_mut().long_term_frame_idx = idx;
                }
            }
            2 => {
                let lpn = m.long_term_pic_num as i32;
                for (i, d) in dpb.iter_mut().enumerate() {
                    if !field {
                        if !cur.is(i)
                            && d.is_long_frame()
                            && nums.long_term_pic_num(d.store(), Structure::Frame) == lpn
                        {
                            d.set_marking(None, Marking::Unused);
                        }
                    } else {
                        for par in Structure::FIELDS {
                            if d.store().has_field(par)
                                && d.mark(par) == Marking::LongTerm
                                && nums.long_term_pic_num(d.store(), par) == lpn
                                && !(cur.is(i) && par == nums.structure)
                            {
                                d.set_marking(Some(par.parity()), Marking::Unused);
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
                for (i, d) in dpb.iter_mut().enumerate() {
                    if cur.is(i) {
                        continue;
                    }
                    let over = match *max_lt_idx {
                        None => true,
                        Some(mx) => d.store().long_term_frame_idx > mx,
                    };
                    if over {
                        unmark(d, Marking::LongTerm);
                    }
                }
            }
            5 => {
                for (i, d) in dpb.iter_mut().enumerate() {
                    if !cur.is(i) {
                        d.set_marking(None, Marking::Unused);
                    }
                }
                *max_lt_idx = None;
                mmco5 = true;
            }
            6 => {
                let idx = m.long_term_frame_idx;
                for (j, d) in dpb.iter_mut().enumerate() {
                    if !cur.is(j)
                        && d.has(Marking::LongTerm)
                        && d.store().long_term_frame_idx == idx
                    {
                        unmark(d, Marking::LongTerm);
                    }
                }
                match cur {
                    Current::New(new) => {
                        new.marking = Marking::LongTerm;
                        new.long_term_frame_idx = Some(idx);
                    }
                    Current::Stored(at) => {
                        // The second field of a frame store whose first field is stored.
                        let d = &mut dpb[*at];
                        let other = nums.structure.opposite();
                        if d.mark(other) == Marking::LongTerm
                            && d.store().long_term_frame_idx != idx
                        {
                            d.set_marking(Some(other.parity()), Marking::Unused);
                        }
                        d.set_marking(Some(nums.structure.parity()), Marking::LongTerm);
                        d.picture_mut().long_term_frame_idx = idx;
                    }
                }
            }
            _ => {}
        }
    }
    mmco5
}
