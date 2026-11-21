//! Stream feature inspection (parameter sets and slice headers only). Used to classify
//! conformance vectors into the selected feature matrix; not part of the decoding path.

use crate::nal::{parse_header, unescape};
use crate::ps::{Pps, Sps, parse_pps, parse_sps};
use crate::slice::{SliceType, parse_slice_header, peek_pps_id};
use std::collections::BTreeSet;
use triergon_bitstream::BitReader;
use triergon_codec::{Budget, BudgetVec};
use triergon_framing::nal::AnnexBUnits;

/// The coding features a stream's parameter sets and slice headers use.
#[derive(Debug, Default, Clone)]
pub struct StreamFeatures {
    /// profile_idc of every SPS.
    pub profiles: BTreeSet<u8>,
    /// constraint_set flags of every SPS (the byte after profile_idc).
    pub constraint_flags: BTreeSet<u8>,
    /// chroma_format_idc of every SPS.
    pub chroma_format_idc: BTreeSet<u32>,
    /// The larger of the luma and chroma bit depths of every SPS.
    pub bit_depth: BTreeSet<u32>,
    /// Coded frame width and height in luma samples of every SPS.
    pub sizes: BTreeSet<(u32, u32)>,
    /// entropy_coding_mode_flag of every PPS.
    pub cabac: BTreeSet<bool>,
    /// frame_mbs_only_flag of every SPS.
    pub frame_mbs_only: BTreeSet<bool>,
    /// Some SPS sets mb_adaptive_frame_field_flag.
    pub mbaff_sps: bool,
    /// Some slice is a field slice.
    pub field_pics: bool,
    /// Some slice is an MBAFF frame slice.
    pub mbaff_pics: bool,
    /// Slice types seen ("I", "P", "B", "SP", "SI").
    pub slice_types: BTreeSet<&'static str>,
    /// Some PPS has more than one slice group.
    pub fmo: bool,
    /// Some PPS sets redundant_pic_cnt_present_flag.
    pub redundant_pic_cnt_present: bool,
    /// Some slice is a redundant slice.
    pub redundant_slices: bool,
    /// Some slice carries a prediction weight table.
    pub weighted_explicit: bool,
    /// Some B slice uses implicit weighted prediction.
    pub weighted_implicit: bool,
    /// Some PPS sets transform_8x8_mode_flag.
    pub transform_8x8: bool,
    /// Some SPS or PPS carries scaling matrices.
    pub scaling_matrices: bool,
    /// pic_order_cnt_type of every SPS.
    pub poc_types: BTreeSet<u32>,
    /// Some PPS sets constrained_intra_pred_flag.
    pub constrained_intra_pred: bool,
    /// Some B slice uses spatial direct prediction.
    pub direct_spatial: bool,
    /// Some B slice uses temporal direct prediction.
    pub direct_temporal: bool,
    /// Some slice modifies a reference picture list.
    pub ref_list_mods: bool,
    /// memory_management_control_operation values seen.
    pub mmco_ops: BTreeSet<u32>,
    /// Some IDR slice sets long_term_reference_flag.
    pub long_term_idr: bool,
    /// Some picture has more than one slice.
    pub multi_slice: bool,
    /// nal_unit_type of every NAL unit.
    pub nal_types: BTreeSet<u8>,
    /// Some SPS sets gaps_in_frame_num_value_allowed_flag.
    pub gaps_allowed: bool,
    /// Some SPS sets qpprime_y_zero_transform_bypass_flag.
    pub lossless: bool,
    /// Some SPS crops the frame.
    pub crop: bool,
    /// The most slices counted in one picture.
    pub max_slices_per_pic: u32,
    /// max_num_reorder_frames and max_dec_frame_buffering of the last SPS that gives them.
    pub bitstream_restriction: Option<(u32, u32)>,
    /// The parameter sets and slice headers that failed to parse, with their errors.
    pub errors: Vec<String>,
}

/// The features of the Annex B stream `data`. Units that fail to parse are listed in
/// [`StreamFeatures::errors`]; slice data is not decoded.
pub fn inspect(data: &[u8]) -> StreamFeatures {
    let mut f = StreamFeatures::default();
    let mut spss: Vec<Option<Box<Sps>>> = vec![None; 32];
    let mut ppss: Vec<Option<Box<Pps>>> = vec![None; 256];
    let Ok(pool) = Budget::new(crate::Limits::default().live_storage_bytes) else {
        return f;
    };
    let mut buf = BudgetVec::new(&pool);
    let mut slices_in_pic = 0u32;
    for nal in AnnexBUnits::new(data) {
        let Some(h) = parse_header(nal) else { continue };
        f.nal_types.insert(h.nal_unit_type);
        let Ok(len) = unescape(&nal[1..], &mut buf) else {
            continue;
        };
        let mut r = BitReader::with_end(&buf, len * 8);
        match h.nal_unit_type {
            7 => match parse_sps(&mut r) {
                Ok(s) => {
                    f.profiles.insert(s.profile_idc);
                    f.constraint_flags.insert(s.constraint_flags);
                    f.chroma_format_idc.insert(s.chroma_format_idc);
                    f.bit_depth.insert(s.bit_depth_luma.max(s.bit_depth_chroma));
                    f.sizes
                        .insert((s.pic_width_in_mbs * 16, s.frame_height_in_mbs() * 16));
                    f.frame_mbs_only.insert(s.frame_mbs_only);
                    f.mbaff_sps |= s.mb_adaptive_frame_field;
                    f.poc_types.insert(s.poc_type);
                    f.scaling_matrices |= s.seq_scaling_matrix_present;
                    f.gaps_allowed |= s.gaps_in_frame_num_allowed;
                    f.lossless |= s.qpprime_y_zero_transform_bypass;
                    f.crop |= s.crop != [0; 4];
                    if let Some(v) = &s.vui
                        && v.bitstream_restriction
                    {
                        f.bitstream_restriction =
                            Some((v.max_num_reorder_frames, v.max_dec_frame_buffering));
                    }
                    let id = s.id as usize;
                    spss[id] = Some(Box::new(s));
                }
                Err(e) => f.errors.push(format!("sps: {e:?}")),
            },
            8 => match parse_pps(&mut r, &spss) {
                Ok(p) => {
                    f.cabac.insert(p.entropy_coding_mode);
                    f.fmo |= p.num_slice_groups > 1;
                    f.redundant_pic_cnt_present |= p.redundant_pic_cnt_present;
                    f.transform_8x8 |= p.transform_8x8_mode;
                    f.scaling_matrices |= p.pic_scaling_matrix_present;
                    f.constrained_intra_pred |= p.constrained_intra_pred;
                    let id = p.id as usize;
                    ppss[id] = Some(Box::new(p));
                }
                Err(e) => f.errors.push(format!("pps: {e:?}")),
            },
            1 | 5 => {
                let mut r2 = r;
                let Ok((first_mb, _, pps_id)) = peek_pps_id(&mut r2) else {
                    continue;
                };
                let Some(pps) = ppss[pps_id as usize].as_deref() else {
                    f.errors.push("slice references missing pps".into());
                    continue;
                };
                let Some(sps) = spss[pps.sps_id as usize].as_deref() else {
                    continue;
                };
                if first_mb == 0 {
                    slices_in_pic = 0;
                }
                slices_in_pic += 1;
                f.max_slices_per_pic = f.max_slices_per_pic.max(slices_in_pic);
                if slices_in_pic > 1 {
                    f.multi_slice = true;
                }
                match parse_slice_header(&mut r, h, sps, pps) {
                    Ok(sh) => {
                        f.slice_types.insert(match sh.slice_type {
                            SliceType::I => "I",
                            SliceType::P => "P",
                            SliceType::B => "B",
                            SliceType::SP => "SP",
                            SliceType::SI => "SI",
                        });
                        f.field_pics |= sh.field_pic;
                        f.mbaff_pics |= sh.mbaff;
                        f.redundant_slices |= sh.redundant_pic_cnt > 0;
                        if sh.slice_type == SliceType::B {
                            if sh.direct_spatial_mv_pred {
                                f.direct_spatial = true;
                            } else {
                                f.direct_temporal = true;
                            }
                            if pps.weighted_bipred_idc == 2 {
                                f.weighted_implicit = true;
                            }
                        }
                        if sh.weights.is_some() {
                            f.weighted_explicit = true;
                        }
                        f.ref_list_mods |= !sh.ref_mods[0].is_empty() || !sh.ref_mods[1].is_empty();
                        for m in &sh.mmcos {
                            f.mmco_ops.insert(m.op);
                        }
                        f.long_term_idr |= sh.long_term_reference_flag;
                    }
                    Err(e) => f.errors.push(format!("slice: {e:?}")),
                }
            }
            _ => {}
        }
    }
    f
}
