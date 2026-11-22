//! Slice header (clause 7.3.3 / 7.4.3), reference list modification (7.3.3.1), prediction
//! weight table (7.3.3.2) and decoded reference picture marking (7.3.3.3).

use crate::DecodeError;
use crate::nal::NalHeader;
use crate::picture::Structure;
use crate::ps::{PocType, Pps, R, Sps};
use triergon_bitstream::BitReader;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SliceType {
    P,
    B,
    I,
    SP,
    SI,
}

impl SliceType {
    pub(crate) fn is_intra(self) -> bool {
        matches!(self, SliceType::I | SliceType::SI)
    }
}

/// One memory_management_control_operation with its operands (7.3.3.3, Table 7-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mmco {
    /// 1: the short-term picture `difference_of_pic_nums_minus1 + 1` below the current
    /// picture number becomes unused.
    ShortTermUnused { difference_minus1: u32 },
    /// 2: the long-term picture `long_term_pic_num` becomes unused.
    LongTermUnused { long_term_pic_num: u32 },
    /// 3: that short-term picture becomes long-term with `long_term_frame_idx`.
    ShortTermToLongTerm {
        difference_minus1: u32,
        long_term_frame_idx: u32,
    },
    /// 4: long-term indices from `max_long_term_frame_idx_plus1` on become unused.
    MaxLongTermIdx { plus1: u32 },
    /// 5: every reference becomes unused.
    AllUnused,
    /// 6: the current picture becomes long-term with `long_term_frame_idx`.
    CurrentToLongTerm { long_term_frame_idx: u32 },
}

impl Mmco {
    /// memory_management_control_operation.
    #[cfg(feature = "inspect")]
    pub(crate) fn op(self) -> u32 {
        match self {
            Mmco::ShortTermUnused { .. } => 1,
            Mmco::LongTermUnused { .. } => 2,
            Mmco::ShortTermToLongTerm { .. } => 3,
            Mmco::MaxLongTermIdx { .. } => 4,
            Mmco::AllUnused => 5,
            Mmco::CurrentToLongTerm { .. } => 6,
        }
    }
}

/// One reordering command of ref_pic_list_modification(), by its
/// modification_of_pic_nums_idc (7.3.3.1, Table 7-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListModification {
    /// 0: the picture number prediction minus `abs_diff_pic_num_minus1 + 1`.
    Subtract(u32),
    /// 1: the picture number prediction plus `abs_diff_pic_num_minus1 + 1`.
    Add(u32),
    /// 2: the long-term picture `long_term_pic_num`.
    LongTerm(u32),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WeightEntry {
    pub(crate) luma: Option<(i32, i32)>,
    pub(crate) chroma: [Option<(i32, i32)>; 2],
}

#[derive(Clone, Debug)]
pub(crate) struct SliceHeader {
    pub(crate) nal: NalHeader,
    pub(crate) idr: bool,
    pub(crate) first_mb: u32,
    pub(crate) slice_type: SliceType,
    pub(crate) frame_num: u32,
    pub(crate) field_pic: bool,
    pub(crate) bottom_field: bool,
    pub(crate) idr_pic_id: u32,
    pub(crate) poc_lsb: u32,
    pub(crate) delta_poc_bottom: i32,
    pub(crate) delta_poc: [i32; 2],
    pub(crate) redundant_pic_cnt: u32,
    pub(crate) direct_spatial_mv_pred: bool,
    pub(crate) num_ref_idx_active: [u32; 2],
    pub(crate) ref_mods: [Vec<ListModification>; 2],
    pub(crate) luma_log2_wd: u32,
    pub(crate) chroma_log2_wd: u32,
    pub(crate) weights: Option<Box<[[WeightEntry; 32]; 2]>>,
    pub(crate) no_output_of_prior_pics: bool,
    pub(crate) long_term_reference_flag: bool,
    pub(crate) adaptive_ref_pic_marking: bool,
    pub(crate) mmcos: Vec<Mmco>,
    pub(crate) cabac_init_idc: u32,
    pub(crate) slice_qp: i32,
    pub(crate) disable_deblocking_filter_idc: u32,
    pub(crate) slice_alpha_c0_offset: i32,
    pub(crate) slice_beta_offset: i32,
    pub(crate) mbaff: bool,
    /// Bit position of slice_data() in the RBSP.
    pub(crate) data_bit_pos: usize,
}

impl SliceHeader {
    /// The coded picture the slice belongs to: a frame or one of its fields.
    pub(crate) fn structure(&self) -> Structure {
        match (self.field_pic, self.bottom_field) {
            (false, _) => Structure::Frame,
            (true, false) => Structure::Top,
            (true, true) => Structure::Bottom,
        }
    }
}

fn inval(what: &'static str) -> DecodeError {
    DecodeError::Invalid(what)
}

/// The slice header fields before the parameter sets are known.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prefix {
    pub(crate) first_mb: u32,
    pub(crate) slice_type: SliceType,
    pub(crate) pps_id: u32,
}

impl Prefix {
    /// first_mb_in_slice, slice_type and pic_parameter_set_id.
    pub(crate) fn read(r: &mut BitReader<'_>) -> R<Prefix> {
        let first_mb = r.ue();
        let st = r.ue();
        let pps_id = r.ue();
        if pps_id > 255 || st > 9 {
            return Err(inval("slice header"));
        }
        let slice_type = match st % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::SP,
            _ => SliceType::SI,
        };
        Ok(Prefix {
            first_mb,
            slice_type,
            pps_id,
        })
    }
}

/// The rest of the slice header after `prefix`, which named `pps` (and through it `sps`).
pub(crate) fn parse_slice_header(
    r: &mut BitReader<'_>,
    prefix: Prefix,
    nal: NalHeader,
    sps: &Sps,
    pps: &Pps,
) -> R<SliceHeader> {
    let Prefix {
        first_mb,
        slice_type,
        ..
    } = prefix;
    let idr = nal.nal_unit_type == 5;
    if sps.separate_colour_plane {
        r.read(2);
    }
    let frame_num = r.read(sps.log2_max_frame_num);
    let mut field_pic = false;
    let mut bottom_field = false;
    if !sps.frame_mbs_only {
        field_pic = r.bit();
        if field_pic {
            bottom_field = r.bit();
        }
    }
    let mbaff = sps.mb_adaptive_frame_field && !field_pic;
    let pic_size_in_mbs = sps.pic_width_in_mbs * sps.frame_height_in_mbs() / (1 + field_pic as u32);
    if first_mb
        .checked_mul(1 + mbaff as u32)
        .is_none_or(|first| first >= pic_size_in_mbs)
    {
        return Err(inval("first_mb_in_slice"));
    }
    let mut idr_pic_id = 0;
    if idr {
        idr_pic_id = r.ue();
    }
    let mut poc_lsb = 0;
    let mut delta_poc_bottom = 0;
    let mut delta_poc = [0; 2];
    match sps.poc {
        PocType::Lsb { log2_max_lsb } => {
            poc_lsb = r.read(log2_max_lsb);
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                delta_poc_bottom = r.se();
            }
        }
        PocType::Cycle {
            delta_always_zero: false,
            ..
        } => {
            delta_poc[0] = r.se();
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                delta_poc[1] = r.se();
            }
        }
        PocType::Cycle { .. } | PocType::FrameNum => {}
    }
    let mut redundant_pic_cnt = 0;
    if pps.redundant_pic_cnt_present {
        redundant_pic_cnt = r.ue();
    }
    let mut direct_spatial_mv_pred = false;
    if slice_type == SliceType::B {
        direct_spatial_mv_pred = r.bit();
    }
    let mut num_ref_idx_active = pps.num_ref_idx_default;
    if matches!(slice_type, SliceType::P | SliceType::SP | SliceType::B) {
        if r.bit() {
            num_ref_idx_active[0] = r.ue().wrapping_add(1);
            if slice_type == SliceType::B {
                num_ref_idx_active[1] = r.ue().wrapping_add(1);
            }
        }
        let max = if field_pic { 32 } else { 16 };
        if num_ref_idx_active[0] > max || num_ref_idx_active[1] > max {
            return Err(inval("num_ref_idx_active_minus1"));
        }
    }
    if slice_type != SliceType::B {
        num_ref_idx_active[1] = 0;
    }
    if slice_type.is_intra() {
        num_ref_idx_active[0] = 0;
    }
    if matches!(nal.nal_unit_type, 20 | 21) {
        return Err(DecodeError::Unsupported("MVC/SVC slice extension"));
    }
    let mut ref_mods: [Vec<ListModification>; 2] = [Vec::new(), Vec::new()];
    for list in 0..2 {
        let present = if list == 0 {
            !slice_type.is_intra()
        } else {
            slice_type == SliceType::B
        };
        if present && r.bit() {
            loop {
                let idc = r.ue();
                if idc == 3 {
                    break;
                }
                if ref_mods[list].len() > 64 || r.overrun() {
                    return Err(inval("modification_of_pic_nums_idc"));
                }
                ref_mods[list].push(match idc {
                    0 => ListModification::Subtract(r.ue()),
                    1 => ListModification::Add(r.ue()),
                    2 => ListModification::LongTerm(r.ue()),
                    _ => return Err(inval("modification_of_pic_nums_idc")),
                });
            }
        }
    }
    let mut luma_log2_wd = 0;
    let mut chroma_log2_wd = 0;
    let mut weights = None;
    if (pps.weighted_pred && matches!(slice_type, SliceType::P | SliceType::SP))
        || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B)
    {
        luma_log2_wd = r.ue();
        if luma_log2_wd > 7 {
            return Err(inval("luma_log2_weight_denom"));
        }
        let chroma = sps.chroma_array_type() != 0;
        if chroma {
            chroma_log2_wd = r.ue();
            if chroma_log2_wd > 7 {
                return Err(inval("chroma_log2_weight_denom"));
            }
        }
        let mut w = Box::new([[WeightEntry::default(); 32]; 2]);
        let nlists = if slice_type == SliceType::B { 2 } else { 1 };
        for list in 0..nlists {
            for entry in w[list].iter_mut().take(num_ref_idx_active[list] as usize) {
                if r.bit() {
                    let lw = r.se();
                    let lo = r.se();
                    if !(-128..=127).contains(&lw) || !(-128..=127).contains(&lo) {
                        return Err(inval("luma weight"));
                    }
                    entry.luma = Some((lw, lo));
                }
                if chroma && r.bit() {
                    for c in &mut entry.chroma {
                        let cw = r.se();
                        let co = r.se();
                        if !(-128..=127).contains(&cw) || !(-128..=127).contains(&co) {
                            return Err(inval("chroma weight"));
                        }
                        *c = Some((cw, co));
                    }
                }
            }
        }
        weights = Some(w);
    }
    let mut no_output_of_prior_pics = false;
    let mut long_term_reference_flag = false;
    let mut adaptive_ref_pic_marking = false;
    let mut mmcos = Vec::new();
    if nal.nal_ref_idc != 0 {
        if idr {
            no_output_of_prior_pics = r.bit();
            long_term_reference_flag = r.bit();
        } else {
            adaptive_ref_pic_marking = r.bit();
            if adaptive_ref_pic_marking {
                loop {
                    let op = r.ue();
                    if op == 0 {
                        break;
                    }
                    if mmcos.len() > 66 || r.overrun() {
                        return Err(inval("memory_management_control_operation"));
                    }
                    mmcos.push(match op {
                        1 => Mmco::ShortTermUnused {
                            difference_minus1: r.ue(),
                        },
                        2 => Mmco::LongTermUnused {
                            long_term_pic_num: r.ue(),
                        },
                        3 => Mmco::ShortTermToLongTerm {
                            difference_minus1: r.ue(),
                            long_term_frame_idx: r.ue(),
                        },
                        4 => Mmco::MaxLongTermIdx { plus1: r.ue() },
                        5 => Mmco::AllUnused,
                        6 => Mmco::CurrentToLongTerm {
                            long_term_frame_idx: r.ue(),
                        },
                        _ => return Err(inval("memory_management_control_operation")),
                    });
                }
            }
        }
    }
    let mut cabac_init_idc = 0;
    if pps.entropy_coding_mode && !slice_type.is_intra() {
        cabac_init_idc = r.ue();
        if cabac_init_idc > 2 {
            return Err(inval("cabac_init_idc"));
        }
    }
    let slice_qp = pps
        .pic_init_qp
        .checked_add(r.se())
        .filter(|qp| (0..=51).contains(qp))
        .ok_or(inval("slice_qp_delta"))?;
    if matches!(slice_type, SliceType::SP | SliceType::SI) {
        if slice_type == SliceType::SP {
            r.bit();
        }
        r.se();
    }
    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset = 0;
    let mut slice_beta_offset = 0;
    if pps.deblocking_filter_control_present {
        disable_deblocking_filter_idc = r.ue();
        if disable_deblocking_filter_idc > 2 {
            return Err(inval("disable_deblocking_filter_idc"));
        }
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset = r.se().wrapping_mul(2);
            slice_beta_offset = r.se().wrapping_mul(2);
            if !(-12..=12).contains(&slice_alpha_c0_offset)
                || !(-12..=12).contains(&slice_beta_offset)
            {
                return Err(inval("slice_alpha/beta_offset_div2"));
            }
        }
    }
    if r.overrun() {
        return Err(inval("slice header truncated"));
    }
    Ok(SliceHeader {
        nal,
        idr,
        first_mb,
        slice_type,
        frame_num,
        field_pic,
        bottom_field,
        idr_pic_id,
        poc_lsb,
        delta_poc_bottom,
        delta_poc,
        redundant_pic_cnt,
        direct_spatial_mv_pred,
        num_ref_idx_active,
        ref_mods,
        luma_log2_wd,
        chroma_log2_wd,
        weights,
        no_output_of_prior_pics,
        long_term_reference_flag,
        adaptive_ref_pic_marking,
        mmcos,
        cabac_init_idc,
        slice_qp,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset,
        slice_beta_offset,
        mbaff,
        data_bit_pos: r.position(),
    })
}
