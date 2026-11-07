//! Sequence and picture parameter sets (clauses 7.3.2.1, 7.3.2.2, 7.4.2.1, 7.4.2.2, Annex E VUI).

use crate::DecodeError;
use crate::tables::*;
use triergon_bitstream::BitReader;

pub(crate) type R<T> = Result<T, DecodeError>;

fn inval(what: &'static str) -> DecodeError {
    DecodeError::InvalidData(what)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vui {
    pub sar: (u32, u32),
    pub video_full_range: bool,
    pub colour_description_present: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub timing: Option<(u32, u32, bool)>,
    pub bitstream_restriction: bool,
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
    pub pic_struct_present: bool,
    pub nal_hrd: bool,
    pub vcl_hrd: bool,
    pub cpb_removal_delay_len: u8,
    pub dpb_output_delay_len: u8,
    pub time_offset_len: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub qpprime_y_zero_transform_bypass: bool,
    pub seq_scaling_matrix_present: bool,
    /// Scaling lists in zig-zag (scan) order: 6 4x4 and 6 8x8 (only 2 used for 4:2:0).
    pub scaling4: [[u8; 16]; 6],
    pub scaling8: [[u8; 64]; 6],
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub pic_width_in_mbs: u32,
    pub pic_height_in_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    pub crop: [u32; 4], // left, right, top, bottom in frame_crop_*_offset units
    pub vui: Option<Vui>,
}

impl Sps {
    pub fn frame_height_in_mbs(&self) -> u32 {
        (2 - self.frame_mbs_only as u32).wrapping_mul(self.pic_height_in_map_units)
    }
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }
    /// Cropped output rectangle (x, y, w, h) in luma samples, equations 7-19 .. 7-22.
    pub fn crop_rect(&self) -> (u32, u32, u32, u32) {
        let (sub_w, sub_h) = match self.chroma_array_type() {
            0 => (1, 1),
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let crop_unit_x = sub_w;
        let crop_unit_y = sub_h * (2 - self.frame_mbs_only as u32);
        let w = self.pic_width_in_mbs * 16;
        let h = self.frame_height_in_mbs() * 16;
        let x0 = crop_unit_x * self.crop[0];
        let x1 = w.saturating_sub(crop_unit_x * self.crop[1]);
        let y0 = crop_unit_y * self.crop[2];
        let y1 = h.saturating_sub(crop_unit_y * self.crop[3]);
        if x1 <= x0 || y1 <= y0 {
            return (0, 0, w, h);
        }
        (x0, y0, x1 - x0, y1 - y0)
    }
    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_slice_groups: u32,
    pub num_ref_idx_default: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp: i32,
    pub pic_init_qs: i32,
    pub chroma_qp_index_offset: i32,
    pub second_chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub pic_scaling_matrix_present: bool,
    pub scaling4: [[u8; 16]; 6],
    pub scaling8: [[u8; 64]; 6],
}

/// Sequence-level 4x4 and 8x8 lists used by fall-back rule B.
type ScalingFallback<'a> = (&'a [[u8; 16]; 6], &'a [[u8; 64]; 6]);

/// scaling_list() syntax, clause 7.3.2.1.1.1. Returns true when useDefaultScalingMatrixFlag.
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn scaling_list(r: &mut BitReader<'_>, list: &mut [u8]) -> R<bool> {
    let mut last = 8i32;
    let mut next = 8i32;
    for j in 0..list.len() {
        if next != 0 {
            let delta = r.se();
            if !(-128..=127).contains(&delta) {
                return Err(inval("delta_scale out of range"));
            }
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return Ok(true);
            }
        }
        list[j] = if next == 0 { last as u8 } else { next as u8 };
        last = list[j] as i32;
    }
    Ok(false)
}

/// Parse the scaling matrix lists with fall-back rule A (SPS, `fallback = None`) or B (PPS,
/// `fallback = Some(sps lists)`), Table 7-2.
fn parse_scaling_matrix(
    r: &mut BitReader<'_>,
    n_lists: usize,
    s4: &mut [[u8; 16]; 6],
    s8: &mut [[u8; 64]; 6],
    fallback: Option<ScalingFallback<'_>>,
) -> R<()> {
    for i in 0..n_lists {
        let present = r.bit();
        if i < 6 {
            let use_default = if present {
                scaling_list(r, &mut s4[i])?
            } else {
                false
            };
            if use_default {
                s4[i] = if i < 3 {
                    DEFAULT_4X4_INTRA
                } else {
                    DEFAULT_4X4_INTER
                };
            } else if !present {
                s4[i] = match (i, fallback) {
                    (0, None) => DEFAULT_4X4_INTRA,
                    (3, None) => DEFAULT_4X4_INTER,
                    (0, Some((f4, _))) | (3, Some((f4, _))) => f4[i],
                    _ => s4[i - 1],
                };
            }
        } else {
            let k = i - 6;
            let use_default = if present {
                scaling_list(r, &mut s8[k])?
            } else {
                false
            };
            if use_default {
                s8[k] = if k % 2 == 0 {
                    DEFAULT_8X8_INTRA
                } else {
                    DEFAULT_8X8_INTER
                };
            } else if !present {
                s8[k] = match (k, fallback) {
                    (0, None) => DEFAULT_8X8_INTRA,
                    (1, None) => DEFAULT_8X8_INTER,
                    (0, Some((_, f8))) | (1, Some((_, f8))) => f8[k],
                    _ => s8[k - 2],
                };
            }
        }
    }
    Ok(())
}

fn hrd(r: &mut BitReader<'_>, v: &mut Vui) -> R<()> {
    let cpb_cnt = r.ue().wrapping_add(1);
    if cpb_cnt > 32 {
        return Err(inval("cpb_cnt_minus1"));
    }
    r.read(4);
    r.read(4);
    for _ in 0..cpb_cnt {
        r.ue();
        r.ue();
        r.bit();
    }
    r.read(5);
    v.cpb_removal_delay_len = r.read(5) as u8 + 1;
    v.dpb_output_delay_len = r.read(5) as u8 + 1;
    v.time_offset_len = r.read(5) as u8;
    Ok(())
}

fn parse_vui(r: &mut BitReader<'_>) -> R<Vui> {
    let mut v = Vui::default();
    if r.bit() {
        let idc = r.read(8);
        if idc == 255 {
            let w = r.read(16);
            let h = r.read(16);
            v.sar = (w, h);
        } else if (idc as usize) < SAR_TABLE.len() {
            v.sar = SAR_TABLE[idc as usize];
        }
    }
    if r.bit() {
        r.bit(); // overscan_appropriate_flag
    }
    v.colour_primaries = 2;
    v.transfer_characteristics = 2;
    v.matrix_coefficients = 2;
    if r.bit() {
        r.read(3); // video_format
        v.video_full_range = r.bit();
        if r.bit() {
            v.colour_description_present = true;
            v.colour_primaries = r.read(8) as u8;
            v.transfer_characteristics = r.read(8) as u8;
            v.matrix_coefficients = r.read(8) as u8;
        }
    }
    if r.bit() {
        r.ue();
        r.ue();
    }
    if r.bit() {
        let a = r.read(32);
        let b = r.read(32);
        let f = r.bit();
        v.timing = Some((a, b, f));
    }
    v.nal_hrd = r.bit();
    if v.nal_hrd {
        hrd(r, &mut v)?;
    }
    v.vcl_hrd = r.bit();
    if v.vcl_hrd {
        hrd(r, &mut v)?;
    }
    if v.nal_hrd || v.vcl_hrd {
        r.bit(); // low_delay_hrd_flag
    }
    v.pic_struct_present = r.bit();
    // Truncated VUIs exist in the wild; stop before reading past the end.
    if r.left() <= 0 {
        return Ok(v);
    }
    v.bitstream_restriction = r.bit();
    if v.bitstream_restriction {
        r.bit();
        r.ue();
        r.ue();
        r.ue();
        r.ue();
        v.max_num_reorder_frames = r.ue();
        v.max_dec_frame_buffering = r.ue();
        if r.overrun() {
            v.bitstream_restriction = false;
            v.max_num_reorder_frames = 0;
            v.max_dec_frame_buffering = 0;
        }
    }
    Ok(v)
}

pub(crate) fn parse_sps(r: &mut BitReader<'_>) -> R<Sps> {
    let profile_idc = r.read(8) as u8;
    let constraint_flags = r.read(8) as u8;
    let level_idc = r.read(8) as u8;
    let id = r.ue();
    if id > 31 {
        return Err(inval("seq_parameter_set_id"));
    }
    let mut sps = Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        id,
        chroma_format_idc: 1,
        separate_colour_plane: false,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        qpprime_y_zero_transform_bypass: false,
        seq_scaling_matrix_present: false,
        scaling4: [[16; 16]; 6],
        scaling8: [[16; 64]; 6],
        log2_max_frame_num: 4,
        poc_type: 0,
        log2_max_poc_lsb: 4,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: Vec::new(),
        max_num_ref_frames: 0,
        gaps_in_frame_num_allowed: false,
        pic_width_in_mbs: 0,
        pic_height_in_map_units: 0,
        frame_mbs_only: true,
        mb_adaptive_frame_field: false,
        direct_8x8_inference: false,
        crop: [0; 4],
        vui: None,
    };
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        sps.chroma_format_idc = r.ue();
        if sps.chroma_format_idc > 3 {
            return Err(inval("chroma_format_idc"));
        }
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.bit();
        }
        sps.bit_depth_luma = r.ue().wrapping_add(8);
        sps.bit_depth_chroma = r.ue().wrapping_add(8);
        if sps.bit_depth_luma > 14 || sps.bit_depth_chroma > 14 {
            return Err(inval("bit_depth"));
        }
        sps.qpprime_y_zero_transform_bypass = r.bit();
        sps.seq_scaling_matrix_present = r.bit();
        if sps.seq_scaling_matrix_present {
            let n = if sps.chroma_format_idc != 3 { 8 } else { 12 };
            let mut s4 = [[16u8; 16]; 6];
            let mut s8 = [[16u8; 64]; 6];
            parse_scaling_matrix(r, n, &mut s4, &mut s8, None)?;
            sps.scaling4 = s4;
            sps.scaling8 = s8;
        }
    }
    sps.log2_max_frame_num = r.ue().wrapping_add(4);
    if sps.log2_max_frame_num > 16 {
        return Err(inval("log2_max_frame_num_minus4"));
    }
    sps.poc_type = r.ue();
    match sps.poc_type {
        0 => {
            sps.log2_max_poc_lsb = r.ue().wrapping_add(4);
            if sps.log2_max_poc_lsb > 16 {
                return Err(inval("log2_max_pic_order_cnt_lsb_minus4"));
            }
        }
        1 => {
            sps.delta_pic_order_always_zero = r.bit();
            sps.offset_for_non_ref_pic = r.se();
            sps.offset_for_top_to_bottom_field = r.se();
            let n = r.ue();
            if n > 255 {
                return Err(inval("num_ref_frames_in_pic_order_cnt_cycle"));
            }
            for _ in 0..n {
                sps.offset_for_ref_frame.push(r.se());
            }
        }
        2 => {}
        _ => return Err(inval("pic_order_cnt_type")),
    }
    sps.max_num_ref_frames = r.ue();
    if sps.max_num_ref_frames > 16 {
        return Err(inval("max_num_ref_frames"));
    }
    sps.gaps_in_frame_num_allowed = r.bit();
    sps.pic_width_in_mbs = r.ue().wrapping_add(1);
    sps.pic_height_in_map_units = r.ue().wrapping_add(1);
    sps.frame_mbs_only = r.bit();
    if !sps.frame_mbs_only {
        sps.mb_adaptive_frame_field = r.bit();
    }
    sps.direct_8x8_inference = r.bit();
    if r.bit() {
        for c in &mut sps.crop {
            *c = r.ue();
        }
    }
    if r.bit() {
        sps.vui = Some(parse_vui(r)?);
    }
    if sps.pic_width_in_mbs > 1024 || sps.frame_height_in_mbs() > 1024 {
        return Err(DecodeError::ResourceLimit("picture dimensions"));
    }
    Ok(sps)
}

pub(crate) fn parse_pps(r: &mut BitReader<'_>, spss: &[Option<Box<Sps>>]) -> R<Pps> {
    let id = r.ue();
    if id > 255 {
        return Err(inval("pic_parameter_set_id"));
    }
    let sps_id = r.ue();
    if sps_id > 31 {
        return Err(inval("seq_parameter_set_id"));
    }
    let entropy_coding_mode = r.bit();
    let bottom_field_pic_order_in_frame_present = r.bit();
    let num_slice_groups = r.ue().wrapping_add(1);
    if num_slice_groups > 8 {
        return Err(inval("num_slice_groups_minus1"));
    }
    if num_slice_groups > 1 {
        // Slice group map syntax (FMO). Parsed so the PPS is well-formed; decoding refuses it.
        let map_type = r.ue();
        match map_type {
            0 => {
                for _ in 0..num_slice_groups {
                    r.ue();
                }
            }
            2 => {
                for _ in 0..num_slice_groups - 1 {
                    r.ue();
                    r.ue();
                }
            }
            3..=5 => {
                r.bit();
                r.ue();
            }
            6 => {
                let n = r.ue().wrapping_add(1);
                let bits = 32 - (num_slice_groups - 1).leading_zeros();
                for _ in 0..n.min(1 << 20) {
                    r.read(bits);
                }
            }
            _ => {}
        }
    }
    let l0 = r.ue().wrapping_add(1);
    let l1 = r.ue().wrapping_add(1);
    if l0 > 32 || l1 > 32 {
        return Err(inval("num_ref_idx_default_active_minus1"));
    }
    let weighted_pred = r.bit();
    let weighted_bipred_idc = r.read(2);
    let pic_init_qp = 26 + r.se();
    let pic_init_qs = 26 + r.se();
    let chroma_qp_index_offset = r.se();
    if !(-12..=12).contains(&chroma_qp_index_offset) {
        return Err(inval("chroma_qp_index_offset"));
    }
    let deblocking_filter_control_present = r.bit();
    let constrained_intra_pred = r.bit();
    let redundant_pic_cnt_present = r.bit();
    let sps = spss.get(sps_id as usize).and_then(|s| s.as_deref());
    let (mut s4, mut s8) = match sps {
        Some(s) => (s.scaling4, s.scaling8),
        None => ([[16; 16]; 6], [[16; 64]; 6]),
    };
    let mut transform_8x8_mode = false;
    let mut pic_scaling_matrix_present = false;
    let mut second = chroma_qp_index_offset;
    if r.more_rbsp_data() {
        transform_8x8_mode = r.bit();
        pic_scaling_matrix_present = r.bit();
        if pic_scaling_matrix_present {
            let chroma_format_idc = sps.map(|s| s.chroma_format_idc).unwrap_or(1);
            let n = 6 + if chroma_format_idc != 3 { 2 } else { 6 } * transform_8x8_mode as usize;
            let (f4, f8) = (s4, s8);
            parse_scaling_matrix(r, n, &mut s4, &mut s8, Some((&f4, &f8)))?;
        }
        second = r.se();
        if !(-12..=12).contains(&second) {
            return Err(inval("second_chroma_qp_index_offset"));
        }
    }
    Ok(Pps {
        id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_slice_groups,
        num_ref_idx_default: [l0, l1],
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp,
        pic_init_qs,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset: second,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
        pic_scaling_matrix_present,
        scaling4: s4,
        scaling8: s8,
    })
}
