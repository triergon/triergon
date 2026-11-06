//! The bitmap subtitle clock for an explicitly buffered stream-probe prefix.
use super::{
    MatroskaBitmapSource, MatroskaBitmapTrack, MatroskaDvbPacket,
    probe_clock::{NativePacketTiming, PacketClock, ProbeTimeline, Timeline},
};
use crate::{Output, Result, bitmap_bridge::BitmapInput};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Error, WorkBudget};

struct Row {
    output: usize,
    clock: PacketClock,
}

/// Project the packet clocks for one selected bitmap track over a caller-owned
/// stream-probe buffer. The caller supplies the actual probe boundary, in
/// output order, before any packet in it reaches the decoder.
pub struct MatroskaBitmapClockProbe<'track, 'prefix> {
    track: &'track MatroskaBitmapTrack,
    prefix: &'prefix [Output],
    rows: BudgetVec<Row>,
    cursor: usize,
    clock: ProbeTimeline,
}

impl<'track, 'prefix> MatroskaBitmapClockProbe<'track, 'prefix> {
    pub fn new(
        track: &'track MatroskaBitmapTrack,
        prefix: &'prefix [Output],
        maximum_outputs: usize,
    ) -> Result<Self> {
        if prefix.len() > maximum_outputs {
            return Err(Error::Limit {
                requested: prefix.len(),
                maximum: maximum_outputs,
            });
        }
        Ok(Self {
            track,
            prefix,
            rows: BudgetVec::new(track.pool()),
            cursor: 0,
            clock: ProbeTimeline::default(),
        })
    }

    /// Scan one buffered output per unit. A pressure refusal leaves the
    /// current output and the running clock available for retry.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let Some(output) = self.prefix.get(self.cursor) else {
                return Ok(true);
            };
            if !work.try_charge(1) {
                return Ok(false);
            }
            if let Some(packet) = self.track.selected_packet(output)? {
                let mut next = self.clock;
                let clock = next.push(self.rows.len(), NativePacketTiming::demux(packet)?)?;
                self.rows
                    .try_push(Row {
                        output: self.cursor,
                        clock,
                    })
                    .map_err(|failure| failure.error)?;
                self.clock = next;
            }
            self.cursor += 1;
        }
    }

    fn ready(&self) -> Result<()> {
        if self.cursor == self.prefix.len() {
            Ok(())
        } else {
            Err(Error::Invalid("Matroska bitmap clock probe pending"))
        }
    }

    /// Replay an original output by index after the clock scan completes.
    /// Its source clocks and delivery classification stay with the packet.
    pub fn packet(
        &self,
        output_index: usize,
        sequence: u64,
    ) -> Result<Option<BitmapInput<MatroskaBitmapSource>>> {
        self.ready()?;
        let output = self
            .prefix
            .get(output_index)
            .ok_or(Error::Invalid("Matroska probe output index"))?;
        let Ok(index) = self
            .rows
            .as_slice()
            .binary_search_by_key(&output_index, |row| row.output)
        else {
            return Ok(None);
        };
        let packet = self
            .track
            .selected_packet(output)?
            .ok_or(Error::SourceMismatch)?;
        self.track
            .make_input(
                packet,
                output.delivery(),
                sequence,
                self.clock
                    .resolve(index, self.rows.as_slice()[index].clock)?,
            )
            .map(Some)
    }

    pub fn dvb_packet(
        &self,
        output_index: usize,
        sequence: u64,
    ) -> Result<Option<MatroskaDvbPacket>> {
        self.track.check_dvb()?;
        self.packet(output_index, sequence)?
            .map(|input| self.track.bind_dvb(input))
            .transpose()
    }

    /// Release the prefix and retain the forward clock after replaying it.
    pub fn into_live(self) -> Result<MatroskaBitmapLiveClock<'track>> {
        self.ready()?;
        Ok(MatroskaBitmapLiveClock {
            track: self.track,
            timeline: self.clock.into_live(),
        })
    }
}

/// Continue the selected clock in packet order after releasing the probe
/// buffer. A seek or configuration change needs a new track binding.
pub struct MatroskaBitmapLiveClock<'track> {
    track: &'track MatroskaBitmapTrack,
    timeline: Timeline,
}

impl MatroskaBitmapLiveClock<'_> {
    fn prepare(
        &self,
        output: &Output,
        sequence: u64,
    ) -> Result<Option<(BitmapInput<MatroskaBitmapSource>, Timeline)>> {
        let Some(packet) = self.track.selected_packet(output)? else {
            return Ok(None);
        };
        let mut next = self.timeline;
        let clock = next.push(NativePacketTiming::demux(packet)?)?;
        let clock = next.resolve(clock)?;
        Ok(Some((
            self.track
                .make_input(packet, output.delivery(), sequence, clock)?,
            next,
        )))
    }

    pub fn packet(
        &mut self,
        output: &Output,
        sequence: u64,
    ) -> Result<Option<BitmapInput<MatroskaBitmapSource>>> {
        let Some((packet, next)) = self.prepare(output, sequence)? else {
            return Ok(None);
        };
        self.timeline = next;
        Ok(Some(packet))
    }

    pub fn dvb_packet(
        &mut self,
        output: &Output,
        sequence: u64,
    ) -> Result<Option<MatroskaDvbPacket>> {
        self.track.check_dvb()?;
        let Some((packet, next)) = self.prepare(output, sequence)? else {
            return Ok(None);
        };
        let packet = self.track.bind_dvb(packet)?;
        self.timeline = next;
        Ok(Some(packet))
    }
}
