//! Slice segment header (clause 7.3.6, semantics 7.4.7).

use crate::bits::BitReader;
use crate::error::{DecodeError, Result};
use crate::nal;
use crate::ps::{Pps, ShortTermRps, Sps, parse_st_rps};

pub const SLICE_B: u8 = 0;
pub const SLICE_P: u8 = 1;
pub const SLICE_I: u8 = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct LongTermEntry {
    pub poc_lsb: u32,
    pub used_by_curr: bool,
    pub msb_present: bool,
    pub delta_poc_msb_cycle: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WeightEntry {
    pub luma_weight: i32,
    pub luma_offset: i32,
    pub chroma_weight: [i32; 2],
    pub chroma_offset: [i32; 2],
    /// luma_weight_flag || chroma_weight_flag (explicit weights differ from default).
    pub present: bool,
}

#[derive(Clone, Debug)]
pub struct PredWeightTable {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    pub l: [[WeightEntry; 16]; 2],
}

#[derive(Clone, Debug)]
pub struct SliceHeader {
    pub nal_unit_type: u8,
    pub temporal_id: u8,
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u8,
    pub dependent_slice_segment: bool,
    pub segment_address: u32,
    pub slice_type: u8,
    pub pic_output_flag: bool,
    pub poc_lsb: u32,
    pub st_rps: ShortTermRps,
    pub long_term: Vec<LongTermEntry>,
    pub num_long_term_sps: u32,
    pub temporal_mvp: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx: [u32; 2],
    pub rpl_mod: [bool; 2],
    pub list_entry: [[u8; 16]; 2],
    pub mvd_l1_zero: bool,
    pub cabac_init: bool,
    pub collocated_from_l0: bool,
    pub collocated_ref_idx: u32,
    pub pwt: Option<Box<PredWeightTable>>,
    pub max_num_merge_cand: u32,
    pub slice_qp: i32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    pub entry_points: Vec<u32>,
    /// Byte offset of slice_segment_data() within the RBSP (header bytes excluded).
    pub data_offset: usize,
    /// SliceAddrRs of the slice containing this segment.
    pub slice_addr_rs: u32,
}

impl SliceHeader {
    pub fn num_pic_total_curr(&self) -> u32 {
        let mut n = 0;
        n += self.st_rps.used[..self.st_rps.num_delta_pocs()]
            .iter()
            .filter(|&&u| u)
            .count() as u32;
        for lt in &self.long_term {
            if lt.used_by_curr {
                n += 1;
            }
        }
        n
    }
}

fn ceil_log2(v: u32) -> u32 {
    if v <= 1 {
        0
    } else {
        32 - (v - 1).leading_zeros()
    }
}

/// Peek the PPS id and first_slice_segment_in_pic_flag without full parsing.
pub fn peek_pps_id(rbsp: &[u8], nal_unit_type: u8) -> Result<(bool, u8)> {
    let mut r = BitReader::new(rbsp);
    let first = r.flag()?;
    if (nal::BLA_W_LP..=nal::RSV_IRAP_23).contains(&nal_unit_type) {
        r.skip(1)?;
    }
    let id = r.ue_max(63, "slice_pic_parameter_set_id")? as u8;
    Ok((first, id))
}

fn parse_pwt(r: &mut BitReader, sh: &SliceHeader) -> Result<PredWeightTable> {
    let luma_log2_denom = r.ue_max(7, "luma_log2_weight_denom")?;
    let dc = r.se()?;
    let chroma_log2_denom = luma_log2_denom as i32 + dc;
    if !(0..=7).contains(&chroma_log2_denom) {
        return Err(DecodeError::InvalidData("ChromaLog2WeightDenom"));
    }
    let chroma_log2_denom = chroma_log2_denom as u32;
    let mut t = PredWeightTable {
        luma_log2_denom,
        chroma_log2_denom,
        l: [[WeightEntry::default(); 16]; 2],
    };
    let lists = if sh.slice_type == SLICE_B { 2 } else { 1 };
    let half_c = 128i32; // WpOffsetHalfRangeC without high_precision_offsets
    for list in 0..lists {
        let n = sh.num_ref_idx[list] as usize;
        let mut lf = [false; 16];
        let mut cf = [false; 16];
        for f in lf.iter_mut().take(n) {
            *f = r.flag()?;
        }
        for f in cf.iter_mut().take(n) {
            *f = r.flag()?;
        }
        for i in 0..n {
            let e = &mut t.l[list][i];
            e.luma_weight = 1 << luma_log2_denom;
            e.luma_offset = 0;
            e.chroma_weight = [1 << chroma_log2_denom; 2];
            e.chroma_offset = [0; 2];
            e.present = lf[i] || cf[i];
            if lf[i] {
                let dw = r.se_range(-128, 127, "delta_luma_weight")?;
                e.luma_weight = (1 << luma_log2_denom) + dw;
                e.luma_offset = r.se_range(-128, 127, "luma_offset")?;
            }
            if cf[i] {
                for j in 0..2 {
                    let dw = r.se_range(-128, 127, "delta_chroma_weight")?;
                    let w = (1 << chroma_log2_denom) + dw;
                    let doff = r.se_range(-4 * half_c, 4 * half_c - 1, "delta_chroma_offset")?;
                    let off = (half_c + doff - ((half_c * w) >> chroma_log2_denom))
                        .clamp(-half_c, half_c - 1);
                    e.chroma_weight[j] = w;
                    e.chroma_offset[j] = off;
                }
            }
        }
    }
    Ok(t)
}

/// Parse a slice segment header. `prev` is the header of the preceding independent slice
/// segment of the same picture (required for dependent slice segments).
pub fn parse_slice_header(
    rbsp: &[u8],
    nal_unit_type: u8,
    temporal_id: u8,
    sps: &Sps,
    pps: &Pps,
    prev: Option<&SliceHeader>,
) -> Result<SliceHeader> {
    let mut r = BitReader::new(rbsp);
    let first_slice_segment_in_pic = r.flag()?;
    let mut no_output_of_prior_pics = false;
    if (nal::BLA_W_LP..=nal::RSV_IRAP_23).contains(&nal_unit_type) {
        no_output_of_prior_pics = r.flag()?;
    }
    let pps_id = r.ue_max(63, "slice_pic_parameter_set_id")? as u8;
    let mut dependent = false;
    let mut segment_address = 0;
    if !first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled {
            dependent = r.flag()?;
        }
        segment_address = r.u(ceil_log2(sps.pic_size_ctb()))?;
        if segment_address >= sps.pic_size_ctb() {
            return Err(DecodeError::InvalidData("slice_segment_address"));
        }
    }
    let mut sh = if dependent {
        let p = prev.ok_or(DecodeError::InvalidData(
            "dependent slice segment without independent segment",
        ))?;
        let mut s = p.clone();
        s.nal_unit_type = nal_unit_type;
        s.temporal_id = temporal_id;
        s.first_slice_segment_in_pic = false;
        s.no_output_of_prior_pics = no_output_of_prior_pics;
        s.dependent_slice_segment = true;
        s.segment_address = segment_address;
        s.entry_points.clear();
        s
    } else {
        SliceHeader {
            nal_unit_type,
            temporal_id,
            first_slice_segment_in_pic,
            no_output_of_prior_pics,
            pps_id,
            dependent_slice_segment: false,
            segment_address,
            slice_type: SLICE_I,
            pic_output_flag: true,
            poc_lsb: 0,
            st_rps: ShortTermRps::default(),
            long_term: Vec::new(),
            num_long_term_sps: 0,
            temporal_mvp: false,
            sao_luma: false,
            sao_chroma: false,
            num_ref_idx: [0, 0],
            rpl_mod: [false, false],
            list_entry: [[0; 16]; 2],
            mvd_l1_zero: false,
            cabac_init: false,
            collocated_from_l0: true,
            collocated_ref_idx: 0,
            pwt: None,
            max_num_merge_cand: 5,
            slice_qp: 26,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            deblocking_disabled: pps.deblocking_disabled,
            beta_offset_div2: pps.beta_offset_div2,
            tc_offset_div2: pps.tc_offset_div2,
            loop_filter_across_slices: pps.loop_filter_across_slices,
            entry_points: Vec::new(),
            data_offset: 0,
            slice_addr_rs: segment_address,
        }
    };
    sh.pps_id = pps_id;
    if !dependent {
        r.skip(pps.num_extra_slice_header_bits as usize)?;
        let st = r.ue_max(2, "slice_type")? as u8;
        sh.slice_type = st;
        if pps.output_flag_present {
            sh.pic_output_flag = r.flag()?;
        }
        if !nal::is_idr(nal_unit_type) {
            sh.poc_lsb = r.u(sps.log2_max_poc_lsb)?;
            let st_sps = r.flag()?;
            let num_st = sps.st_rps.len();
            if !st_sps {
                let mdpb = sps.max_dec_pic_buffering[sps.max_sub_layers_minus1 as usize] - 1;
                sh.st_rps = parse_st_rps(&mut r, num_st, num_st, &sps.st_rps, mdpb)?;
            } else {
                if num_st == 0 {
                    return Err(DecodeError::InvalidData(
                        "short_term_ref_pic_set_sps_flag without SPS sets",
                    ));
                }
                let idx = if num_st > 1 {
                    r.u(ceil_log2(num_st as u32))? as usize
                } else {
                    0
                };
                if idx >= num_st {
                    return Err(DecodeError::InvalidData("short_term_ref_pic_set_idx"));
                }
                sh.st_rps = sps.st_rps[idx].clone();
            }
            if sps.long_term_refs_present {
                let nsps = sps.lt_ref_pic_poc_lsb.len() as u32;
                let num_lt_sps = if nsps > 0 {
                    r.ue_max(nsps, "num_long_term_sps")?
                } else {
                    0
                };
                let num_lt_pics = r.ue_max(32, "num_long_term_pics")?;
                if num_lt_sps + num_lt_pics > 32 {
                    return Err(DecodeError::InvalidData("too many long-term pictures"));
                }
                sh.num_long_term_sps = num_lt_sps;
                for i in 0..num_lt_sps + num_lt_pics {
                    let mut e = LongTermEntry::default();
                    if i < num_lt_sps {
                        let idx = if nsps > 1 { r.u(ceil_log2(nsps))? } else { 0 };
                        if idx >= nsps {
                            return Err(DecodeError::InvalidData("lt_idx_sps"));
                        }
                        e.poc_lsb = sps.lt_ref_pic_poc_lsb[idx as usize];
                        e.used_by_curr = sps.lt_used_by_curr[idx as usize];
                    } else {
                        e.poc_lsb = r.u(sps.log2_max_poc_lsb)?;
                        e.used_by_curr = r.flag()?;
                    }
                    e.msb_present = r.flag()?;
                    if e.msb_present {
                        e.delta_poc_msb_cycle = r.ue()?;
                    }
                    sh.long_term.push(e);
                }
                // DeltaPocMsbCycleLt accumulation (7-52).
                for i in 0..sh.long_term.len() {
                    if i != 0 && i as u32 != num_lt_sps {
                        let p = sh.long_term[i - 1].delta_poc_msb_cycle;
                        sh.long_term[i].delta_poc_msb_cycle =
                            sh.long_term[i].delta_poc_msb_cycle.wrapping_add(p);
                    }
                }
            }
            if sps.temporal_mvp {
                sh.temporal_mvp = r.flag()?;
            }
        }
        if sps.sao {
            sh.sao_luma = r.flag()?;
            sh.sao_chroma = r.flag()?;
        }
        if sh.slice_type != SLICE_I {
            sh.num_ref_idx = [
                pps.num_ref_idx_l0_default,
                if st == SLICE_B {
                    pps.num_ref_idx_l1_default
                } else {
                    0
                },
            ];
            if r.flag()? {
                sh.num_ref_idx[0] = r.ue_max(14, "num_ref_idx_l0_active_minus1")? + 1;
                if st == SLICE_B {
                    sh.num_ref_idx[1] = r.ue_max(14, "num_ref_idx_l1_active_minus1")? + 1;
                }
            }
            let total = sh.num_pic_total_curr();
            if total == 0 {
                return Err(DecodeError::InvalidData(
                    "P/B slice without reference pictures",
                ));
            }
            if pps.lists_modification_present && total > 1 {
                let bits = ceil_log2(total);
                sh.rpl_mod[0] = r.flag()?;
                if sh.rpl_mod[0] {
                    for i in 0..sh.num_ref_idx[0] as usize {
                        let v = r.u(bits)?;
                        if v >= total {
                            return Err(DecodeError::InvalidData("list_entry_l0"));
                        }
                        sh.list_entry[0][i] = v as u8;
                    }
                }
                if st == SLICE_B {
                    sh.rpl_mod[1] = r.flag()?;
                    if sh.rpl_mod[1] {
                        for i in 0..sh.num_ref_idx[1] as usize {
                            let v = r.u(bits)?;
                            if v >= total {
                                return Err(DecodeError::InvalidData("list_entry_l1"));
                            }
                            sh.list_entry[1][i] = v as u8;
                        }
                    }
                }
            }
            if st == SLICE_B {
                sh.mvd_l1_zero = r.flag()?;
            }
            if pps.cabac_init_present {
                sh.cabac_init = r.flag()?;
            }
            if sh.temporal_mvp {
                if st == SLICE_B {
                    sh.collocated_from_l0 = r.flag()?;
                }
                if (sh.collocated_from_l0 && sh.num_ref_idx[0] > 1)
                    || (!sh.collocated_from_l0 && sh.num_ref_idx[1] > 1)
                {
                    let lim = if sh.collocated_from_l0 {
                        sh.num_ref_idx[0]
                    } else {
                        sh.num_ref_idx[1]
                    } - 1;
                    sh.collocated_ref_idx = r.ue_max(lim, "collocated_ref_idx")?;
                }
            }
            if (pps.weighted_pred && st == SLICE_P) || (pps.weighted_bipred && st == SLICE_B) {
                sh.pwt = Some(Box::new(parse_pwt(&mut r, &sh)?));
            }
            let five_minus = r.ue_max(4, "five_minus_max_num_merge_cand")?;
            sh.max_num_merge_cand = 5 - five_minus;
        }
        let qd = r.se()?;
        sh.slice_qp = pps.init_qp + qd;
        let bd_off = 6 * (sps.bit_depth_luma as i32 - 8);
        if sh.slice_qp < -bd_off || sh.slice_qp > 51 {
            return Err(DecodeError::InvalidData("SliceQpY"));
        }
        if pps.slice_chroma_qp_offsets_present {
            sh.cb_qp_offset = r.se_range(-12, 12, "slice_cb_qp_offset")?;
            sh.cr_qp_offset = r.se_range(-12, 12, "slice_cr_qp_offset")?;
        }
        let mut override_flag = false;
        if pps.deblocking_override_enabled {
            override_flag = r.flag()?;
        }
        if override_flag {
            sh.deblocking_disabled = r.flag()?;
            if !sh.deblocking_disabled {
                sh.beta_offset_div2 = r.se_range(-6, 6, "slice_beta_offset_div2")?;
                sh.tc_offset_div2 = r.se_range(-6, 6, "slice_tc_offset_div2")?;
            }
        }
        if pps.loop_filter_across_slices
            && (sh.sao_luma || sh.sao_chroma || !sh.deblocking_disabled)
        {
            sh.loop_filter_across_slices = r.flag()?;
        }
        sh.slice_addr_rs = segment_address;
    }
    if pps.tiles_enabled || pps.entropy_coding_sync {
        let n = r.ue_max(sps.pic_size_ctb(), "num_entry_point_offsets")?;
        if n > 0 {
            let len = r.ue_max(31, "offset_len_minus1")? + 1;
            sh.entry_points.reserve(n as usize);
            for _ in 0..n {
                let v = if len == 32 { r.u(32)? } else { r.u(len)? };
                sh.entry_points.push(v.wrapping_add(1));
            }
        }
    }
    if pps.slice_header_extension_present {
        let len = r.ue_max(256, "slice_segment_header_extension_length")?;
        r.skip(len as usize * 8)?;
    }
    // byte_alignment(): alignment_bit_equal_to_one then zeros.
    let one = r.flag()?;
    if !one {
        return Err(DecodeError::InvalidData("slice header byte_alignment"));
    }
    r.align();
    sh.data_offset = r.byte_pos();
    Ok(sh)
}
