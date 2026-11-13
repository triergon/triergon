//! The H.264 parameter set and slice header fields that decide picture output
//! (7.3.2.1.1, 7.3.2.2, 7.3.3, E.1.1): order counts, reference marking with
//! `memory_management_control_operation` 5, the VUI frame period and bitstream
//! restriction. The rest of each RBSP is passed over.

use super::nal::{LastPayload, Timing};
use triergon_bitstream::{BitError, BitSource, Rbsp};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::{Codec as Coding, skip_avc_scaling_matrix, skip_vui_to_timing};

/// How a sequence codes picture order counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PocType {
    Lsb {
        log2_max_lsb: u32,
    },
    /// The cycle's offsets are `cycle_len` values from `cycle_start` in the
    /// table's offset pool.
    Cycle {
        always_zero: bool,
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        cycle_start: usize,
        cycle_len: usize,
    },
    FrameNum,
}

/// The fields of a sequence parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sps {
    pub level_idc: u8,
    pub chroma_array_type: u32,
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub poc: PocType,
    pub max_num_ref_frames: u32,
    pub frame_mbs_only: bool,
    /// PicWidthInMbs * FrameHeightInMbs.
    pub frame_mbs: u32,
    pub timing: Option<Timing>,
    /// The VUI bitstream restriction's `max_num_reorder_frames`.
    pub max_num_reorder_frames: Option<u32>,
}

/// The fields of a picture parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pps {
    pub sps_id: u8,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub redundant_pic_cnt_present: bool,
}

/// The fields of a picture's first slice header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slice {
    pub idr: bool,
    pub reference: bool,
    /// An I or SI slice.
    pub intra: bool,
    pub sps_id: u8,
    pub frame_num: u32,
    /// `bottom_field_flag` of a field picture.
    pub field: Option<bool>,
    pub poc_lsb: u32,
    pub delta_bottom: i32,
    pub delta: [i32; 2],
    pub mmco5: bool,
    pub no_output_of_prior_pics: bool,
}

/// The parameter sets received, by id.
#[derive(Debug)]
pub(crate) struct Tables {
    sps: BudgetVec<Option<Sps>>,
    pps: BudgetVec<Option<Pps>>,
    last_sps: LastPayload,
    last_pps: LastPayload,
    /// The offsets of type 1 order count cycles.
    offsets: BudgetVec<i32>,
}

const SPS_IDS: usize = 32;
const PPS_IDS: usize = 256;

type Bits<'a> = Rbsp<'a>;

impl Tables {
    pub(crate) fn new(pool: &Budget) -> Self {
        Self {
            sps: BudgetVec::new(pool),
            pps: BudgetVec::new(pool),
            last_sps: LastPayload::new(pool),
            last_pps: LastPayload::new(pool),
            offsets: BudgetVec::new(pool),
        }
    }

    pub(crate) fn sps(&self, id: u8) -> Option<&Sps> {
        self.sps.get(usize::from(id))?.as_ref()
    }

    /// The offsets of a type 1 cycle.
    pub(crate) fn cycle(&self, start: usize, len: usize) -> &[i32] {
        self.offsets.get(start..start + len).unwrap_or_default()
    }

    /// The tables as they are, to return to: their lengths. Parameter sets
    /// change in place, so the caller keeps copies of those it replaces.
    pub(crate) fn offsets_len(&self) -> usize {
        self.offsets.len()
    }

    pub(crate) fn truncate_offsets(&mut self, len: usize) {
        self.offsets.truncate(len);
    }

    /// Reads an SPS NAL unit payload (after the header) and stores it,
    /// returning its id and the set it replaces; a repeated set changes
    /// nothing. An SPS cut short or out of range is ignored.
    pub(crate) fn put_sps(&mut self, payload: &[u8]) -> Result<Option<(u8, Option<Sps>)>> {
        if self.sps.is_empty() {
            self.sps.try_resize(SPS_IDS, None)?;
        }
        if self.last_sps.repeats(payload) {
            return Ok(None);
        }
        self.last_sps.remember(payload)?;
        let mark = self.offsets.len();
        let mut bits = Rbsp::lenient(payload);
        match read_sps(&mut bits, &mut self.offsets) {
            Ok(Some((id, sps))) if usize::from(id) < SPS_IDS => {
                let slot = usize::from(id);
                if let Some(old) = self.sps[slot]
                    && self.same(&old, &sps)
                {
                    self.offsets.truncate(mark);
                    return Ok(None);
                }
                Ok(Some((id, self.sps[slot].replace(sps))))
            }
            Err(Failure::Memory(error)) => {
                self.offsets.truncate(mark);
                Err(error)
            }
            _ => {
                self.offsets.truncate(mark);
                Ok(None)
            }
        }
    }

    /// Whether two sets read alike, their cycles compared by value.
    fn same(&self, a: &Sps, b: &Sps) -> bool {
        match (a.poc, b.poc) {
            (
                PocType::Cycle {
                    cycle_start: x,
                    cycle_len: m,
                    ..
                },
                PocType::Cycle {
                    cycle_start: y,
                    cycle_len: n,
                    ..
                },
            ) => {
                let unpooled = |sps: &Sps| Sps {
                    poc: match sps.poc {
                        PocType::Cycle {
                            always_zero,
                            offset_for_non_ref_pic,
                            offset_for_top_to_bottom_field,
                            ..
                        } => PocType::Cycle {
                            always_zero,
                            offset_for_non_ref_pic,
                            offset_for_top_to_bottom_field,
                            cycle_start: 0,
                            cycle_len: 0,
                        },
                        other => other,
                    },
                    ..*sps
                };
                unpooled(a) == unpooled(b) && self.cycle(x, m) == self.cycle(y, n)
            }
            _ => a == b,
        }
    }

    /// Reads a PPS NAL unit payload and stores it, returning its id and the
    /// set it replaces.
    pub(crate) fn put_pps(&mut self, payload: &[u8]) -> Result<Option<(u8, Option<Pps>)>> {
        if self.pps.is_empty() {
            self.pps.try_resize(PPS_IDS, None)?;
        }
        if self.last_pps.repeats(payload) {
            return Ok(None);
        }
        self.last_pps.remember(payload)?;
        let mut bits = Rbsp::lenient(payload);
        Ok(read_pps(&mut bits).ok().and_then(|(id, pps)| {
            let slot = self.pps.get_mut(id as usize)?;
            Some((id as u8, slot.replace(pps)))
        }))
    }

    /// Forgets the last sets read, after a failed step puts sets back.
    pub(crate) fn forget_repeats(&mut self) {
        self.last_sps.forget();
        self.last_pps.forget();
    }

    /// Puts back a set a failed step replaced.
    pub(crate) fn restore_sps(&mut self, id: u8, old: Option<Sps>) {
        if let Some(slot) = self.sps.get_mut(usize::from(id)) {
            *slot = old;
        }
    }

    pub(crate) fn restore_pps(&mut self, id: u8, old: Option<Pps>) {
        if let Some(slot) = self.pps.get_mut(usize::from(id)) {
            *slot = old;
        }
    }

    /// The fields of a slice NAL unit (`nal_unit_type` 1 or 5), `None` when
    /// its parameter sets are missing or it is cut short.
    pub(crate) fn slice(&self, nal: &[u8]) -> Option<Slice> {
        let &header = nal.first()?;
        let mut bits = Rbsp::lenient(nal.get(1..)?);
        read_slice(&mut bits, header, self).ok()
    }

    fn pps(&self, id: u32) -> Option<&Pps> {
        self.pps.get(id as usize)?.as_ref()
    }
}

/// Why a read stopped.
enum Failure {
    /// Cut short or out of range: the NAL unit is not taken.
    Invalid,
    /// The pool refused memory.
    Memory(triergon_core::Error),
}

impl From<BitError> for Failure {
    fn from(_: BitError) -> Self {
        Self::Invalid
    }
}

fn read_sps(
    bits: &mut Bits<'_>,
    offsets: &mut BudgetVec<i32>,
) -> std::result::Result<Option<(u8, Sps)>, Failure> {
    let profile = bits.read(8)?;
    bits.read(8)?; // constraint flags
    let level_idc = bits.read(8)? as u8;
    let id = bits.ue()?;
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = bits.flag()?;
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.flag()?; // qpprime_y_zero_transform_bypass_flag
        skip_avc_scaling_matrix(bits, chroma_format_idc).map_err(|_| Failure::Invalid)?;
    }
    let log2_max_frame_num = bits.ue()?.saturating_add(4);
    if log2_max_frame_num > 16 {
        return Err(Failure::Invalid);
    }
    let poc = match bits.ue()? {
        0 => {
            let log2_max_lsb = bits.ue()?.saturating_add(4);
            if log2_max_lsb > 16 {
                return Err(Failure::Invalid);
            }
            PocType::Lsb { log2_max_lsb }
        }
        1 => {
            let always_zero = bits.flag()?;
            let offset_for_non_ref_pic = bits.se()?;
            let offset_for_top_to_bottom_field = bits.se()?;
            let cycle_len = bits.ue()? as usize;
            if cycle_len > 255 {
                return Err(Failure::Invalid);
            }
            let cycle_start = offsets.len();
            offsets.try_reserve(cycle_len).map_err(Failure::Memory)?;
            for _ in 0..cycle_len {
                let offset = bits.se()?;
                offsets
                    .try_push(offset)
                    .map_err(|rejected| Failure::Memory(rejected.error))?;
            }
            PocType::Cycle {
                always_zero,
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                cycle_start,
                cycle_len,
            }
        }
        2 => PocType::FrameNum,
        _ => return Err(Failure::Invalid),
    };
    let max_num_ref_frames = bits.ue()?;
    bits.flag()?; // gaps_in_frame_num_value_allowed_flag
    let width = bits.ue()?.saturating_add(1);
    let height_units = bits.ue()?.saturating_add(1);
    let frame_mbs_only = bits.flag()?;
    if !frame_mbs_only {
        bits.flag()?; // mb_adaptive_frame_field_flag
    }
    bits.flag()?; // direct_8x8_inference_flag
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?; // frame cropping offsets
        }
    }
    let frame_height = height_units.saturating_mul(2 - u32::from(frame_mbs_only));
    let mut sps = Sps {
        level_idc,
        chroma_array_type: if separate_colour_plane {
            0
        } else {
            chroma_format_idc
        },
        separate_colour_plane,
        log2_max_frame_num,
        poc,
        max_num_ref_frames,
        frame_mbs_only,
        frame_mbs: width.saturating_mul(frame_height),
        timing: None,
        max_num_reorder_frames: None,
    };
    if bits.flag()? {
        read_vui(bits, &mut sps)?;
    }
    if id >= 32 {
        return Err(Failure::Invalid);
    }
    Ok(Some((id as u8, sps)))
}

/// The VUI's timing and bitstream restriction (E.1.1). A VUI cut short keeps
/// what was read before.
fn read_vui(bits: &mut Bits<'_>, sps: &mut Sps) -> std::result::Result<(), Failure> {
    let Ok(timing_present) = skip_vui_to_timing(bits, Coding::Avc) else {
        return Ok(());
    };
    if timing_present {
        let num_units_in_tick = bits.read(32)?;
        let time_scale = bits.read(32)?;
        bits.flag()?; // fixed_frame_rate_flag
        sps.timing = (num_units_in_tick > 0 && time_scale > 0).then_some(Timing {
            num_units_in_tick,
            time_scale,
        });
    }
    let nal_hrd = bits.flag()?;
    if nal_hrd {
        skip_hrd(bits)?;
    }
    let vcl_hrd = bits.flag()?;
    if vcl_hrd {
        skip_hrd(bits)?;
    }
    if nal_hrd || vcl_hrd {
        bits.flag()?; // low_delay_hrd_flag
    }
    bits.flag()?; // pic_struct_present_flag
    if bits.flag()? {
        bits.flag()?; // motion_vectors_over_pic_boundaries_flag
        for _ in 0..4 {
            bits.ue()?; // max_bytes_per_pic_denom .. log2_max_mv_length_vertical
        }
        sps.max_num_reorder_frames = Some(bits.ue()?);
        bits.ue()?; // max_dec_frame_buffering
    }
    Ok(())
}

/// hrd_parameters( ) (E.1.2).
fn skip_hrd(bits: &mut Bits<'_>) -> std::result::Result<(), BitError> {
    let count = bits.ue()?.saturating_add(1);
    if count > 32 {
        return Err(BitError::LongPrefix);
    }
    bits.read(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        bits.ue()?;
        bits.ue()?;
        bits.flag()?;
    }
    bits.read(20)?; // the four delay and length fields
    Ok(())
}

fn read_pps(bits: &mut Bits<'_>) -> std::result::Result<(u32, Pps), BitError> {
    let id = bits.ue()?;
    let sps_id = bits.ue()?;
    if id >= 256 || sps_id >= 32 {
        return Err(BitError::LongPrefix);
    }
    bits.flag()?; // entropy_coding_mode_flag
    let bottom_field_pic_order_in_frame_present = bits.flag()?;
    let groups = bits.ue()?.saturating_add(1);
    if groups > 1 {
        skip_slice_groups(bits, groups)?;
    }
    let l0 = bits.ue()?.saturating_add(1);
    let l1 = bits.ue()?.saturating_add(1);
    let weighted_pred = bits.flag()?;
    let weighted_bipred_idc = bits.read(2)?;
    bits.se()?; // pic_init_qp_minus26
    bits.se()?; // pic_init_qs_minus26
    bits.se()?; // chroma_qp_index_offset
    bits.flag()?; // deblocking_filter_control_present_flag
    bits.flag()?; // constrained_intra_pred_flag
    let redundant_pic_cnt_present = bits.flag()?;
    Ok((
        id,
        Pps {
            sps_id: sps_id as u8,
            bottom_field_pic_order_in_frame_present,
            num_ref_idx_default: [l0, l1],
            weighted_pred,
            weighted_bipred_idc,
            redundant_pic_cnt_present,
        },
    ))
}

/// The slice group map syntax of a PPS (7.3.2.2).
fn skip_slice_groups(bits: &mut Bits<'_>, groups: u32) -> std::result::Result<(), BitError> {
    if groups > 8 {
        return Err(BitError::LongPrefix);
    }
    match bits.ue()? {
        0 => {
            for _ in 0..groups {
                bits.ue()?; // run_length_minus1
            }
        }
        2 => {
            for _ in 1..groups {
                bits.ue()?; // top_left
                bits.ue()?; // bottom_right
            }
        }
        3..=5 => {
            bits.flag()?; // slice_group_change_direction_flag
            bits.ue()?; // slice_group_change_rate_minus1
        }
        6 => {
            let units = bits.ue()?.saturating_add(1);
            let width = 32 - (groups - 1).leading_zeros();
            for _ in 0..units {
                bits.read(width)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_slice(
    bits: &mut Bits<'_>,
    header: u8,
    tables: &Tables,
) -> std::result::Result<Slice, BitError> {
    let missing = BitError::LongPrefix;
    let nal_type = header & 0x1f;
    let reference = header & 0x60 != 0;
    let idr = nal_type == 5;
    bits.ue()?; // first_mb_in_slice
    let slice_type = bits.ue()? % 5;
    let pps = *tables.pps(bits.ue()?).ok_or(missing)?;
    let sps = *tables.sps(pps.sps_id).ok_or(missing)?;
    if sps.separate_colour_plane {
        bits.read(2)?; // colour_plane_id
    }
    let frame_num = bits.read(sps.log2_max_frame_num)?;
    let mut field = None;
    if !sps.frame_mbs_only && bits.flag()? {
        field = Some(bits.flag()?);
    }
    if idr {
        bits.ue()?; // idr_pic_id
    }
    let mut slice = Slice {
        idr,
        reference,
        intra: matches!(slice_type, 2 | 4),
        sps_id: pps.sps_id,
        frame_num,
        field,
        poc_lsb: 0,
        delta_bottom: 0,
        delta: [0; 2],
        mmco5: false,
        no_output_of_prior_pics: false,
    };
    match sps.poc {
        PocType::Lsb { log2_max_lsb } => {
            slice.poc_lsb = bits.read(log2_max_lsb)?;
            if pps.bottom_field_pic_order_in_frame_present && field.is_none() {
                slice.delta_bottom = bits.se()?;
            }
        }
        PocType::Cycle { always_zero, .. } if !always_zero => {
            slice.delta[0] = bits.se()?;
            if pps.bottom_field_pic_order_in_frame_present && field.is_none() {
                slice.delta[1] = bits.se()?;
            }
        }
        _ => {}
    }
    if pps.redundant_pic_cnt_present {
        bits.ue()?; // redundant_pic_cnt
    }
    let (p, b) = (matches!(slice_type, 0 | 3), slice_type == 1);
    if b {
        bits.flag()?; // direct_spatial_mv_pred_flag
    }
    let [mut l0, mut l1] = pps.num_ref_idx_default;
    if (p || b) && bits.flag()? {
        l0 = bits.ue()?.saturating_add(1);
        if b {
            l1 = bits.ue()?.saturating_add(1);
        }
    }
    if l0 > 32 || l1 > 32 {
        return Err(missing);
    }
    // ref_pic_list_modification( ): one list for P and SP, two for B.
    for _ in 0..u32::from(p || b) + u32::from(b) {
        if bits.flag()? {
            loop {
                match bits.ue()? {
                    3 => break,
                    0..=2 => {
                        bits.ue()?;
                    }
                    _ => return Err(missing),
                }
            }
        }
    }
    if (pps.weighted_pred && p) || (pps.weighted_bipred_idc == 1 && b) {
        skip_pred_weight_table(bits, sps.chroma_array_type != 0, l0, b.then_some(l1))?;
    }
    if reference {
        if idr {
            slice.no_output_of_prior_pics = bits.flag()?;
            bits.flag()?; // long_term_reference_flag
        } else if bits.flag()? {
            // adaptive_ref_pic_marking_mode_flag: the operations up to 0.
            loop {
                let operation = bits.ue()?;
                match operation {
                    0 => break,
                    1 | 2 | 4 => {
                        bits.ue()?;
                    }
                    3 => {
                        bits.ue()?; // difference_of_pic_nums_minus1
                        bits.ue()?; // long_term_frame_idx
                    }
                    5 => slice.mmco5 = true,
                    6 => {
                        bits.ue()?; // long_term_frame_idx
                    }
                    _ => return Err(missing),
                }
            }
        }
    }
    Ok(slice)
}

/// pred_weight_table( ) (7.3.3.2).
fn skip_pred_weight_table(
    bits: &mut Bits<'_>,
    chroma: bool,
    l0: u32,
    l1: Option<u32>,
) -> std::result::Result<(), BitError> {
    bits.ue()?; // luma_log2_weight_denom
    if chroma {
        bits.ue()?; // chroma_log2_weight_denom
    }
    for count in [Some(l0), l1].into_iter().flatten() {
        for _ in 0..count {
            if bits.flag()? {
                bits.se()?;
                bits.se()?;
            }
            if chroma && bits.flag()? {
                for _ in 0..4 {
                    bits.se()?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_harness_protocol::unhex;

    #[test]
    fn sequence_parameter_sets_state_the_frame_rate_and_order_count_coding() {
        // From a libx264 (High 4:4:4) encode at 30000/1001 Hz.
        let sps = unhex("67f4000a919a623d808800001f4800075300789129c0");
        let pool = Budget::new(1 << 20).expect("pool");
        let mut tables = Tables::new(&pool);
        let (id, _) = tables.put_sps(&sps[1..]).expect("memory").expect("taken");
        let sps = tables.sps(id).expect("stored");
        assert_eq!(
            sps.timing,
            Some(Timing {
                num_units_in_tick: 1001,
                time_scale: 60000
            })
        );
        assert_eq!(sps.poc, PocType::Lsb { log2_max_lsb: 5 });
    }

    #[test]
    fn an_h264_scaling_delta_out_of_range_rejects_the_parameter_set() {
        // Profile 100 with a first scaling list delta of i32::MAX.
        let sps = [0x64, 0, 0, 0x5d, 0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xfe];
        let pool = Budget::new(1 << 20).expect("pool");
        let mut tables = Tables::new(&pool);
        assert!(matches!(tables.put_sps(&sps), Ok(None)));
    }
}
