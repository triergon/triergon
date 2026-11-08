//! Stream feature inspection (parameter sets and slice headers only). Used to classify
//! conformance vectors into the selected feature matrix; not part of the decoding path.

use crate::nal::{NalIter, parse_header, unescape};
use crate::ps::{Pps, Sps, parse_pps, parse_sps};
use crate::slice::{SliceType, parse_slice_header, peek_pps_id};
use std::collections::BTreeSet;
use triergon_bitstream::BitReader;
use triergon_codec::{Budget, BudgetVec};

#[derive(Debug, Default, Clone)]
pub struct StreamFeatures {
    pub profiles: BTreeSet<u8>,
    pub constraint_flags: BTreeSet<u8>,
    pub chroma_format_idc: BTreeSet<u32>,
    pub bit_depth: BTreeSet<u32>,
    pub sizes: BTreeSet<(u32, u32)>,
    pub cabac: BTreeSet<bool>,
    pub frame_mbs_only: BTreeSet<bool>,
    pub mbaff_sps: bool,
    pub field_pics: bool,
    pub mbaff_pics: bool,
    pub slice_types: BTreeSet<&'static str>,
    pub fmo: bool,
    pub redundant_pic_cnt_present: bool,
    pub redundant_slices: bool,
    pub weighted_explicit: bool,
    pub weighted_implicit: bool,
    pub transform_8x8: bool,
    pub scaling_matrices: bool,
    pub poc_types: BTreeSet<u32>,
    pub constrained_intra_pred: bool,
    pub direct_spatial: bool,
    pub direct_temporal: bool,
    pub ref_list_mods: bool,
    pub mmco_ops: BTreeSet<u32>,
    pub long_term_idr: bool,
    pub multi_slice: bool,
    pub nal_types: BTreeSet<u8>,
    pub gaps_allowed: bool,
    pub lossless: bool,
    pub crop: bool,
    pub max_slices_per_pic: u32,
    pub bitstream_restriction: Option<(u32, u32)>,
    pub errors: Vec<String>,
}

pub fn inspect(data: &[u8]) -> StreamFeatures {
    let mut f = StreamFeatures::default();
    let mut spss: Vec<Option<Box<Sps>>> = vec![None; 32];
    let mut ppss: Vec<Option<Box<Pps>>> = vec![None; 256];
    let Ok(pool) = Budget::new(crate::Limits::default().live_storage_bytes) else {
        return f;
    };
    let mut buf = BudgetVec::new(&pool);
    let mut slices_in_pic = 0u32;
    for nal in NalIter::new(data) {
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
