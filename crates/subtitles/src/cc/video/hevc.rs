//! The HEVC parameter set and slice segment header fields that decide picture
//! output (7.3.2.2, 7.3.2.3, 7.3.6.1, 7.3.7): order counts, the reference
//! picture set, `no_output_of_prior_pics_flag`, `pic_output_flag`, the
//! sub-layer buffering values and the VUI frame period. The rest of each RBSP
//! is passed over.

use super::nal::{LastPayload, Timing, vui_timing};
use triergon_bitstream::{BitError, BitSource, Rbsp};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::{Codec as Coding, picture_output::HevcLimits, skip_hevc_profile_tier_level};

/// The fields of a sequence parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sps {
    pub separate_colour_plane: bool,
    pub log2_max_lsb: u32,
    /// The highest sub-layer's buffering values.
    pub limits: HevcLimits,
    /// The short-term reference picture sets: `sets` entries of the table's
    /// set pool from `sets_start`.
    pub sets_start: usize,
    pub sets: usize,
    /// `long_term_ref_pics_present_flag` and the SPS's long-term order count
    /// LSBs (`lt_count` entries of the LSB pool from `lt_start`).
    pub long_term: bool,
    pub lt_start: usize,
    pub lt_count: usize,
    pub timing: Option<Timing>,
}

/// The delta order counts of one short-term reference picture set as read:
/// the negative ones first.
#[derive(Clone, Copy, Debug, Default)]
struct Deltas {
    values: [i32; 32],
    negative: usize,
    len: usize,
}

impl Deltas {
    /// Negative deltas come first, so they are pushed before the positive.
    fn push_negative(&mut self, delta: i64) {
        if self.len < self.values.len() {
            self.values[self.len] = delta as i32;
            self.len += 1;
            self.negative += 1;
        }
    }

    fn push_positive(&mut self, delta: i64) {
        if self.len < self.values.len() {
            self.values[self.len] = delta as i32;
            self.len += 1;
        }
    }

    fn as_slice(&self) -> &[i32] {
        &self.values[..self.len]
    }
}

fn push_deltas(
    deltas: &[i32],
    references: &mut BudgetVec<Reference>,
) -> std::result::Result<(), Failure> {
    references.try_reserve(deltas.len())?;
    for &delta in deltas {
        references
            .try_push(Reference::Delta(delta))
            .map_err(|rejected| Failure::Memory(rejected.error))?;
    }
    Ok(())
}

/// One short-term reference picture set: `negative` then `positive` deltas of
/// the delta pool from `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StSet {
    start: usize,
    negative: usize,
    positive: usize,
}

/// The fields of a picture parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pps {
    pub sps_id: u8,
    pub output_flag_present: bool,
    pub extra_slice_header_bits: u32,
}

/// A reference picture of a slice's reference picture set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reference {
    /// Its order count relative to the current picture's.
    Delta(i32),
    /// A long-term picture named by its order count LSBs.
    Lsb(u32),
    /// A long-term picture named by its order count relative to the current
    /// picture's.
    LongDelta(i64),
}

/// The fields of a picture's first slice segment header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slice {
    pub nal_type: u8,
    pub temporal_id: u8,
    pub sps_id: u8,
    pub no_output_of_prior_pics: bool,
    pub pic_output: bool,
    pub poc_lsb: u32,
}

/// The parameter sets received, by id, and their pooled lists.
#[derive(Debug)]
pub(crate) struct Tables {
    sps: BudgetVec<Option<Sps>>,
    pps: BudgetVec<Option<Pps>>,
    last_sps: LastPayload,
    last_pps: LastPayload,
    sets: BudgetVec<StSet>,
    deltas: BudgetVec<i32>,
    lt_lsbs: BudgetVec<u32>,
}

/// The pool lengths, to return to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mark {
    sets: usize,
    deltas: usize,
    lt_lsbs: usize,
}

const SPS_IDS: usize = 16;
const PPS_IDS: usize = 64;

type Bits<'a> = Rbsp<'a>;

/// Why a read stopped.
enum Failure {
    Invalid,
    Memory(triergon_core::Error),
}

impl From<BitError> for Failure {
    fn from(_: BitError) -> Self {
        Self::Invalid
    }
}

impl From<triergon_core::Error> for Failure {
    fn from(error: triergon_core::Error) -> Self {
        Self::Memory(error)
    }
}

impl Tables {
    pub(crate) fn new(pool: &Budget) -> Self {
        Self {
            sps: BudgetVec::new(pool),
            pps: BudgetVec::new(pool),
            last_sps: LastPayload::new(pool),
            last_pps: LastPayload::new(pool),
            sets: BudgetVec::new(pool),
            deltas: BudgetVec::new(pool),
            lt_lsbs: BudgetVec::new(pool),
        }
    }

    pub(crate) fn sps(&self, id: u8) -> Option<&Sps> {
        self.sps.get(usize::from(id))?.as_ref()
    }

    pub(crate) fn mark(&self) -> Mark {
        Mark {
            sets: self.sets.len(),
            deltas: self.deltas.len(),
            lt_lsbs: self.lt_lsbs.len(),
        }
    }

    pub(crate) fn rollback(&mut self, mark: Mark) {
        self.sets.truncate(mark.sets);
        self.deltas.truncate(mark.deltas);
        self.lt_lsbs.truncate(mark.lt_lsbs);
    }

    /// Reads an SPS NAL unit payload (after the two header bytes) and stores
    /// it, returning its id and the set it replaces. An SPS cut short or out
    /// of range is ignored.
    pub(crate) fn put_sps(&mut self, payload: &[u8]) -> Result<Option<(u8, Option<Sps>)>> {
        if self.sps.is_empty() {
            self.sps.try_resize(SPS_IDS, None)?;
        }
        if self.last_sps.repeats(payload) {
            return Ok(None);
        }
        self.last_sps.remember(payload)?;
        let mark = self.mark();
        let mut bits = Rbsp::lenient(payload);
        match self.read_sps(&mut bits) {
            Ok((id, sps)) => {
                let slot = usize::from(id);
                if let Some(old) = self.sps[slot]
                    && self.same(&old, &sps)
                {
                    self.rollback(mark);
                    return Ok(None);
                }
                Ok(Some((id, self.sps[slot].replace(sps))))
            }
            Err(failure) => {
                self.rollback(mark);
                match failure {
                    Failure::Memory(error) => Err(error),
                    Failure::Invalid => Ok(None),
                }
            }
        }
    }

    /// Whether two sets read alike, their pooled lists compared by value.
    fn same(&self, a: &Sps, b: &Sps) -> bool {
        let unpooled = |sps: &Sps| Sps {
            sets_start: 0,
            lt_start: 0,
            ..*sps
        };
        let sets = |sps: &Sps| &self.sets[sps.sets_start..sps.sets_start + sps.sets];
        let deltas = |sps: &Sps| {
            sets(sps)
                .iter()
                .flat_map(|set| &self.deltas[set.start..set.start + set.negative + set.positive])
        };
        let shape = |sps: &Sps| sets(sps).iter().map(|set| (set.negative, set.positive));
        unpooled(a) == unpooled(b)
            && shape(a).eq(shape(b))
            && deltas(a).eq(deltas(b))
            && self.lt_lsbs[a.lt_start..a.lt_start + a.lt_count]
                == self.lt_lsbs[b.lt_start..b.lt_start + b.lt_count]
    }

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
            let slot = self.pps.get_mut(usize::from(id))?;
            Some((id, slot.replace(pps)))
        }))
    }

    /// Forgets the last sets read, after a failed step puts sets back.
    pub(crate) fn forget_repeats(&mut self) {
        self.last_sps.forget();
        self.last_pps.forget();
    }

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

    /// The fields of a slice segment NAL unit that starts a picture, and its
    /// reference picture set in `references` (cleared first). `None` when it
    /// does not start a picture, its parameter sets are missing or it is cut
    /// short.
    pub(crate) fn slice(
        &self,
        nal: &[u8],
        references: &mut BudgetVec<Reference>,
    ) -> Result<Option<Slice>> {
        references.clear();
        let (Some(&first), Some(&second)) = (nal.first(), nal.get(1)) else {
            return Ok(None);
        };
        let mut bits = Rbsp::lenient(nal.get(2..).unwrap_or_default());
        match self.read_slice(
            &mut bits,
            first >> 1 & 0x3f,
            (second & 7).saturating_sub(1),
            references,
        ) {
            Ok(slice) => Ok(slice),
            Err(Failure::Invalid) => {
                references.clear();
                Ok(None)
            }
            Err(Failure::Memory(error)) => Err(error),
        }
    }

    fn read_sps(&mut self, bits: &mut Bits<'_>) -> std::result::Result<(u8, Sps), Failure> {
        bits.read(4)?; // sps_video_parameter_set_id
        let sub_layers = bits.read(3)?; // sps_max_sub_layers_minus1
        if sub_layers > 6 {
            return Err(Failure::Invalid);
        }
        bits.flag()?; // sps_temporal_id_nesting_flag
        skip_hevc_profile_tier_level(bits, sub_layers as u8)?;
        let id = bits.ue()?;
        if id as usize >= SPS_IDS {
            return Err(Failure::Invalid);
        }
        let chroma_format_idc = bits.ue()?;
        let separate_colour_plane = chroma_format_idc == 3 && bits.flag()?;
        bits.ue()?; // pic_width_in_luma_samples
        bits.ue()?; // pic_height_in_luma_samples
        if bits.flag()? {
            for _ in 0..4 {
                bits.ue()?; // conformance window offsets
            }
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        // log2_max_pic_order_cnt_lsb, summed in 32 bits and limited to 16 as
        // FFmpeg's decoder does.
        let log2_max_lsb = bits.ue()?.wrapping_add(4);
        if log2_max_lsb > 16 {
            return Err(Failure::Invalid);
        }
        let all_layers = bits.flag()?;
        let mut limits = HevcLimits::from_sps(0, 0, 0);
        for _ in (if all_layers { 0 } else { sub_layers })..=sub_layers {
            let dpb = bits.ue()?;
            let reorder = bits.ue()?;
            let latency = bits.ue()?;
            limits = HevcLimits::from_sps(dpb, reorder, latency);
        }
        for _ in 0..6 {
            bits.ue()?; // block sizes and transform hierarchy depths
        }
        if bits.flag()? && bits.flag()? {
            skip_scaling_list_data(bits)?;
        }
        bits.read(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
        if bits.flag()? {
            bits.read(8)?; // pcm sample bit depths
            bits.ue()?;
            bits.ue()?;
            bits.flag()?;
        }
        let sets = bits.ue()? as usize;
        if sets > 64 {
            return Err(Failure::Invalid);
        }
        let sets_start = self.sets.len();
        self.sets.try_reserve(sets)?;
        for index in 0..sets {
            let deltas = self.st_set(bits, sets_start, index, sets)?;
            let start = self.deltas.len();
            self.deltas.try_extend_from_slice(deltas.as_slice())?;
            self.sets
                .try_push(StSet {
                    start,
                    negative: deltas.negative,
                    positive: deltas.len - deltas.negative,
                })
                .map_err(|rejected| Failure::Memory(rejected.error))?;
        }
        let long_term = bits.flag()?;
        let lt_start = self.lt_lsbs.len();
        let mut lt_count = 0;
        if long_term {
            lt_count = bits.ue()? as usize;
            if lt_count > 32 {
                return Err(Failure::Invalid);
            }
            self.lt_lsbs.try_reserve(lt_count)?;
            for _ in 0..lt_count {
                let lsb = bits.read(log2_max_lsb)?;
                bits.flag()?; // used_by_curr_pic_lt_sps_flag
                self.lt_lsbs
                    .try_push(lsb)
                    .map_err(|rejected| Failure::Memory(rejected.error))?;
            }
        }
        bits.read(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
        let timing = if bits.flag()? {
            vui_timing(bits, Coding::Hevc).ok().flatten()
        } else {
            None
        };
        Ok((
            id as u8,
            Sps {
                separate_colour_plane,
                log2_max_lsb,
                limits,
                sets_start,
                sets,
                long_term,
                lt_start,
                lt_count,
                timing,
            },
        ))
    }

    /// st_ref_pic_set( index ) (7.3.7) as its delta order counts (7.4.8),
    /// in an SPS whose sets start at `sets_start` (`index < sets`) or in a
    /// slice header (`index == sets`).
    fn st_set(
        &self,
        bits: &mut Bits<'_>,
        sets_start: usize,
        index: usize,
        sets: usize,
    ) -> std::result::Result<Deltas, Failure> {
        let mut out = Deltas::default();
        if index != 0 && bits.flag()? {
            // inter_ref_pic_set_prediction_flag
            let delta_idx = if index == sets {
                bits.ue()? as usize + 1
            } else {
                1
            };
            let sign = bits.flag()?;
            let abs = i64::from(bits.ue()?) + 1;
            let delta_rps = if sign { -abs } else { abs };
            let reference = *index
                .checked_sub(delta_idx)
                .and_then(|r| self.sets.get(sets_start + r))
                .ok_or(Failure::Invalid)?;
            let count = reference.negative + reference.positive;
            // use_delta_flag of each reference delta and of deltaRps itself.
            let mut used = [false; 33];
            for flag in &mut used[..=count] {
                *flag = bits.flag()? || bits.flag()?;
            }
            let delta = |at: usize| i64::from(self.deltas[reference.start + at]);
            // 7-61: S0 from the reference's positive deltas (last first),
            // deltaRps, then its negative deltas.
            for j in (0..reference.positive).rev() {
                let d = delta(reference.negative + j) + delta_rps;
                if d < 0 && used[reference.negative + j] {
                    out.push_negative(d);
                }
            }
            if delta_rps < 0 && used[count] {
                out.push_negative(delta_rps);
            }
            for j in 0..reference.negative {
                let d = delta(j) + delta_rps;
                if d < 0 && used[j] {
                    out.push_negative(d);
                }
            }
            // 7-62: S1 likewise.
            for j in (0..reference.negative).rev() {
                let d = delta(j) + delta_rps;
                if d > 0 && used[j] {
                    out.push_positive(d);
                }
            }
            if delta_rps > 0 && used[count] {
                out.push_positive(delta_rps);
            }
            for j in 0..reference.positive {
                let d = delta(reference.negative + j) + delta_rps;
                if d > 0 && used[reference.negative + j] {
                    out.push_positive(d);
                }
            }
            return Ok(out);
        }
        let negative = bits.ue()?;
        let positive = bits.ue()?;
        if negative > 16 || positive > 16 {
            return Err(Failure::Invalid);
        }
        let mut poc = 0i64;
        for _ in 0..negative {
            poc -= i64::from(bits.ue()?) + 1;
            bits.flag()?; // used_by_curr_pic_s0_flag
            out.push_negative(poc);
        }
        poc = 0;
        for _ in 0..positive {
            poc += i64::from(bits.ue()?) + 1;
            bits.flag()?; // used_by_curr_pic_s1_flag
            out.push_positive(poc);
        }
        Ok(out)
    }

    fn read_slice(
        &self,
        bits: &mut Bits<'_>,
        nal_type: u8,
        temporal_id: u8,
        references: &mut BudgetVec<Reference>,
    ) -> std::result::Result<Option<Slice>, Failure> {
        if !bits.flag()? {
            return Ok(None); // not the first slice segment of a picture
        }
        let irap = (16..=23).contains(&nal_type);
        let no_output_of_prior_pics = irap && bits.flag()?;
        let pps_id = bits.ue()?;
        let pps = *self
            .pps
            .get(pps_id as usize)
            .and_then(Option::as_ref)
            .ok_or(Failure::Invalid)?;
        let sps = *self.sps(pps.sps_id).ok_or(Failure::Invalid)?;
        bits.skip(pps.extra_slice_header_bits)?; // slice_reserved_flag
        bits.ue()?; // slice_type
        let pic_output = !pps.output_flag_present || bits.flag()?;
        if sps.separate_colour_plane {
            bits.read(2)?; // colour_plane_id
        }
        let mut slice = Slice {
            nal_type,
            temporal_id,
            sps_id: pps.sps_id,
            no_output_of_prior_pics,
            pic_output,
            poc_lsb: 0,
        };
        if matches!(nal_type, 19 | 20) {
            return Ok(Some(slice)); // IDR: no order count LSBs, an empty set
        }
        slice.poc_lsb = bits.read(sps.log2_max_lsb)?;
        if bits.flag()? {
            // short_term_ref_pic_set_sps_flag
            let width = usize::BITS - sps.sets.saturating_sub(1).leading_zeros();
            let index = if sps.sets > 1 {
                bits.read(width)? as usize
            } else {
                0
            };
            let set = *self
                .sets
                .get(sps.sets_start + index)
                .filter(|_| index < sps.sets)
                .ok_or(Failure::Invalid)?;
            push_deltas(
                &self.deltas[set.start..set.start + set.negative + set.positive],
                references,
            )?;
        } else {
            let own = self.st_set(bits, sps.sets_start, sps.sets, sps.sets)?;
            push_deltas(own.as_slice(), references)?;
        }
        self.read_long_term(bits, sps, slice, references)?;
        Ok(Some(slice))
    }

    fn read_long_term(
        &self,
        bits: &mut Bits<'_>,
        sps: Sps,
        slice: Slice,
        references: &mut BudgetVec<Reference>,
    ) -> std::result::Result<(), Failure> {
        if !sps.long_term {
            return Ok(());
        }
        let from_sps = if sps.lt_count > 0 {
            bits.ue()? as usize
        } else {
            0
        };
        let own = bits.ue()? as usize;
        if from_sps > sps.lt_count || from_sps + own > 32 {
            return Err(Failure::Invalid);
        }
        let max_lsb = 1i64 << sps.log2_max_lsb;
        let mut cycle = 0i64;
        references.try_reserve(from_sps + own)?;
        for i in 0..from_sps + own {
            let lsb = if i < from_sps {
                let width = usize::BITS - sps.lt_count.saturating_sub(1).leading_zeros();
                let index = if sps.lt_count > 1 {
                    bits.read(width)? as usize
                } else {
                    0
                };
                *self
                    .lt_lsbs
                    .get(sps.lt_start + index)
                    .filter(|_| index < sps.lt_count)
                    .ok_or(Failure::Invalid)?
            } else {
                let lsb = bits.read(sps.log2_max_lsb)?;
                bits.flag()?; // used_by_curr_pic_lt_flag
                lsb
            };
            // delta_poc_msb_present_flag and DeltaPocMsbCycleLt (7-52), whose
            // absent deltas count 0.
            let present = bits.flag()?;
            let delta = if present { i64::from(bits.ue()?) } else { 0 };
            cycle = if i == 0 || i == from_sps {
                delta
            } else {
                cycle + delta
            };
            let reference = if present {
                Reference::LongDelta(-cycle * max_lsb - (i64::from(slice.poc_lsb) - i64::from(lsb)))
            } else {
                Reference::Lsb(lsb)
            };
            references
                .try_push(reference)
                .map_err(|rejected| Failure::Memory(rejected.error))?;
        }
        Ok(())
    }
}

fn read_pps(bits: &mut Bits<'_>) -> std::result::Result<(u8, Pps), BitError> {
    let id = bits.ue()?;
    let sps_id = bits.ue()?;
    if id as usize >= PPS_IDS || sps_id as usize >= SPS_IDS {
        return Err(BitError::LongPrefix);
    }
    bits.flag()?; // dependent_slice_segments_enabled_flag
    let output_flag_present = bits.flag()?;
    let extra_slice_header_bits = bits.read(3)?;
    Ok((
        id as u8,
        Pps {
            sps_id: sps_id as u8,
            output_flag_present,
            extra_slice_header_bits,
        },
    ))
}

/// scaling_list_data( ) (7.3.4).
fn skip_scaling_list_data(bits: &mut Bits<'_>) -> std::result::Result<(), BitError> {
    for size in 0..4 {
        for _ in (0..6).step_by(if size == 3 { 3 } else { 1 }) {
            if bits.flag()? {
                if size > 1 {
                    bits.se()?;
                }
                for _ in 0..64.min(1 << (4 + (size << 1))) {
                    bits.se()?;
                }
            } else {
                bits.ue()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_harness_protocol::unhex;

    /// Whether an SPS NAL unit is taken, and its timing.
    fn timing(nal: &[u8]) -> (bool, Option<Timing>) {
        let pool = Budget::new(1 << 20).expect("pool");
        let mut tables = Tables::new(&pool);
        match tables.put_sps(&nal[2..]).expect("memory") {
            Some((id, _)) => (true, tables.sps(id).and_then(|sps| sps.timing)),
            None => (false, None),
        }
    }

    #[test]
    fn sequence_parameter_sets_state_the_frame_rate() {
        // From a libx265 encode at 30000/1001 Hz.
        let hevc = unhex(
            "4201010408000003009e0800000300001e90041062cb2225c995e02dc080800100000303e90000753008",
        );
        let expected = Timing {
            num_units_in_tick: 1001,
            time_scale: 30000,
        };
        assert_eq!(timing(&hevc), (true, Some(expected)));
    }

    #[test]
    fn hevc_poc_lsb_lengths_are_limited_to_16_bits_as_ffmpeg_sums_them() {
        let expected = (
            true,
            Some(Timing {
                num_units_in_tick: 1001,
                time_scale: 60_000,
            }),
        );
        assert_eq!(timing(&hevc_sps(12)), expected);
        assert_eq!(timing(&hevc_sps(13)), (false, None));
        assert_eq!(timing(&hevc_sps(0xffff_fffb)), (false, None));
        // FFmpeg sums in 32 bits: 0xFFFFFFFE codes two-bit LSBs.
        assert_eq!(timing(&hevc_sps(0xffff_fffe)), expected);
    }

    #[test]
    fn a_repeated_parameter_set_changes_nothing() {
        let pool = Budget::new(1 << 20).expect("pool");
        let mut tables = Tables::new(&pool);
        let sps = hevc_sps(4);
        assert!(matches!(tables.put_sps(&sps[2..]), Ok(Some((0, None)))));
        let pooled = tables.mark();
        assert!(matches!(tables.put_sps(&sps[2..]), Ok(None)));
        assert_eq!(tables.mark().lt_lsbs, pooled.lt_lsbs);
    }

    /// Writes RBSP bits and frames them as a NAL unit.
    #[derive(Default)]
    struct BitWriter {
        bits: Vec<bool>,
    }

    impl BitWriter {
        fn put(&mut self, value: u64, width: u32) -> &mut Self {
            self.bits
                .extend((0..width).rev().map(|bit| value >> bit & 1 == 1));
            self
        }

        fn flag(&mut self, set: bool) -> &mut Self {
            self.put(u64::from(set), 1)
        }

        fn ue(&mut self, value: u32) -> &mut Self {
            let code = u64::from(value) + 1;
            let width = 64 - code.leading_zeros();
            self.put(0, width - 1).put(code, width)
        }

        /// The NAL unit: `header`, the bits with their stop bit, and emulation
        /// prevention bytes.
        fn nal(&mut self, header: &[u8]) -> Vec<u8> {
            self.flag(true);
            while !self.bits.len().is_multiple_of(8) {
                self.flag(false);
            }
            let mut nal = header.to_vec();
            let mut zeros = 0;
            for byte in self.bits.chunks(8) {
                let byte = byte.iter().fold(0u8, |b, &bit| b << 1 | u8::from(bit));
                if zeros == 2 && byte <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                nal.push(byte);
            }
            nal
        }
    }

    /// An HEVC sequence parameter set with VUI timing 1001/60000 whose
    /// `log2_max_pic_order_cnt_lsb_minus4` is coded as `poc_lsb_minus4`, with one
    /// long-term reference picture whose POC LSBs take that many bits plus four.
    fn hevc_sps(poc_lsb_minus4: u32) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.put(0, 4).put(0, 3).flag(true); // VPS, one sub-layer, nesting
        bits.put(0x0160_0000, 32).put(0, 32).put(0, 24).put(93, 8); // profile_tier_level
        bits.ue(0).ue(1).ue(1920).ue(1080).flag(false); // id, 4:2:0, size, no window
        bits.ue(0).ue(0).ue(poc_lsb_minus4);
        bits.flag(true).ue(4).ue(0).ue(0); // sub-layer ordering
        for _ in 0..6 {
            bits.ue(0); // block sizes and transform depths
        }
        bits.flag(false).put(0, 2).flag(false); // no scaling lists, AMP, SAO, PCM
        bits.ue(0); // no short-term reference picture sets
        let poc_lsb_bits = poc_lsb_minus4.wrapping_add(4).min(16);
        bits.flag(true).ue(1).put(1, poc_lsb_bits).flag(true); // one long-term picture
        bits.put(0, 2).flag(true); // TMVP, strong intra smoothing; VUI
        bits.put(0, 4).put(0, 3).flag(false); // VUI flags up to the display window
        bits.flag(true).put(1001, 32).put(60_000, 32).flag(false);
        bits.flag(false).flag(false); // no bitstream restriction; no extensions
        bits.nal(&[0x42, 0x01])
    }
}
