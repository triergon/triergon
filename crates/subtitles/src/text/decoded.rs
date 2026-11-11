//! What FFmpeg's `avcodec_decode_subtitle2` returns for a text subtitle
//! packet: the clock it gives the subtitle and its one ASS rectangle.

use triergon_core::{Bytes, Error, Result, Rounding, TimeBase};

/// The packet's clock, in packet time base ticks. Unknown and a known zero
/// duration stay distinct.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
}

/// A decoded subtitle: its clock, and the ASS text of its one rectangle,
/// whose geometry and flags are zero.
#[derive(Debug)]
pub struct Subtitle {
    pub pts_microseconds: Option<i64>,
    pub start_display_ms: u32,
    pub end_display_ms: u32,
    pub ass: Bytes,
}

impl Subtitle {
    /// A subtitle of `ass` with the times the decode call gives a packet at
    /// `clock` in `packet_timebase`: the PTS in microseconds when both are
    /// known, and the duration in milliseconds, kept to 32 bits, as the end of
    /// the display.
    pub(crate) fn at(ass: Bytes, clock: Clock, packet_timebase: Option<TimeBase>) -> Result<Self> {
        let pts = clock
            .pts
            .filter(|&pts| pts != i64::MIN)
            .and_then(|pts| packet_timebase.map(|base| rescale(pts, base, 1_000_000)))
            .filter(|&pts| pts != i64::MIN);
        let duration = clock
            .duration
            .filter(|&duration| duration != 0)
            .and_then(|duration| packet_timebase.map(|base| rescale(duration, base, 1000)))
            .unwrap_or(0);
        let end =
            u32::try_from((duration as u64) & u64::from(u32::MAX)).map_err(|_| Error::Overflow)?;
        Ok(Self {
            pts_microseconds: pts,
            start_display_ms: 0,
            end_display_ms: end,
            ass,
        })
    }

    /// The subtitle format: text.
    pub const fn format(&self) -> u16 {
        1
    }
    /// The rectangle type: ASS.
    pub const fn rectangle_type(&self) -> u8 {
        3
    }
    pub const fn geometry(&self) -> [i32; 4] {
        [0; 4]
    }
    pub const fn flags(&self) -> u32 {
        0
    }
    pub const fn palette_entries(&self) -> u32 {
        0
    }
}

/// `ticks` in `base` as whole `1/denominator` units, rounded to nearest with
/// ties away from zero; `i64::MIN` when it does not fit.
fn rescale(ticks: i64, base: TimeBase, denominator: u64) -> i64 {
    TimeBase::new(1, denominator)
        .and_then(|to| base.rescale(ticks, to, Rounding::NearestAway))
        .unwrap_or(i64::MIN)
}
