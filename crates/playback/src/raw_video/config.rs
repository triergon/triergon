//! Active decoder configuration for raw Annex B AVC and base-layer HEVC.
pub(crate) mod hevc;

use crate::hls_avc::syntax as avc;
pub use crate::hls_avc::syntax::AvcVuiTiming;
pub use hevc::HevcVuiTiming;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, SourceId};
use triergon_framing::{Codec, NalHeader, RawAccessUnit};

/// Exact active NAL bodies, without start codes or Annex B padding.
/// Dimensions use the AVC frame crop or HEVC conformance window.
#[derive(Clone, Debug)]
pub struct RawVideoConfiguration {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Timing fields carried by the active AVC SPS, when present.
    pub avc_timing: Option<AvcVuiTiming>,
    pub vps: Option<Bytes>,
    pub sps: Bytes,
    pub pps: Bytes,
}

impl RawVideoConfiguration {
    pub fn parameter_sets(&self) -> impl Iterator<Item = &Bytes> {
        self.vps.iter().chain([&self.sps, &self.pps])
    }

    /// Project physically present Annex E timing from the active base-layer
    /// HEVC SPS. This is independent of raster-only configuration tracking.
    /// Returns `None` for AVC or absent SPS timing. VPS inference and HRD
    /// are outside the projection.
    pub fn hevc_timing(&self) -> Result<Option<HevcVuiTiming>> {
        if self.codec != Codec::Hevc {
            return Ok(None);
        }
        if !matches!(
            NalHeader::parse(self.codec, &self.sps)?,
            NalHeader::Hevc {
                nal_unit_type: 33,
                layer_id: 0,
                ..
            }
        ) {
            return Err(Error::Invalid("expected base-layer HEVC SPS"));
        }
        hevc::sps_timing(&self.sps[self.codec.minimum_header_bytes()..])
    }
}

impl PartialEq for RawVideoConfiguration {
    fn eq(&self, other: &Self) -> bool {
        self.codec == other.codec
            && self.width == other.width
            && self.height == other.height
            && self.avc_timing == other.avc_timing
            && self
                .parameter_sets()
                .map(Bytes::as_ref)
                .eq(other.parameter_sets().map(Bytes::as_ref))
    }
}

impl Eq for RawVideoConfiguration {}

#[derive(Clone, Copy)]
struct Sps {
    vps: Option<u8>,
    width: u32,
    height: u32,
    avc_timing: Option<AvcVuiTiming>,
}

struct Set<T> {
    bytes: Bytes,
    info: T,
}

#[derive(Clone, Copy)]
enum Update<'a> {
    Vps(u8, &'a Bytes),
    Sps(u8, Sps, &'a Bytes),
    Pps(u8, u8, &'a Bytes),
}

/// Observe complete access units from one reader, in decoding order. Use a
/// fresh tracker when restarting the source. Published configurations alias
/// the original charged NAL owners and survive later ID reuse.
pub struct RawVideoConfigTracker {
    pool: Budget,
    codec: Codec,
    source: Option<SourceId>,
    max_parameter_set_bytes: usize,
    vps: [Option<Bytes>; 16],
    sps: [Option<Set<Sps>>; 32],
    pps: [Option<Set<u8>>; 256],
}

impl RawVideoConfigTracker {
    pub fn new(pool: &Budget, codec: Codec, max_parameter_set_bytes: usize) -> Result<Self> {
        if max_parameter_set_bytes == 0 {
            return Err(Error::Invalid("zero raw video parameter-set limit"));
        }
        Ok(Self {
            pool: pool.clone(),
            codec,
            source: None,
            max_parameter_set_bytes,
            vps: std::array::from_fn(|_| None),
            sps: std::array::from_fn(|_| None),
            pps: std::array::from_fn(|_| None),
        })
    }

    /// Return the active chain for the first base VCL in this unit. Prefix-only
    /// units update the tables and return None. A failed observation commits
    /// neither its parameter sets nor its source binding.
    pub fn observe(&mut self, unit: &RawAccessUnit) -> Result<Option<RawVideoConfiguration>> {
        if unit.codec != self.codec {
            return Err(Error::Invalid("raw video configuration codec mismatch"));
        }
        if self.source.is_some_and(|source| source != unit.source) {
            return Err(Error::SourceMismatch);
        }
        let mut updates = BudgetVec::new(&self.pool);
        let mut active = None;
        for nal in unit.nals() {
            let facts = nal.facts();
            if facts.source != unit.source {
                return Err(Error::SourceMismatch);
            }
            if !nal.bytes().belongs_to(&self.pool) {
                return Err(Error::ForeignPool);
            }
            if facts.header.codec() != self.codec {
                return Err(Error::Invalid("raw video configuration NAL codec mismatch"));
            }
            if matches!(facts.header, NalHeader::Hevc { layer_id, .. } if layer_id != 0) {
                continue;
            }
            let nal_type = facts.header.nal_unit_type();
            if let Some(update) = self.parameter(nal_type, nal.bytes())? {
                updates
                    .try_push(update)
                    .map_err(|rejected| rejected.error)?;
            } else if active.is_none() {
                let raw = &nal.bytes()[self.codec.minimum_header_bytes()..];
                let pps_id = match (self.codec, nal_type) {
                    (Codec::Avc, 1 | 5) => avc::slice_header_prefix(raw)?.1,
                    (Codec::Hevc, 0..=9 | 16..=21) => hevc::slice_pps(raw, nal_type)?,
                    _ => continue,
                };
                active = Some(self.resolve(pps_id, updates.as_slice())?);
            }
        }
        if active.is_some() != unit.has_vcl {
            return Err(Error::Invalid("raw video configuration VCL mismatch"));
        }
        for &update in updates.as_slice() {
            self.commit(update);
        }
        self.source = Some(unit.source);
        Ok(active)
    }

    fn parameter<'a>(&self, nal_type: u8, bytes: &'a Bytes) -> Result<Option<Update<'a>>> {
        if !matches!(
            (self.codec, nal_type),
            (Codec::Avc, 7 | 8) | (Codec::Hevc, 32..=34)
        ) {
            return Ok(None);
        }
        if bytes.len() > self.max_parameter_set_bytes {
            return Err(Error::Limit {
                requested: bytes.len(),
                maximum: self.max_parameter_set_bytes,
            });
        }
        let raw = &bytes[self.codec.minimum_header_bytes()..];
        let update = match (self.codec, nal_type) {
            (Codec::Hevc, 32) => Update::Vps(hevc::vps(raw)?, bytes),
            (Codec::Hevc, 33) => {
                let (id, info) = hevc::sps(raw)?;
                Update::Sps(id, info, bytes)
            }
            (Codec::Hevc, 34) => {
                let (id, sps) = hevc::pps(raw)?;
                Update::Pps(id, sps, bytes)
            }
            (Codec::Avc, 7) => {
                let (id, raster, avc_timing) = avc::sps_with_timing(raw)?;
                Update::Sps(
                    id,
                    Sps {
                        vps: None,
                        width: raster.width,
                        height: raster.height,
                        avc_timing,
                    },
                    bytes,
                )
            }
            (Codec::Avc, 8) => {
                let (id, sps) = avc::pps(raw)?;
                Update::Pps(id, sps, bytes)
            }
            _ => unreachable!("checked parameter-set type"),
        };
        Ok(Some(update))
    }

    fn resolve(&self, pps_id: u8, updates: &[Update<'_>]) -> Result<RawVideoConfiguration> {
        let (sps_id, pps) = updates
            .iter()
            .rev()
            .find_map(|update| match *update {
                Update::Pps(id, sps, bytes) if id == pps_id => Some((sps, bytes)),
                _ => None,
            })
            .or_else(|| {
                self.pps[usize::from(pps_id)]
                    .as_ref()
                    .map(|pps| (pps.info, &pps.bytes))
            })
            .ok_or(Error::Invalid("raw video picture has no active PPS"))?;
        let (info, sps) = updates
            .iter()
            .rev()
            .find_map(|update| match *update {
                Update::Sps(id, info, bytes) if id == sps_id => Some((info, bytes)),
                _ => None,
            })
            .or_else(|| {
                self.sps[usize::from(sps_id)]
                    .as_ref()
                    .map(|sps| (sps.info, &sps.bytes))
            })
            .ok_or(Error::Invalid("raw video picture has no active SPS"))?;
        let vps = if let Some(vps_id) = info.vps {
            let bytes = updates
                .iter()
                .rev()
                .find_map(|update| match *update {
                    Update::Vps(id, bytes) if id == vps_id => Some(bytes),
                    _ => None,
                })
                .or(self.vps[usize::from(vps_id)].as_ref())
                .ok_or(Error::Invalid("raw HEVC picture has no active VPS"))?;
            Some(bytes.clone())
        } else {
            None
        };
        Ok(RawVideoConfiguration {
            codec: self.codec,
            width: info.width,
            height: info.height,
            avc_timing: info.avc_timing,
            vps,
            sps: sps.clone(),
            pps: pps.clone(),
        })
    }

    fn commit(&mut self, update: Update<'_>) {
        match update {
            Update::Vps(id, bytes) => self.vps[usize::from(id)] = Some(bytes.clone()),
            Update::Sps(id, info, bytes) => {
                self.sps[usize::from(id)] = Some(Set {
                    bytes: bytes.clone(),
                    info,
                });
            }
            Update::Pps(id, info, bytes) => {
                self.pps[usize::from(id)] = Some(Set {
                    bytes: bytes.clone(),
                    info,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use triergon_core::{InputWindow, SourceCapabilities, Step, WorkBudget};
    use triergon_framing::{RawAnnexBLimits, RawAnnexBReader};

    const SOURCE: SourceId = SourceId(77);
    static NO: AtomicBool = AtomicBool::new(false);

    #[derive(Default)]
    struct Bits(Vec<bool>);

    impl Bits {
        fn fixed(&mut self, value: u32, width: u8) {
            self.0
                .extend((0..width).rev().map(|bit| value & (1 << bit) != 0));
        }

        fn zeroes(&mut self, count: usize) {
            self.0.extend(std::iter::repeat_n(false, count));
        }

        fn ue(&mut self, value: u32) {
            let code = value + 1;
            let zeros = code.ilog2();
            self.zeroes(zeros as usize);
            self.fixed(code, zeros as u8 + 1);
        }

        fn nal(mut self, header: &[u8]) -> Vec<u8> {
            self.0.push(true);
            self.0.resize(self.0.len().next_multiple_of(8), false);
            let mut n = header.to_vec();
            let mut zeroes = 0;
            for octet in self.0.chunks(8) {
                let byte = octet.iter().fold(0, |n, &bit| n * 2 + u8::from(bit));
                if zeroes == 2 && byte <= 3 {
                    n.push(3);
                    zeroes = 0;
                }
                n.push(byte);
                zeroes = if byte == 0 { zeroes + 1 } else { 0 };
            }
            n
        }
    }

    fn hevc_sps(vps: u32, id: u32, width: u32, height: u32) -> Vec<u8> {
        hevc_sps_prefix(vps, id, width, height, 1).nal(&[66, 1])
    }

    fn hevc_sps_prefix(vps: u32, id: u32, width: u32, height: u32, profile: u32) -> Bits {
        let mut b = Bits::default();
        b.fixed(vps, 4);
        b.fixed(1, 3); // One sublayer profile entry.
        b.fixed(1, 1);
        b.fixed(profile, 8);
        b.zeroes(88); // Remainder of the general profile and level.
        b.fixed(3, 2);
        b.zeroes(14);
        b.fixed(1, 8);
        b.zeroes(88); // Remainder of the sublayer profile and level.
        b.ue(id);
        b.ue(1); // 4:2:0 crop units.
        b.ue(width);
        b.ue(height);
        b.fixed(1, 1);
        for offset in [1, 2, 3, 4] {
            b.ue(offset);
        }
        b
    }

    fn hevc_pps(id: u32, sps: u32) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(id);
        b.ue(sps);
        b.nal(&[68, 1])
    }

    fn hevc_slice(kind: u8, pps: u32) -> Vec<u8> {
        let mut b = Bits::default();
        b.fixed(1, 1);
        if (16..=23).contains(&kind) {
            b.fixed(0, 1);
        }
        b.ue(pps);
        b.nal(&[kind << 1, 1])
    }

    fn read(pool: &Budget, codec: Codec, nals: &[&[u8]]) -> Vec<RawAccessUnit> {
        let mut input = Vec::new();
        for nal in nals {
            input.extend_from_slice(&[0, 0, 0, 1]);
            input.extend_from_slice(nal);
        }
        read_annex_b(pool, codec, &input)
    }

    fn read_annex_b(pool: &Budget, codec: Codec, input: &[u8]) -> Vec<RawAccessUnit> {
        let bytes = Bytes::copy_from_slice(pool, input).unwrap();
        let mut reader = RawAnnexBReader::new(
            pool,
            codec,
            SOURCE,
            SourceCapabilities {
                length: Some(input.len() as u64),
                random_access: false,
            },
            RawAnnexBLimits::default(),
        )
        .unwrap();
        let mut out = Vec::new();
        for _ in 0..8192 {
            match reader.step(&mut WorkBudget::new(128), &NO).unwrap() {
                Step::Ready(unit) => out.push(unit),
                Step::NeedInput(request) => {
                    let at = request.offset as usize;
                    if at == input.len() {
                        reader.finish(request.source, request.token).unwrap();
                    } else {
                        let end = (at + request.maximum).min(input.len());
                        reader
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    bytes.slice(at..end).unwrap(),
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                Step::Yield => {}
                Step::End(_) => return out,
            }
        }
        panic!("raw reader did not finish")
    }

    #[test]
    fn hevc_active_chain_survives_id_reuse_and_pressure() {
        let pool = Budget::new(64 * 1024).unwrap();
        let vps = [64, 1, 0x28];
        let unused_vps = [64, 1, 0x18];
        let sps = hevc_sps(2, 3, 80, 64);
        let unused_sps = hevc_sps(1, 4, 64, 64);
        let pps = hevc_pps(11, 3);
        let unused_pps = hevc_pps(12, 4);
        let new_sps = hevc_sps(2, 3, 96, 80);
        let units = read(
            &pool,
            Codec::Hevc,
            &[
                &vps,
                &unused_vps,
                &sps,
                &unused_sps,
                &pps,
                &unused_pps,
                &[64, 9, 0x2f], // Enhancement-layer VPS with the same ID.
                &hevc_slice(19, 11),
                &new_sps,
                &hevc_slice(1, 11),
                &unused_pps,
            ],
        );
        assert_eq!(units.len(), 3);
        let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Hevc, 512).unwrap();
        let first = tracker.observe(&units[0]).unwrap().unwrap();
        assert_eq!((first.width, first.height), (74, 50));
        assert_eq!(first.hevc_timing(), Err(Error::Truncated));
        assert!(
            first
                .parameter_sets()
                .map(Bytes::as_ref)
                .eq([&vps[..], &sps, &pps])
        );
        assert!(first.sps.shares_storage_with(units[0].nals()[2].bytes()));
        let pressure = pool.reserve(pool.available()).unwrap();
        assert!(matches!(
            tracker.observe(&units[1]),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(tracker.sps[3].as_ref().unwrap().bytes.as_ref(), sps);
        drop(pressure);
        let second = tracker.observe(&units[1]).unwrap().unwrap();
        assert_eq!((second.width, second.height), (90, 66));
        assert_ne!(first, second);
        assert_eq!(second.sps.as_ref(), new_sps);
        assert_eq!(first.sps.as_ref(), sps);
        assert!(tracker.observe(&units[2]).unwrap().is_none());
        assert_eq!(tracker.pps[12].as_ref().unwrap().bytes.as_ref(), unused_pps);
    }

    #[test]
    fn hevc_main10_timing_after_scaling_lists_and_predicted_rps() {
        let mut b = hevc_sps_prefix(2, 3, 80, 64, 2);
        for field in [2, 2, 8] {
            b.ue(field); // Main10 bit depths and 12-bit POC.
        }
        b.fixed(1, 1); // Both sublayer ordering entries present.
        for field in [3, 1, 0, 4, 2, 0, 0, 3, 0, 3, 2, 2] {
            b.ue(field);
        }
        b.fixed(3, 2); // Custom scaling lists.
        for size in 0..4 {
            for matrix in 0..if size == 3 { 2 } else { 6 } {
                if matches!((size, matrix), (0, 0) | (3, 1)) {
                    b.fixed(1, 1);
                    if size == 3 {
                        b.ue(0); // Signed DC delta codeword.
                    }
                    for _ in 0..if size == 0 { 16 } else { 64 } {
                        b.ue(2); // Signed coefficient delta codeword.
                    }
                } else {
                    b.fixed(0, 1);
                    b.ue(matrix);
                }
            }
        }
        b.fixed(7, 3); // AMP, SAO, PCM.
        b.fixed(0x99, 8);
        b.ue(0);
        b.ue(1);
        b.fixed(0, 1);
        b.ue(4); // Four short-term sets.
        b.ue(2);
        b.ue(1);
        for delta in [0, 1, 1] {
            b.ue(delta);
            b.fixed(1, 1);
        }
        // [-1, -3, +2] shifted by +1, keeping [-2, +1, +3].
        b.fixed(0b10, 2);
        b.ue(0);
        b.fixed(0b10111, 5);
        // Shift by -1 and discard +2, leaving [-3, -1].
        b.fixed(0b11, 2);
        b.ue(0);
        b.fixed(0b101001, 6);
        // Shift by +1, retaining -2 and dropping the zero and extra delta.
        b.fixed(0b10, 2);
        b.ue(0);
        b.fixed(0b10100, 5);
        b.fixed(1, 1); // Long-term references.
        b.ue(2);
        b.fixed(0x654, 12);
        b.fixed(1, 1);
        b.fixed(0x456, 12);
        b.fixed(0, 1);
        b.fixed(7, 3); // Temporal MVP, intra smoothing, VUI present.
        b.fixed(0x1ff, 9); // Extended SAR.
        b.fixed(4, 16);
        b.fixed(3, 16);
        b.fixed(3, 2); // Overscan.
        b.fixed(0b110111, 6); // Video signal and colour description.
        b.fixed(0x01_01_01, 24);
        b.fixed(1, 1); // Chroma sample locations.
        b.ue(1);
        b.ue(2);
        b.fixed(0b1001, 4); // Neutral chroma and default display window.
        for offset in [1, 0, 0, 1] {
            b.ue(offset);
        }
        b.fixed(1, 1);
        b.fixed(1001, 32);
        b.fixed(60_000, 32);
        b.fixed(1, 1);
        b.ue(2);
        b.zeroes(3); // No HRD, bitstream restrictions, or SPS extension.
        let sps = b.nal(&[66, 1]);

        let pool = Budget::new(64 * 1024).unwrap();
        let units = read(
            &pool,
            Codec::Hevc,
            &[&[64, 1, 0x28], &sps, &hevc_pps(11, 3), &hevc_slice(19, 11)],
        );
        let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Hevc, 4096).unwrap();
        let config = tracker.observe(&units[0]).unwrap().unwrap();
        assert_eq!((config.width, config.height), (74, 50));
        assert_eq!(config.sps.as_ref(), sps);
        assert_eq!(
            config.hevc_timing().unwrap(),
            Some(HevcVuiTiming {
                num_units_in_tick: 1001,
                time_scale: 60_000,
                poc_proportional_to_timing_flag: true,
                num_ticks_poc_diff_one_minus1: Some(2),
            })
        );
    }

    fn hevc_minimal_sps_through_tools() -> Bits {
        let mut b = hevc_sps_prefix(2, 3, 80, 64, 1);
        for _ in 0..3 {
            b.ue(0);
        }
        b.fixed(0, 1); // Highest sublayer ordering entry only.
        for _ in 0..9 {
            b.ue(0);
        }
        b.zeroes(4); // No scaling lists, AMP, SAO, or PCM.
        b
    }

    #[test]
    fn hevc_absent_timing_and_malformed_prefix_are_distinct() {
        for vui_present in [0, 1] {
            let mut b = hevc_minimal_sps_through_tools();
            b.ue(0);
            b.zeroes(3); // No long-term references, temporal MVP or smoothing.
            b.fixed(vui_present, 1);
            if vui_present != 0 {
                b.zeroes(10); // No optional VUI fields or bitstream restriction.
            }
            b.fixed(0, 1); // SPS extension absent.
            let sps = b.nal(&[66, 1]);
            assert_eq!(hevc::sps_timing(&sps[2..]), Ok(None));
        }

        let mut b = hevc_minimal_sps_through_tools();
        b.ue(65);
        let too_many_sets = b.nal(&[66, 1]);
        assert!(matches!(
            hevc::sps_timing(&too_many_sets[2..]),
            Err(Error::Invalid(_))
        ));

        let mut b = hevc_minimal_sps_through_tools();
        b.ue(0);
        b.fixed(1, 4); // VUI present, other flags absent.
        b.zeroes(8);
        b.fixed(1, 1);
        b.fixed(1, 32);
        b.fixed(12, 32);
        b.fixed(1, 1);
        // A missing ue(v) field must not be read as zero from the stop bit.
        let truncated = b.nal(&[66, 1]);
        assert_eq!(hevc::sps_timing(&truncated[2..]), Err(Error::Truncated));
    }

    #[test]
    fn hevc_active_timing_matches_annex_b_fixture() {
        let pool = Budget::new(256 * 1024).unwrap();
        let input = include_bytes!("../../../framing/fixtures/raw-annex-b/hevc-aud0.hevc");
        let units = read_annex_b(&pool, Codec::Hevc, input);
        let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Hevc, 4096).unwrap();
        let mut pictures = 0;
        for unit in units {
            if let Some(config) = tracker.observe(&unit).unwrap() {
                assert_eq!(
                    config.hevc_timing().unwrap(),
                    Some(HevcVuiTiming {
                        num_units_in_tick: 1,
                        time_scale: 12,
                        poc_proportional_to_timing_flag: false,
                        num_ticks_poc_diff_one_minus1: None,
                    })
                );
                pictures += 1;
            }
        }
        assert!(pictures > 0);
    }

    fn avc_sps_prefix() -> Bits {
        let mut bits = Bits::default();
        bits.fixed(66, 8);
        bits.fixed(0, 8);
        bits.fixed(30, 8);
        for field in [0, 0, 2, 1] {
            // SPS ID, frame_num width, POC type, refs
            bits.ue(field);
        }
        bits.fixed(0, 1);
        bits.ue(3);
        bits.ue(2);
        bits.fixed(7, 3); // frame-only, direct 8x8, crop
        for crop in [1, 2, 0, 1] {
            bits.ue(crop);
        }
        bits
    }

    #[test]
    fn avc_active_sps_uses_existing_crop_parser_and_retains_exact_nals() {
        let mut bits = avc_sps_prefix();
        bits.fixed(0, 1);
        let sps = bits.nal(&[0x67]);

        let mut bits = Bits::default();
        bits.ue(2);
        bits.ue(0);
        bits.fixed(0, 2);
        for _ in 0..3 {
            bits.ue(0);
        }
        bits.fixed(0, 3);
        for _ in 0..3 {
            bits.ue(0);
        }
        bits.fixed(0, 3);
        let pps = bits.nal(&[0x68]);

        let mut bits = Bits::default();
        for field in [0, 2, 2] {
            bits.ue(field);
        }
        bits.fixed(0, 4);
        bits.ue(0);
        let idr = bits.nal(&[0x65]);

        let pool = Budget::new(64 * 1024).unwrap();
        let units = read(&pool, Codec::Avc, &[&sps, &pps, &idr]);
        let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Avc, 512).unwrap();
        let config = tracker.observe(&units[0]).unwrap().unwrap();
        assert_eq!((config.width, config.height), (58, 46));
        assert!(config.vps.is_none());
        assert_eq!(config.avc_timing, None);
        assert!(
            config
                .parameter_sets()
                .map(Bytes::as_ref)
                .eq([&sps[..], &pps])
        );
        assert_eq!(config, config.clone());
    }

    #[test]
    fn avc_vui_skips_optional_prefix_fields_and_preserves_full_width_timing() {
        let mut bits = avc_sps_prefix();
        bits.fixed(1, 1); // VUI present.
        bits.fixed(1, 1); // Aspect ratio present.
        bits.fixed(255, 8); // Extended SAR.
        bits.fixed(4, 16);
        bits.fixed(3, 16);
        bits.fixed(3, 2); // Overscan present and appropriate.
        bits.fixed(1, 1); // Video signal present.
        bits.fixed(5, 3); // Unspecified format.
        bits.fixed(3, 2); // Full range and colour description present.
        bits.fixed(0x01_01_01, 24);
        bits.fixed(1, 1); // Chroma sample locations present.
        bits.ue(1);
        bits.ue(3);
        bits.fixed(1, 1); // Timing present.
        bits.fixed(0x8000_0001, 32);
        bits.fixed(u32::MAX, 32);
        bits.fixed(1, 1);
        bits.zeroes(4); // No HRD, picture structure, or bitstream restrictions.
        let sps = bits.nal(&[0x67]);

        let (id, raster, timing) = avc::sps_with_timing(&sps[1..]).unwrap();
        assert_eq!((id, raster), avc::sps(&sps[1..]).unwrap());
        assert_eq!(
            timing,
            Some(AvcVuiTiming {
                num_units_in_tick: 0x8000_0001,
                time_scale: u32::MAX,
                fixed_frame_rate_flag: true,
            })
        );
    }

    #[test]
    fn avc_vui_distinguishes_absent_and_truncated_timing() {
        let mut bits = avc_sps_prefix();
        bits.fixed(1, 1); // VUI present.
        bits.zeroes(9); // No optional fields, including timing.
        let absent = bits.nal(&[0x67]);
        assert_eq!(avc::sps_with_timing(&absent[1..]).unwrap().2, None);

        let mut bits = avc_sps_prefix();
        bits.fixed(1, 1);
        bits.zeroes(4);
        bits.fixed(1, 1);
        bits.fixed(1001, 32);
        bits.fixed(60_000, 32);
        // The RBSP stop bit cannot stand in for a missing fixed-rate flag.
        let truncated = bits.nal(&[0x67]);
        assert_eq!(avc::sps_with_timing(&truncated[1..]), Err(Error::Truncated));
        assert_eq!(
            avc::sps(&truncated[1..]).unwrap(),
            avc::sps(&absent[1..]).unwrap()
        );
    }

    #[test]
    fn avc_active_timing_matches_annex_b_fixture() {
        let pool = Budget::new(256 * 1024).unwrap();
        let input = include_bytes!("../../tests/fixtures/raw-video/avc-80x48.h264");
        let units = read_annex_b(&pool, Codec::Avc, input);
        let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Avc, 4096).unwrap();
        let configurations: Vec<_> = units
            .iter()
            .filter_map(|unit| tracker.observe(unit).unwrap())
            .collect();
        assert!(!configurations.is_empty());
        for config in configurations {
            assert_eq!((config.width, config.height), (80, 48));
            assert_eq!(
                config.avc_timing,
                Some(AvcVuiTiming {
                    num_units_in_tick: 1,
                    time_scale: 16,
                    fixed_frame_rate_flag: false,
                })
            );
        }
    }

    #[test]
    fn invalid_chain_and_foreign_pool_leave_tracker_unbound() {
        let pool = Budget::new(64 * 1024).unwrap();
        let other = Budget::new(64 * 1024).unwrap();
        let sps = hevc_sps(2, 3, 80, 64);
        let pps = hevc_pps(11, 3);
        let slice = hevc_slice(19, 11);
        let units = read(&pool, Codec::Hevc, &[&sps, &pps, &slice]);
        let mut tracker = RawVideoConfigTracker::new(&pool, Codec::Hevc, 512).unwrap();
        assert_eq!(
            tracker.observe(&units[0]).err(),
            Some(Error::Invalid("raw HEVC picture has no active VPS"))
        );
        assert!(tracker.source.is_none() && tracker.sps[3].is_none() && tracker.pps[11].is_none());
        let units = read(&other, Codec::Hevc, &[&[64, 1, 0x28], &sps, &pps, &slice]);
        assert_eq!(tracker.observe(&units[0]).err(), Some(Error::ForeignPool));
        let units = read(&pool, Codec::Hevc, &[&[64, 1, 0x28], &sps, &pps, &slice]);
        let mut limited = RawVideoConfigTracker::new(&pool, Codec::Hevc, 4).unwrap();
        assert_eq!(
            limited.observe(&units[0]).err(),
            Some(Error::Limit {
                requested: sps.len(),
                maximum: 4,
            })
        );
        assert!(limited.vps[2].is_none());
    }
}
