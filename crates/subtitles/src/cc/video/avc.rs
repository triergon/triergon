//! The H.264 parameter sets and slice header fields that decide picture output,
//! read by `triergon_framing`'s H.264 reader, and kept per id.

use super::nal::{LastPayload, Timing};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::avc::{
    self, AvcPocSyntax, AvcPps, AvcReading, AvcSliceDepth, AvcSliceHeader, AvcSps,
};

/// The largest `max_num_reorder_frames` of a sequence the reference takes; an
/// SPS above it is not taken (case `h264-reorder-ts`, probe p15).
const MOST_REORDER_FRAMES: u32 = 16;

/// A sequence parameter set and the cycle of its type 1 order counts.
#[derive(Debug)]
pub(crate) struct Sps {
    pub fields: AvcSps,
    pub cycle: BudgetVec<i32>,
}

impl Sps {
    /// The VUI frame period's clock.
    pub(crate) fn timing(&self) -> Option<Timing> {
        self.fields
            .vui
            .timing
            .map(|(num_units_in_tick, time_scale)| Timing {
                num_units_in_tick,
                time_scale,
            })
    }
}

/// The fields of a picture's first slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slice {
    pub idr: bool,
    pub reference: bool,
    pub sps_id: u8,
    pub header: AvcSliceHeader,
}

/// The parameter sets received, by id.
#[derive(Debug)]
pub(crate) struct Tables {
    pool: Budget,
    sps: BudgetVec<Option<Sps>>,
    pps: BudgetVec<Option<AvcPps>>,
    last_sps: LastPayload,
    last_pps: LastPayload,
}

/// The slot of set `id` in `table`, grown to it.
fn slot<T>(table: &mut BudgetVec<Option<T>>, id: u8) -> Result<&mut Option<T>> {
    let at = usize::from(id);
    if table.len() <= at {
        table.try_extend_with(at + 1 - table.len(), |_| None)?;
    }
    Ok(&mut table[at])
}

impl Tables {
    pub(crate) fn new(pool: &Budget) -> Self {
        Self {
            pool: pool.clone(),
            sps: BudgetVec::new(pool),
            pps: BudgetVec::new(pool),
            last_sps: LastPayload::new(pool),
            last_pps: LastPayload::new(pool),
        }
    }

    pub(crate) fn sps(&self, id: u8) -> Option<&Sps> {
        self.sps.get(usize::from(id))?.as_ref()
    }

    /// Reads an SPS NAL unit payload (after the header) and stores it,
    /// returning its id and the set it replaces; a repeated set changes
    /// nothing. An SPS cut short, out of range or not taken by the reference
    /// is ignored.
    pub(crate) fn put_sps(&mut self, payload: &[u8]) -> Result<Option<(u8, Option<Sps>)>> {
        if self.last_sps.repeats(payload) {
            return Ok(None);
        }
        self.last_sps.remember(payload)?;
        let mut offsets = [0i32; 255];
        let Ok(fields) = avc::read_sps(payload, AvcReading::Lenient, true, |at, offset| {
            offsets[at] = offset;
        }) else {
            return Ok(None);
        };
        if fields
            .vui
            .max_num_reorder_frames
            .is_some_and(|reorder| reorder > MOST_REORDER_FRAMES)
        {
            return Ok(None);
        }
        let len = match fields.poc {
            AvcPocSyntax::Cycle { len, .. } => usize::from(len),
            _ => 0,
        };
        let mut cycle = BudgetVec::new(&self.pool);
        cycle.try_extend_from_slice(&offsets[..len])?;
        let slot = slot(&mut self.sps, fields.id)?;
        Ok(Some((fields.id, slot.replace(Sps { fields, cycle }))))
    }

    /// Reads a PPS NAL unit payload and stores it, returning its id and the
    /// set it replaces.
    pub(crate) fn put_pps(&mut self, payload: &[u8]) -> Result<Option<(u8, Option<AvcPps>)>> {
        if self.last_pps.repeats(payload) {
            return Ok(None);
        }
        self.last_pps.remember(payload)?;
        let Ok(pps) = avc::read_pps(payload, AvcReading::Lenient) else {
            return Ok(None);
        };
        Ok(Some((pps.id, slot(&mut self.pps, pps.id)?.replace(pps))))
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

    pub(crate) fn restore_pps(&mut self, id: u8, old: Option<AvcPps>) {
        if let Some(slot) = self.pps.get_mut(usize::from(id)) {
            *slot = old;
        }
    }

    /// The fields of a slice NAL unit (`nal_unit_type` 1 or 5), `None` when
    /// its parameter sets are missing or it is cut short.
    pub(crate) fn slice(&self, nal: &[u8]) -> Option<Slice> {
        let (&header, payload) = nal.split_first()?;
        let idr = header & 0x1f == 5;
        let reference = header & 0x60 != 0;
        let pps_id = avc::slice_pps_id(payload, AvcReading::Lenient).ok()?;
        let pps = self.pps.get(usize::from(pps_id))?.as_ref()?;
        let sps = self.sps(pps.sps_id)?;
        let header = avc::read_slice_header(
            payload,
            reference,
            idr,
            &sps.fields,
            pps,
            AvcReading::Lenient,
            AvcSliceDepth::Marking,
        )
        .ok()?;
        Some(Slice {
            idr,
            reference,
            sps_id: pps.sps_id,
            header,
        })
    }
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
            sps.timing(),
            Some(Timing {
                num_units_in_tick: 1001,
                time_scale: 60000
            })
        );
        assert_eq!(sps.fields.poc, AvcPocSyntax::Lsb { bits: 5 });
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
