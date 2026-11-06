//! Separately named observed parser-position view, never physical source facts.
//! Default-flags public parser operation of the reference named on
//! [`crate::compat::dts`]; no FF implementation used.
//! The explicit-flush hint is an observed signed parser hint, not a source end,
//! syntax validation, a generated timestamp, or a generic DTS/FFmpeg promise.
use crate::{Dts14CarrierBoundary, Dts14CarrierPacket};
use triergon_core::{Error, Result};
/// Identifies this parser-position model. The value is stable and does not follow
/// the item's name.
pub const PARSER_HINT_PROFILE: &str = "d32b387-default-raw14-parser-positions-v1";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserHintOrigin {
    FollowingSync,
    ExplicitFlushLookbehind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserHints {
    pub frame_offset: i64,
    pub consumed_offset: i64,
    pub opaque_next_offset: i64,
    pub origin: ParserHintOrigin,
}
impl ParserHints {
    /// Explicit opt-in to this observed operation. The source packet's physical
    /// identity/range/boundary remain unchanged. No validated-scope token exists.
    /// Fresh native context at source0 and contiguous default-flags input windows
    /// are required by the operation; reset/reopen equivalence is not asserted.
    pub fn observe(packet: &Dts14CarrierPacket) -> Result<Self> {
        let start = packet.identity.offset;
        let end = start
            .checked_add(u64::try_from(packet.source_bytes().len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let p = packet.provision;
        if p.last_window_start > p.last_window_end || p.consumed_through > p.last_window_end {
            return Err(Error::Invalid("DTS parser hint provision bounds"));
        }
        let (consumed, next, origin) = match packet.boundary {
            Dts14CarrierBoundary::FollowingCarrierSync { offset } => {
                if offset != end
                    || p.consumed_through != end.checked_add(6).ok_or(Error::Overflow)?
                {
                    return Err(Error::Invalid("DTS parser hint following-sync evidence"));
                }
                // Public consumption is nonnegative. Sync bytes already consumed
                // in earlier windows cannot be subtracted from that API counter.
                (
                    end.max(p.last_window_start),
                    i64::try_from(end).map_err(|_| Error::Overflow)?,
                    ParserHintOrigin::FollowingSync,
                )
            }
            Dts14CarrierBoundary::ExplicitSourceEnd => {
                if p.consumed_through != end {
                    return Err(Error::Invalid("DTS parser hint explicit-flush evidence"));
                }
                // Observed opaque reference EOF hint; varying-geometry probes are mandatory. Signed:
                // small sources produce negative values. Never a SourceLocation.
                (
                    end,
                    i64::try_from(end)
                        .map_err(|_| Error::Overflow)?
                        .checked_sub(100)
                        .ok_or(Error::Overflow)?,
                    ParserHintOrigin::ExplicitFlushLookbehind,
                )
            }
        };
        Ok(Self {
            frame_offset: i64::try_from(start).map_err(|_| Error::Overflow)?,
            consumed_offset: i64::try_from(consumed).map_err(|_| Error::Overflow)?,
            opaque_next_offset: next,
            origin,
        })
    }
}
