//! Self-authored HEVC streams built bit by bit from the Rec. ITU-T H.265 syntax tables:
//! SPS, PPS, IDR and P slices whose slice data is filler.
#![allow(dead_code)] // Each test file uses its own subset.

#[derive(Default)]
pub struct Bits {
    bytes: Vec<u8>,
    bit: u32,
}

impl Bits {
    pub fn put(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bit == 0 {
                self.bytes.push(0);
            }
            if (value >> i) & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> self.bit;
            }
            self.bit = (self.bit + 1) % 8;
        }
    }
    pub fn flag(&mut self, value: bool) {
        self.put(u32::from(value), 1);
    }
    pub fn ue(&mut self, value: u32) {
        let code = value + 1;
        let len = 32 - code.leading_zeros();
        self.put(0, len - 1);
        self.put(code, len);
    }
    /// rbsp_trailing_bits() or byte_alignment(): a one, then zeros to the byte boundary.
    pub fn align_one(mut self) -> Vec<u8> {
        self.flag(true);
        while self.bit != 0 {
            self.flag(false);
        }
        self.bytes
    }
}

/// One NAL unit with start code, header and emulation prevention.
pub fn nal(stream: &mut Vec<u8>, nal_unit_type: u8, rbsp: &[u8]) {
    stream.extend_from_slice(&[0, 0, 0, 1, nal_unit_type << 1, 1]);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros == 2 && b <= 3 {
            stream.push(3);
            zeros = 0;
        }
        stream.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

/// Main (8-bit) or Main 10 sequence of `width` x `height` with 16x16 CTBs and a DPB of
/// `dpb` pictures, no reordering.
pub fn sps(width: u32, height: u32, bit_depth: u32, dpb: u32) -> Vec<u8> {
    let profile = if bit_depth == 8 { 1 } else { 2 };
    let mut b = Bits::default();
    b.put(0, 4); // sps_video_parameter_set_id
    b.put(0, 3); // sps_max_sub_layers_minus1
    b.flag(true); // sps_temporal_id_nesting_flag
    b.put(0, 2); // general_profile_space
    b.flag(false); // general_tier_flag
    b.put(profile, 5); // general_profile_idc: Main or Main 10
    b.put(0x8000_0000 >> profile, 32); // general_profile_compatibility_flag[profile]
    b.put(0, 24); // progressive .. general_reserved_zero_43bits, general_inbld_flag
    b.put(0, 24);
    b.put(186, 8); // general_level_idc: level 6.2
    b.ue(0); // sps_seq_parameter_set_id
    b.ue(1); // chroma_format_idc: 4:2:0
    b.ue(width); // pic_width_in_luma_samples
    b.ue(height); // pic_height_in_luma_samples
    b.flag(false); // conformance_window_flag
    b.ue(bit_depth - 8); // bit_depth_luma_minus8
    b.ue(bit_depth - 8); // bit_depth_chroma_minus8
    b.ue(4); // log2_max_pic_order_cnt_lsb_minus4
    b.flag(true); // sps_sub_layer_ordering_info_present_flag
    b.ue(dpb - 1); // sps_max_dec_pic_buffering_minus1
    b.ue(0); // sps_max_num_reorder_pics
    b.ue(0); // sps_max_latency_increase_plus1
    b.ue(0); // log2_min_luma_coding_block_size_minus3
    b.ue(1); // log2_diff_max_min_luma_coding_block_size: 16x16 CTBs
    b.ue(0); // log2_min_luma_transform_block_size_minus2
    b.ue(1); // log2_diff_max_min_luma_transform_block_size
    b.ue(0); // max_transform_hierarchy_depth_inter
    b.ue(0); // max_transform_hierarchy_depth_intra
    for _ in 0..4 {
        // scaling_list_enabled, amp_enabled, sample_adaptive_offset_enabled, pcm_enabled
        b.flag(false);
    }
    b.ue(0); // num_short_term_ref_pic_sets
    for _ in 0..5 {
        // long_term_ref_pics_present, sps_temporal_mvp_enabled,
        // strong_intra_smoothing_enabled, vui_parameters_present, sps_extension_present
        b.flag(false);
    }
    b.align_one()
}

pub fn pps() -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0); // pps_pic_parameter_set_id
    b.ue(0); // pps_seq_parameter_set_id
    b.flag(false); // dependent_slice_segments_enabled_flag
    b.flag(false); // output_flag_present_flag
    b.put(0, 3); // num_extra_slice_header_bits
    b.flag(false); // sign_data_hiding_enabled_flag
    b.flag(false); // cabac_init_present_flag
    b.ue(0); // num_ref_idx_l0_default_active_minus1
    b.ue(0); // num_ref_idx_l1_default_active_minus1
    b.ue(0); // init_qp_minus26 (se(v) 0)
    b.flag(false); // constrained_intra_pred_flag
    b.flag(false); // transform_skip_enabled_flag
    b.flag(false); // cu_qp_delta_enabled_flag
    b.ue(0); // pps_cb_qp_offset
    b.ue(0); // pps_cr_qp_offset
    for _ in 0..10 {
        // pps_slice_chroma_qp_offsets_present, weighted_pred, weighted_bipred,
        // transquant_bypass_enabled, tiles_enabled, entropy_coding_sync_enabled,
        // pps_loop_filter_across_slices_enabled, deblocking_filter_control_present,
        // pps_scaling_list_data_present, lists_modification_present
        b.flag(false);
    }
    b.ue(0); // log2_parallel_merge_level_minus2
    b.flag(false); // slice_segment_header_extension_present_flag
    b.flag(false); // pps_extension_present_flag
    b.align_one()
}

pub fn idr_slice() -> Vec<u8> {
    let mut b = Bits::default();
    b.flag(true); // first_slice_segment_in_pic_flag
    b.flag(false); // no_output_of_prior_pics_flag
    b.ue(0); // slice_pic_parameter_set_id
    b.ue(2); // slice_type: I
    b.ue(0); // slice_qp_delta (se(v) 0)
    with_filler(b)
}

/// A P slice of picture order count `poc` referencing the `refs` pictures before it.
pub fn p_slice(poc: u32, refs: u32) -> Vec<u8> {
    let mut b = Bits::default();
    b.flag(true); // first_slice_segment_in_pic_flag
    b.ue(0); // slice_pic_parameter_set_id
    b.ue(1); // slice_type: P
    b.put(poc, 8); // slice_pic_order_cnt_lsb
    b.flag(false); // short_term_ref_pic_set_sps_flag
    b.ue(refs); // num_negative_pics
    b.ue(0); // num_positive_pics
    for _ in 0..refs {
        b.ue(0); // delta_poc_s0_minus1
        b.flag(true); // used_by_curr_pic_s0_flag
    }
    b.flag(false); // num_ref_idx_active_override_flag
    b.ue(0); // five_minus_max_num_merge_cand
    b.ue(0); // slice_qp_delta (se(v) 0)
    with_filler(b)
}

pub fn with_filler(b: Bits) -> Vec<u8> {
    let mut rbsp = b.align_one();
    rbsp.extend_from_slice(&[0x5a; 32]); // filler slice data
    rbsp
}

pub fn idr_access_unit(width: u32, height: u32, bit_depth: u32, dpb: u32) -> Vec<u8> {
    let mut au = Vec::new();
    nal(&mut au, 33, &sps(width, height, bit_depth, dpb));
    nal(&mut au, 34, &pps());
    nal(&mut au, 19, &idr_slice());
    au
}

pub fn access_unit() -> Vec<u8> {
    idr_access_unit(64, 64, 8, 2)
}

/// A non-IDR slice segment of picture order count `poc` at CTB `address` of a picture
/// with 16 CTBs: an I slice, or a P slice when `refs` pictures before it are referenced.
pub fn slice_segment(poc: u32, address: u32, refs: u32) -> Vec<u8> {
    let mut b = Bits::default();
    b.flag(address == 0); // first_slice_segment_in_pic_flag
    b.ue(0); // slice_pic_parameter_set_id
    if address != 0 {
        b.put(address, 4); // slice_segment_address, Ceil(Log2(16)) bits
    }
    b.ue(if refs == 0 { 2 } else { 1 }); // slice_type: I or P
    b.put(poc, 8); // slice_pic_order_cnt_lsb
    b.flag(false); // short_term_ref_pic_set_sps_flag
    b.ue(refs); // num_negative_pics
    b.ue(0); // num_positive_pics
    for _ in 0..refs {
        b.ue(0); // delta_poc_s0_minus1
        b.flag(true); // used_by_curr_pic_s0_flag
    }
    if refs != 0 {
        b.flag(false); // num_ref_idx_active_override_flag
        b.ue(0); // five_minus_max_num_merge_cand
    }
    b.ue(0); // slice_qp_delta (se(v) 0)
    with_filler(b)
}

/// One NAL unit without its start code, as `Decoder::send_nal` takes it.
pub fn bare_nal(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut stream = Vec::new();
    nal(&mut stream, nal_unit_type, rbsp);
    stream.split_off(4)
}
