//! Explicit public receiver time view. Source/demux/index clocks stay unchanged.
//! This pin-bound compatibility view describes a public receiver operation, not
//! ISO clock semantics. Index construction and packet selection use demux facts.
use crate::{Mov9ClockField, Mov9PacketView};
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32PublicReceiverV6;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9ReceiverOriginV6 {
    DemuxValue,
    ApiSentinel,
    PublicRelativeReference,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9PublicClockV6 {
    demux: Mov9ClockField,
    returned: i64,
    origin: Mov9ReceiverOriginV6,
}
impl Mov9PublicClockV6 {
    /// Classify by distance below the signed ceiling. The eligible region has
    /// width 2^49; its returned origin is halfway across that region. Widening
    /// first keeps negative values and the API sentinel outside it without
    /// signed subtraction overflow. The retained demux field is never changed.
    pub fn from_demux(
        demux: Mov9ClockField,
        _profile: PinnedMovD32PublicReceiverV6,
    ) -> Result<Self> {
        let raw = demux.signed_ticks();
        let half_width = 1i128 << 48;
        let distance = i128::from(i64::MAX) - i128::from(raw);
        let (returned, origin) = if distance < 2 * half_width {
            let value = i64::try_from(half_width - distance).map_err(|_| Error::Overflow)?;
            (value, Mov9ReceiverOriginV6::PublicRelativeReference)
        } else if raw == i64::MIN {
            (raw, Mov9ReceiverOriginV6::ApiSentinel)
        } else {
            (raw, Mov9ReceiverOriginV6::DemuxValue)
        };
        Ok(Self {
            demux,
            returned,
            origin,
        })
    }
    pub fn demux_field(self) -> Mov9ClockField {
        self.demux
    }
    pub fn public_raw_ticks(self) -> i64 {
        self.returned
    }
    pub fn public_api_ticks(self) -> Option<i64> {
        (self.returned != i64::MIN).then_some(self.returned)
    }
    pub fn origin(self) -> Mov9ReceiverOriginV6 {
        self.origin
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mov9PublicPacketTimesV6 {
    pub pts: Mov9PublicClockV6,
    pub dts: Mov9PublicClockV6,
}
impl Mov9PublicPacketTimesV6 {
    pub fn from_demux(
        view: &Mov9PacketView,
        profile: PinnedMovD32PublicReceiverV6,
    ) -> Result<Self> {
        Ok(Self {
            pts: Mov9PublicClockV6::from_demux(view.pts_field, profile)?,
            dts: Mov9PublicClockV6::from_demux(view.dts_field, profile)?,
        })
    }
}
