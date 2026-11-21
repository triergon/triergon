//! The AVC access units and AAC frames of the readers through [`PacketInfo`], with
//! the track they were read from.
//!
//! An HEVC picture of a media segment is several slices of the reader's input and has
//! no single payload slice, so it has no packet view; its `contiguous_feed` copies it.
use crate::{aac::HlsAacFrame, avc::AvcTsAccessUnit};
use triergon_core::container::{PacketInfo, RandomAccess, TrackId};
use triergon_core::{Error, Result, Rounding, Time, TimeBase};

const TS_CLOCK_RATE: u64 = 90_000;

/// One AVC access unit or AAC frame with its track and times, borrowed from the unit.
/// Video times are ticks of the 90 kHz clock. Audio times are ticks of the finest
/// clock both the 90 kHz PES times and the frame's sample duration count exactly in.
#[derive(Clone, Copy, Debug)]
pub struct UnitPacket<'a> {
    track: TrackId,
    payload: &'a [u8],
    time_base: TimeBase,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    random_access: RandomAccess,
}

impl<'a> UnitPacket<'a> {
    /// An AVC access unit of PID `pid`: its Annex B bytes as the stream carries them,
    /// and its PES times. An IDR unit is a random-access point; for another the stream
    /// does not say.
    pub(crate) fn video(pid: u16, unit: &'a AvcTsAccessUnit) -> Result<Self> {
        Ok(Self {
            track: TrackId(u64::from(pid)),
            payload: unit.source_bytes().as_ref(),
            time_base: TimeBase::new(1, TS_CLOCK_RATE)?,
            pts: unit.clock.and_then(|clock| clock.pts90),
            dts: unit.clock.and_then(|clock| clock.dts90),
            duration: None,
            random_access: if unit.idr {
                RandomAccess::Sync
            } else {
                RandomAccess::Unknown
            },
        })
    }

    /// An ADTS frame of PID `pid` presented at `pts`, decoded at `dts`.
    pub(crate) fn audio(
        pid: u16,
        frame: &'a HlsAacFrame,
        pts: Time,
        dts: Option<Time>,
    ) -> Result<Self> {
        let samples = u64::try_from(frame.duration.denominator()).map_err(|_| Error::Overflow)?;
        let lcm = TS_CLOCK_RATE
            .checked_mul(samples / gcd(TS_CLOCK_RATE, samples))
            .ok_or(Error::Overflow)?;
        let time_base = TimeBase::new(1, lcm)?;
        let ticks = |time: Time| time.to_i64_ticks(time_base, Rounding::Floor);
        Ok(Self {
            track: TrackId(u64::from(pid)),
            payload: frame.bytes.as_ref(),
            time_base,
            pts: Some(ticks(pts)?),
            dts: dts.map(ticks).transpose()?,
            duration: Some(ticks(frame.duration)?),
            random_access: RandomAccess::Unknown,
        })
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl PacketInfo for UnitPacket<'_> {
    fn track(&self) -> TrackId {
        self.track
    }
    fn payload(&self) -> &[u8] {
        self.payload
    }
    fn time_base(&self) -> TimeBase {
        self.time_base
    }
    fn pts(&self) -> Option<i64> {
        self.pts
    }
    fn dts(&self) -> Option<i64> {
        self.dts
    }
    fn duration(&self) -> Option<i64> {
        self.duration
    }
    fn random_access(&self) -> RandomAccess {
        self.random_access
    }
}
