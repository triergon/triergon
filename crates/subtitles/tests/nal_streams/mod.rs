//! Small H.264 and HEVC access units for extraction tests: parameter sets and
//! slice headers with only the fields picture output reads, and caption SEI.

#![allow(dead_code)]

/// RBSP bits, most significant first.
#[derive(Default)]
pub struct Bits(Vec<bool>);

impl Bits {
    pub fn u(&mut self, value: u64, width: u32) -> &mut Self {
        self.0
            .extend((0..width).rev().map(|bit| value >> bit & 1 == 1));
        self
    }

    pub fn flag(&mut self, set: bool) -> &mut Self {
        self.u(u64::from(set), 1)
    }

    pub fn ue(&mut self, value: u32) -> &mut Self {
        let code = u64::from(value) + 1;
        let width = 64 - code.leading_zeros();
        self.u(0, width - 1).u(code, width)
    }

    pub fn se(&mut self, value: i32) -> &mut Self {
        let code = if value > 0 {
            2 * value.unsigned_abs() - 1
        } else {
            2 * value.unsigned_abs()
        };
        self.ue(code)
    }

    /// The NAL unit: `header`, the bits with their stop bit, and emulation
    /// prevention bytes.
    pub fn nal(&mut self, header: &[u8]) -> Vec<u8> {
        self.flag(true);
        while !self.0.len().is_multiple_of(8) {
            self.flag(false);
        }
        let mut nal = header.to_vec();
        let mut zeros = 0;
        for byte in self.0.chunks(8) {
            let byte = byte.iter().fold(0u8, |b, &bit| b << 1 | u8::from(bit));
            if zeros >= 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            nal.push(byte);
        }
        nal
    }
}

/// An access unit of NAL units in start code framing.
pub fn annex_b(nals: &[Vec<u8>]) -> Vec<u8> {
    nals.iter()
        .flat_map(|nal| [&[0, 0, 1][..], nal].concat())
        .collect()
}

/// The caption bytes of picture `n`: one field 1 construct.
pub fn tag(n: u8) -> [u8; 3] {
    [0xfc, 0x80 | n, 0x80]
}

/// An SEI NAL unit (`header`) with a T.35 A/53 message carrying `tag(n)`.
pub fn caption_sei(header: &[u8], n: u8) -> Vec<u8> {
    let mut payload = b"\xb5\x00\x31GA94\x03\x41\xff".to_vec();
    payload.extend_from_slice(&tag(n));
    payload.push(0xff);
    let mut bits = Bits::default();
    bits.u(4, 8).u(payload.len() as u64, 8);
    for byte in payload {
        bits.u(u64::from(byte), 8);
    }
    bits.nal(header)
}

/// H.264 sequence parameters: order count type 0 with 8-bit LSBs, type 1
/// with `cycle` (and `offset_for_non_ref_pic`) or type 2, 4-bit `frame_num`,
/// frames or fields, VUI timing and an optional bitstream restriction
/// (`max_num_reorder_frames`).
#[derive(Clone)]
pub struct AvcSps {
    pub poc_type: u32,
    pub reorder: Option<u32>,
    pub max_ref_frames: u32,
    pub timing: Option<(u32, u32)>,
    pub frame_mbs_only: bool,
    pub offset_for_non_ref_pic: i32,
    pub cycle: Vec<i32>,
}

impl Default for AvcSps {
    fn default() -> Self {
        Self {
            poc_type: 0,
            reorder: Some(1),
            max_ref_frames: 2,
            timing: Some((1001, 60000)),
            frame_mbs_only: true,
            offset_for_non_ref_pic: 0,
            cycle: Vec::new(),
        }
    }
}

pub fn avc_sps(sps: &AvcSps) -> Vec<u8> {
    let mut b = Bits::default();
    b.u(66, 8).u(0, 8).u(30, 8).ue(0); // Baseline, level 3.0, id 0
    b.ue(0).ue(sps.poc_type); // log2_max_frame_num_minus4, pic_order_cnt_type
    if sps.poc_type == 0 {
        b.ue(4); // 8-bit LSBs
    }
    if sps.poc_type == 1 {
        b.flag(false).se(sps.offset_for_non_ref_pic).se(0);
        b.ue(sps.cycle.len() as u32);
        for &offset in &sps.cycle {
            b.se(offset);
        }
    }
    b.ue(sps.max_ref_frames).flag(false).ue(3).ue(2); // 64x48 frames
    b.flag(sps.frame_mbs_only);
    if !sps.frame_mbs_only {
        b.flag(false); // mb_adaptive_frame_field_flag
    }
    b.flag(true).flag(false).flag(true); // direct, no crop, VUI
    b.flag(false).flag(false).flag(false).flag(false); // aspect, overscan, signal, chroma loc
    match sps.timing {
        Some((num, scale)) => {
            b.flag(true)
                .u(u64::from(num), 32)
                .u(u64::from(scale), 32)
                .flag(true);
        }
        None => {
            b.flag(false);
        }
    }
    b.flag(false).flag(false).flag(false); // no HRD, pic_struct_present_flag
    match sps.reorder {
        Some(reorder) => {
            b.flag(true)
                .flag(true)
                .ue(0)
                .ue(0)
                .ue(16)
                .ue(16)
                .ue(reorder)
                .ue(4);
        }
        None => {
            b.flag(false);
        }
    }
    b.nal(&[0x67])
}

pub fn avc_pps() -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0).ue(0).flag(false).flag(false).ue(0).ue(0).ue(0); // ids, CAVLC, one group, lists
    b.flag(false).u(0, 2).se(0).se(0).se(0); // no weighted prediction, QPs
    b.flag(true).flag(false).flag(false);
    b.nal(&[0x68])
}

/// An H.264 picture's first slice.
#[derive(Clone, Copy, Default)]
pub struct AvcSlice {
    pub idr: bool,
    pub reference: bool,
    /// 0 P, 1 B, 2 I.
    pub slice_type: u32,
    pub frame_num: u32,
    pub poc_lsb: u32,
    /// `delta_pic_order_cnt[ 0 ]` (order count type 1).
    pub delta: i32,
    /// `bottom_field_flag` of a field picture.
    pub field: Option<bool>,
    pub no_output_of_prior_pics: bool,
    pub mmco5: bool,
}

pub fn avc_slice(s: &AvcSlice, poc_type: u32) -> Vec<u8> {
    avc_slice_of(s, poc_type, s.field.is_none())
}

/// A slice of a sequence whose `frame_mbs_only_flag` is `frames_only`.
pub fn avc_slice_of(s: &AvcSlice, poc_type: u32, frames_only: bool) -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0)
        .ue(s.slice_type + 5)
        .ue(0)
        .u(u64::from(s.frame_num), 4);
    if !frames_only {
        b.flag(s.field.is_some());
        if let Some(bottom) = s.field {
            b.flag(bottom);
        }
    }
    if s.idr {
        b.ue(0);
    }
    if poc_type == 0 {
        b.u(u64::from(s.poc_lsb), 8);
    }
    if poc_type == 1 {
        b.se(s.delta);
    }
    if s.slice_type == 1 {
        b.flag(true); // direct_spatial_mv_pred_flag
    }
    if s.slice_type != 2 {
        b.flag(false); // num_ref_idx_active_override_flag
        b.flag(false); // ref_pic_list_modification_flag_l0
        if s.slice_type == 1 {
            b.flag(false);
        }
    }
    if s.reference {
        if s.idr {
            b.flag(s.no_output_of_prior_pics).flag(false);
        } else if s.mmco5 {
            b.flag(true).ue(5).ue(0);
        } else {
            b.flag(false);
        }
    }
    b.se(0); // slice_qp_delta
    let header = match (s.idr, s.reference) {
        (true, _) => 0x65,
        (false, true) => 0x41,
        (false, false) => 0x01,
    };
    b.nal(&[header])
}

/// HEVC sequence parameters: 8-bit order count LSBs, one sub-layer with
/// `sps_max_dec_pic_buffering_minus1`, `sps_max_num_reorder_pics` and
/// `sps_max_latency_increase_plus1`, no SPS reference picture sets, VUI timing
/// 1001/30000.
pub fn hevc_sps(dpb_minus1: u32, reorder: u32, latency_plus1: u32) -> Vec<u8> {
    hevc_sps_with(dpb_minus1, reorder, latency_plus1, &[], &[])
}

/// How a short-term reference picture set is coded.
#[derive(Clone)]
pub enum StSet {
    /// Its delta order counts.
    Explicit(Vec<i32>),
    /// Predicted from the set before it (in an SPS) or from the SPS's last
    /// set (in a slice header): `delta_rps` and, for each delta of that set
    /// and then `delta_rps`, whether it is used.
    Predicted { delta_rps: i32, used: Vec<bool> },
}

fn write_st_set(b: &mut Bits, set: &StSet, first: bool) {
    match set {
        StSet::Explicit(deltas) => {
            if !first {
                b.flag(false);
            }
            let negative: Vec<_> = deltas.iter().filter(|&&d| d < 0).copied().collect();
            let positive: Vec<_> = deltas.iter().filter(|&&d| d > 0).copied().collect();
            b.ue(negative.len() as u32).ue(positive.len() as u32);
            let mut last = 0;
            let mut sorted = negative;
            sorted.sort_unstable_by(|a, b| b.cmp(a));
            for d in sorted {
                b.ue((last - d - 1) as u32).flag(true);
                last = d;
            }
            last = 0;
            let mut sorted = positive;
            sorted.sort_unstable();
            for d in sorted {
                b.ue((d - last - 1) as u32).flag(true);
                last = d;
            }
        }
        StSet::Predicted { delta_rps, used } => {
            b.flag(true);
            b.flag(*delta_rps < 0).ue(delta_rps.unsigned_abs() - 1);
            for &u in used {
                b.flag(u);
                if !u {
                    b.flag(false); // use_delta_flag
                }
            }
        }
    }
}

/// [`hevc_sps`] with short-term reference picture sets and long-term order
/// count LSBs of its own.
pub fn hevc_sps_with(
    dpb_minus1: u32,
    reorder: u32,
    latency_plus1: u32,
    sets: &[StSet],
    lt_lsbs: &[u32],
) -> Vec<u8> {
    let mut b = Bits::default();
    b.u(0, 4).u(0, 3).flag(true); // VPS, one sub-layer, nesting
    b.u(0x0160_0000, 32).u(0, 32).u(0, 24).u(93, 8); // profile_tier_level
    b.ue(0).ue(1).ue(64).ue(48).flag(false); // id, 4:2:0, size, no window
    b.ue(0).ue(0).ue(4); // bit depths, 8-bit LSBs
    b.flag(true).ue(dpb_minus1).ue(reorder).ue(latency_plus1);
    for _ in 0..6 {
        b.ue(0); // block sizes and transform depths
    }
    b.flag(false).u(0, 2).flag(false); // no scaling lists, AMP, SAO, PCM
    b.ue(sets.len() as u32);
    for (k, set) in sets.iter().enumerate() {
        write_st_set(&mut b, set, k == 0);
    }
    b.flag(!lt_lsbs.is_empty());
    if !lt_lsbs.is_empty() {
        b.ue(lt_lsbs.len() as u32);
        for &lsb in lt_lsbs {
            b.u(u64::from(lsb), 8).flag(true);
        }
    }
    b.u(0, 2).flag(true); // TMVP, strong intra smoothing; VUI
    b.u(0, 4).u(0, 3).flag(false); // VUI flags up to the display window
    b.flag(true).u(1001, 32).u(30000, 32).flag(false);
    b.flag(false).flag(false); // no bitstream restriction; no extensions
    b.nal(&[0x42, 0x01])
}

pub fn hevc_pps() -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0).ue(0).flag(false).flag(false).u(0, 3);
    b.nal(&[0x44, 0x01])
}

/// An HEVC picture's first slice segment: `nal_type`, order count LSBs and
/// its own short-term reference picture set of delta order counts.
pub fn hevc_slice(
    nal_type: u8,
    poc_lsb: u32,
    deltas: &[i32],
    no_output_of_prior_pics: bool,
) -> Vec<u8> {
    let mut b = Bits::default();
    b.flag(true);
    if (16..=23).contains(&nal_type) {
        b.flag(no_output_of_prior_pics);
    }
    b.ue(0).ue(1); // PPS 0, P slice
    if !matches!(nal_type, 19 | 20) {
        b.u(u64::from(poc_lsb), 8).flag(false); // a set of its own
        let negative: Vec<_> = deltas.iter().filter(|&&d| d < 0).copied().collect();
        let positive: Vec<_> = deltas.iter().filter(|&&d| d > 0).copied().collect();
        b.ue(negative.len() as u32).ue(positive.len() as u32);
        let mut last = 0;
        let mut sorted = negative;
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        for d in sorted {
            b.ue((last - d - 1) as u32).flag(true);
            last = d;
        }
        last = 0;
        let mut sorted = positive;
        sorted.sort_unstable();
        for d in sorted {
            b.ue((d - last - 1) as u32).flag(true);
            last = d;
        }
    }
    b.nal(&[nal_type << 1, 0x01])
}

/// The reference picture set of a slice: a set of its own or an SPS set's
/// index (of `sets` SPS sets), and long-term pictures by their LSBs (with the
/// SPS's long-term LSBs, by index), optionally with their MSB cycle.
pub enum SliceRps {
    Own(StSet),
    Index { index: u32, sets: u32 },
}

/// A long-term picture of a slice header.
pub enum LongTerm {
    /// The SPS's long-term LSBs of `index`.
    Sps { index: u32 },
    /// LSBs and, when given, `delta_poc_msb_cycle_lt`.
    Own { lsb: u32, msb_cycle: Option<u32> },
}

/// An HEVC first slice segment with a reference picture set in any coding
/// (a set of its own following SPS sets) and, for an SPS with long-term
/// pictures, its `num_long_term_ref_pics_sps` and the slice's long-term
/// pictures (those from the SPS first).
pub fn hevc_slice_with(
    nal_type: u8,
    poc_lsb: u32,
    rps: &SliceRps,
    long_term: Option<(u32, &[LongTerm])>,
) -> Vec<u8> {
    let mut b = Bits::default();
    b.flag(true);
    if (16..=23).contains(&nal_type) {
        b.flag(false);
    }
    b.ue(0).ue(1);
    if !matches!(nal_type, 19 | 20) {
        b.u(u64::from(poc_lsb), 8);
        match rps {
            SliceRps::Own(set) => {
                b.flag(false);
                if let StSet::Predicted { .. } = set {
                    // inter_ref_pic_set_prediction_flag, then delta_idx_minus1.
                    let mut inner = Bits::default();
                    write_st_set(&mut inner, set, false);
                    b.0.push(inner.0[0]);
                    b.ue(0);
                    b.0.extend_from_slice(&inner.0[1..]);
                } else {
                    write_st_set(&mut b, set, false);
                }
            }
            SliceRps::Index { index, sets } => {
                b.flag(true);
                if *sets > 1 {
                    b.u(u64::from(*index), 32 - (sets - 1).leading_zeros());
                }
            }
        }
        if let Some((sps_count, entries)) = long_term {
            let from_sps = entries
                .iter()
                .filter(|e| matches!(e, LongTerm::Sps { .. }))
                .count() as u32;
            if sps_count > 0 {
                b.ue(from_sps);
            }
            b.ue(entries.len() as u32 - from_sps);
            for entry in entries {
                match entry {
                    LongTerm::Sps { index } => {
                        if sps_count > 1 {
                            b.u(u64::from(*index), 32 - (sps_count - 1).leading_zeros());
                        }
                        b.flag(false);
                    }
                    LongTerm::Own { lsb, msb_cycle } => {
                        b.u(u64::from(*lsb), 8).flag(true).flag(msb_cycle.is_some());
                        if let Some(cycle) = msb_cycle {
                            b.ue(*cycle);
                        }
                    }
                }
            }
        }
    }
    b.nal(&[nal_type << 1, 0x01])
}

/// An access unit with its NAL units prefixed by 4-byte lengths.
pub fn length_prefixed(nals: &[Vec<u8>]) -> Vec<u8> {
    nals.iter()
        .flat_map(|nal| [&(nal.len() as u32).to_be_bytes()[..], nal].concat())
        .collect()
}

/// An HEVC access unit: parameter sets when `params`, a caption SEI with
/// `tag(n)`, then the slice.
pub fn hevc_au(params: bool, n: u8, slice: Vec<u8>) -> Vec<u8> {
    let mut nals = Vec::new();
    if params {
        nals.push(hevc_sps(4, 1, 0));
        nals.push(hevc_pps());
    }
    nals.push(caption_sei(&[0x4e, 0x01], n));
    nals.push(slice);
    annex_b(&nals)
}
