//! Shared native initial-packet timing over a bounded probe prefix.
use crate::Result;
use triergon_core::Error;
use triergon_matroska::typed::Packet;

#[derive(Clone, Copy)]
pub(super) struct NativePacketTiming {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration_ns: Option<u64>,
}

impl NativePacketTiming {
    pub fn demux(packet: &Packet) -> Result<Self> {
        let timing = packet.packet.timing.ffmpeg9()?;
        Ok(Self {
            pts: timing
                .pts
                .map(triergon_matroska::timing::ExactTime::rounded_ns)
                .transpose()?,
            dts: None,
            duration_ns: timing
                .duration
                .map(triergon_matroska::timing::ExactTime::rounded_duration_ns)
                .transpose()?,
        })
    }
}

#[derive(Clone, Copy)]
enum Stamp {
    Relative(i128),
    Absolute(i128),
}

impl Stamp {
    fn offset(self, delta: i128) -> Result<Self> {
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
pub(super) struct PacketClock {
    stamp: Option<Stamp>,
    duration_ns: Option<u64>,
}

impl PacketClock {
    fn resolve(self, shift: Option<i128>) -> Result<NativePacketTiming> {
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
        Ok(NativePacketTiming {
            pts,
            dts: pts,
            duration_ns: self.duration_ns,
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Timeline {
    next: i128,
    /// First absolute clock minus the relative elapsed time at that packet.
    shift: Option<i128>,
}

impl Timeline {
    pub fn push(&mut self, timing: NativePacketTiming) -> Result<PacketClock> {
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

    pub fn resolve(self, clock: PacketClock) -> Result<NativePacketTiming> {
        clock.resolve(self.shift)
    }
}

#[derive(Clone, Copy)]
struct InitialDuration {
    packets: usize,
    first: Stamp,
    duration_ns: u64,
}

impl InitialDuration {
    fn offset(self, index: usize) -> Result<i128> {
        i128::from(self.duration_ns)
            .checked_mul(index as i128)
            .ok_or(Error::Overflow)
    }

    fn clock(self, index: usize) -> Result<PacketClock> {
        Ok(PacketClock {
            stamp: Some(self.first.offset(self.offset(index)?)?),
            duration_ns: Some(self.duration_ns),
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct ProbeTimeline {
    timeline: Timeline,
    leading: usize,
    untimed: usize,
    seen_duration: bool,
    fill: Option<InitialDuration>,
}

impl ProbeTimeline {
    pub fn push(&mut self, count: usize, timing: NativePacketTiming) -> Result<PacketClock> {
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

    pub fn resolve(self, index: usize, original: PacketClock) -> Result<NativePacketTiming> {
        let clock = match self.fill.filter(|fill| index < fill.packets) {
            Some(fill) => fill.clock(index)?,
            None => original,
        };
        self.timeline.resolve(clock)
    }

    pub fn into_live(self) -> Timeline {
        self.timeline
    }
}

pub(super) fn syncframe_duration_ns(samples: u16, sample_rate: u32, scale_ns: u64) -> Option<u64> {
    let span_ns = u64::from(samples) * 1_000_000_000 / u64::from(sample_rate);
    // Flooring twice gives the same whole tick count without multiplying
    // the sample rate by an arbitrary container scale.
    let ticks = span_ns.checked_div(scale_ns)?;
    (ticks != 0).then_some(ticks * scale_ns)
}
