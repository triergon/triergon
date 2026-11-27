//! Packet times as FFmpeg's public API returns them.
//!
//! [`ReceiverClock`] pairs a demuxer clock field with the value the API returns for it,
//! which differs near the top of the signed range. The demuxer fields, and the index and
//! packet order built from them, are unchanged. This is not an ISO clock rule.
use crate::compat::mov::{clocks::ClockField, view::PacketView};
use triergon_core::{Error, Result};

/// Selects the API's timestamp form; pass it to [`ReceiverClock::from_demux`] or
/// [`ReceiverPacketTimes::from_demux`].
#[derive(Clone, Copy, Debug)]
pub struct ReceiverClockProfile;
/// Which form of the demuxer's field the API returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverOrigin {
    /// The demuxer's field, unchanged.
    DemuxValue,
    /// `i64::MIN`, the API's no-timestamp value, unchanged.
    ApiSentinel,
    /// A field less than 2^49 ticks below `i64::MAX`, returned with `i64::MAX - 2^48`
    /// subtracted, so in (-2^48, 2^48].
    PublicRelativeReference,
}
/// One packet time as FFmpeg's public API returns it, beside the demuxer's field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverClock {
    demux: ClockField,
    returned: i64,
    origin: ReceiverOrigin,
}
impl ReceiverClock {
    /// Works out the API's value for `demux`: a field less than 2^49 ticks below
    /// `i64::MAX` comes back with `i64::MAX - 2^48` subtracted, and any other field,
    /// `i64::MIN` included, unchanged.
    pub fn from_demux(demux: ClockField, _profile: ReceiverClockProfile) -> Result<Self> {
        let raw = demux.signed_ticks();
        let half_width = 1i128 << 48;
        let distance = i128::from(i64::MAX) - i128::from(raw);
        let (returned, origin) = if distance < 2 * half_width {
            let value = i64::try_from(half_width - distance).map_err(|_| Error::Overflow)?;
            (value, ReceiverOrigin::PublicRelativeReference)
        } else if raw == i64::MIN {
            (raw, ReceiverOrigin::ApiSentinel)
        } else {
            (raw, ReceiverOrigin::DemuxValue)
        };
        Ok(Self {
            demux,
            returned,
            origin,
        })
    }
    /// The demuxer's field, unchanged.
    pub fn demux_field(self) -> ClockField {
        self.demux
    }
    /// The value the API returns, in ticks of the stream time base; `i64::MIN` for no
    /// timestamp.
    pub fn public_raw_ticks(self) -> i64 {
        self.returned
    }
    /// The value the API returns, in ticks of the stream time base; `None` for no
    /// timestamp.
    pub fn public_api_ticks(self) -> Option<i64> {
        (self.returned != i64::MIN).then_some(self.returned)
    }
    /// Which form of the demuxer's field the API returns.
    pub fn origin(self) -> ReceiverOrigin {
        self.origin
    }
}
/// A packet's presentation and decode times as FFmpeg's public API returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverPacketTimes {
    /// Presentation time, from the packet's `pts_field`.
    pub pts: ReceiverClock,
    /// Decode time, from the packet's `dts_field`.
    pub dts: ReceiverClock,
}
impl ReceiverPacketTimes {
    /// Works out the API's values for `view`'s `pts_field` and `dts_field`.
    pub fn from_demux(view: &PacketView, profile: ReceiverClockProfile) -> Result<Self> {
        Ok(Self {
            pts: ReceiverClock::from_demux(view.pts_field, profile)?,
            dts: ReceiverClock::from_demux(view.dts_field, profile)?,
        })
    }
}
