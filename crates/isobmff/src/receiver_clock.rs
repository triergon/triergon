//! Explicit public receiver time view. Source/demux/index clocks stay unchanged.
//! This pin-bound compatibility view describes a public receiver operation, not
//! ISO clock semantics. Index construction and packet selection use demux facts.
use crate::{ClockField, PacketView};
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub struct ReceiverClockProfile;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverOrigin {
    DemuxValue,
    ApiSentinel,
    PublicRelativeReference,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverClock {
    demux: ClockField,
    returned: i64,
    origin: ReceiverOrigin,
}
impl ReceiverClock {
    /// Classify by distance below the signed ceiling. The eligible region has
    /// width 2^49; its returned origin is halfway across that region. Widening
    /// first keeps negative values and the API sentinel outside it without
    /// signed subtraction overflow. The retained demux field is never changed.
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
    pub fn demux_field(self) -> ClockField {
        self.demux
    }
    pub fn public_raw_ticks(self) -> i64 {
        self.returned
    }
    pub fn public_api_ticks(self) -> Option<i64> {
        (self.returned != i64::MIN).then_some(self.returned)
    }
    pub fn origin(self) -> ReceiverOrigin {
        self.origin
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverPacketTimes {
    pub pts: ReceiverClock,
    pub dts: ReceiverClock,
}
impl ReceiverPacketTimes {
    pub fn from_demux(view: &PacketView, profile: ReceiverClockProfile) -> Result<Self> {
        Ok(Self {
            pts: ReceiverClock::from_demux(view.pts_field, profile)?,
            dts: ReceiverClock::from_demux(view.dts_field, profile)?,
        })
    }
}
