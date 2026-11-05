//! The bitmap subtitle clock for an explicitly buffered stream-probe prefix.
use super::{MatroskaBitmapSource, MatroskaBitmapTrack, MatroskaDvbPacket, bitmap::BitmapTiming};
use crate::{Output, Result, bitmap_bridge::BitmapInput};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Error, WorkBudget};

#[derive(Clone, Copy)]
enum Stamp {
    Relative(i128),
    Absolute(i128),
}

impl Stamp {
    fn offset(self, delta: i128) -> triergon_core::Result<Self> {
        Ok(match self {
            Self::Relative(value) => {
                Self::Relative(value.checked_add(delta).ok_or(Error::Overflow)?)
            }
            Self::Absolute(value) => {
                Self::Absolute(value.checked_add(delta).ok_or(Error::Overflow)?)
            }
        })
    }
}

#[derive(Clone, Copy)]
struct PacketClock {
    stamp: Option<Stamp>,
    duration_ns: Option<u64>,
}

impl PacketClock {
    fn resolve(self, shift: Option<i128>) -> triergon_core::Result<BitmapTiming> {
        let pts = match self.stamp {
            None => None,
            Some(Stamp::Absolute(value)) => Some(value),
            Some(Stamp::Relative(value)) => Some(
                value
                    .checked_add(shift.unwrap_or(0))
                    .ok_or(Error::Overflow)?,
            ),
        }
        .map(i64::try_from)
        .transpose()
        .map_err(|_| Error::Overflow)?;
        Ok(BitmapTiming {
            pts,
            dts: pts,
            duration_ns: self.duration_ns,
        })
    }
}

#[derive(Clone, Copy, Default)]
struct Timeline {
    next: i128,
    /// First absolute clock minus the relative elapsed time at that packet.
    shift: Option<i128>,
}

impl Timeline {
    fn push(&mut self, timing: BitmapTiming) -> triergon_core::Result<PacketClock> {
        let stamp = if let Some(pts) = timing.pts {
            let pts = i128::from(pts);
            if self.shift.is_none() {
                self.shift = Some(pts.checked_sub(self.next).ok_or(Error::Overflow)?);
            }
            self.next = pts;
            Some(Stamp::Absolute(pts))
        } else if timing.duration_ns.is_some() {
            Some(if self.shift.is_some() {
                Stamp::Absolute(self.next)
            } else {
                Stamp::Relative(self.next)
            })
        } else {
            None
        };
        if let Some(duration) = timing.duration_ns {
            self.next = self
                .next
                .checked_add(i128::from(duration))
                .ok_or(Error::Overflow)?;
        }
        Ok(PacketClock {
            stamp,
            duration_ns: timing.duration_ns,
        })
    }
}

#[derive(Clone, Copy)]
struct InitialDuration {
    packets: usize,
    first: Stamp,
    duration_ns: u64,
}

impl InitialDuration {
    fn offset(self, index: usize) -> triergon_core::Result<i128> {
        i128::from(self.duration_ns)
            .checked_mul(index as i128)
            .ok_or(Error::Overflow)
    }

    fn clock(self, index: usize) -> triergon_core::Result<PacketClock> {
        Ok(PacketClock {
            stamp: Some(self.first.offset(self.offset(index)?)?),
            duration_ns: Some(self.duration_ns),
        })
    }
}

#[derive(Clone, Copy, Default)]
struct ProbeTimeline {
    timeline: Timeline,
    leading: usize,
    untimed: usize,
    seen_duration: bool,
    fill: Option<InitialDuration>,
}

impl ProbeTimeline {
    fn push(&mut self, count: usize, timing: BitmapTiming) -> triergon_core::Result<PacketClock> {
        if !self.seen_duration {
            if let Some(duration_ns) = timing.duration_ns {
                self.seen_duration = true;
                // A single prefix interval captures backfill. Once a duration
                // reaches the stream, that interval cannot be extended later.
                if self.leading != 0 {
                    let mut fill = InitialDuration {
                        packets: self.leading,
                        first: Stamp::Relative(0),
                        duration_ns,
                    };
                    if let Some(anchor) = self.timeline.shift {
                        fill.first = Stamp::Absolute(
                            anchor
                                .checked_sub(fill.offset(self.untimed)?)
                                .ok_or(Error::Overflow)?,
                        );
                        // Backdating an existing anchor stops at the packet
                        // that supplied it. An anchor at the head stays
                        // eligible for duration fill.
                        if self.untimed != 0 {
                            fill.packets = self.untimed;
                        }
                    }
                    if fill.packets == count {
                        self.timeline.next = match fill.first.offset(fill.offset(count)?)? {
                            Stamp::Relative(value) | Stamp::Absolute(value) => value,
                        };
                    }
                    self.fill = Some(fill);
                }
            } else if self.leading == count
                && timing.pts.is_none_or(|pts| {
                    self.timeline
                        .shift
                        .is_none_or(|anchor| anchor == i128::from(pts))
                })
            {
                self.leading += 1;
                if self.timeline.shift.is_none() && timing.pts.is_none() {
                    self.untimed += 1;
                }
            }
        }
        self.timeline.push(timing)
    }
}

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
            }
            .into());
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
                return Err(Error::Cancelled.into());
            }
            let Some(output) = self.prefix.get(self.cursor) else {
                return Ok(true);
            };
            if !work.try_charge(1) {
                return Ok(false);
            }
            if let Some(packet) = self.track.selected_packet(output)? {
                let mut next = self.clock;
                let clock = next.push(self.rows.len(), BitmapTiming::demux(packet)?)?;
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
            Err(Error::Invalid("Matroska bitmap clock probe pending").into())
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
        let clock = match self.clock.fill.filter(|fill| index < fill.packets) {
            Some(fill) => fill.clock(index)?,
            None => self.rows.as_slice()[index].clock,
        };
        self.track
            .make_input(
                packet,
                output.delivery(),
                sequence,
                clock.resolve(self.clock.timeline.shift)?,
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
            timeline: self.clock.timeline,
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
        let clock = next
            .push(BitmapTiming::demux(packet)?)?
            .resolve(next.shift)?;
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
