//! The byte positions the reference parser reports with a packet. They are the
//! parser's counters and never positions in the source. They follow the output of
//! the parser named on [`crate::reference_trace::dts`], run with default flags
//! through its public interface. The position reported with a packet flushed at
//! the end of input is a signed value the parser gives; it is not the end of the
//! source, a syntax check or a timestamp, and nothing is claimed for other DTS
//! streams or other FFmpeg revisions.
use crate::dts::{Dts14CarrierBoundary, Dts14CarrierPacket};
use triergon_core::{Error, Result};
/// Which way of returning a packet the positions of a [`ParserHints`] belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserHintOrigin {
    /// The parser found the next sync pattern and returned the packet before it.
    FollowingSync,
    /// The parser was flushed at the end of input and returned the bytes it held.
    /// Its next-frame position is then 100 bytes before that end.
    ExplicitFlushLookbehind,
}
/// The byte positions FFmpeg's `dca` parser reports with one packet: signed counts
/// of the bytes it was fed, starting at offset zero of the source. They are the
/// parser's counters and need not be places in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserHints {
    /// Offset of the packet's first byte.
    pub frame_offset: i64,
    /// Offset the parser's count of consumed bytes has reached when it returns the
    /// packet: the packet's end, or the start of the last input window if that is
    /// later. The count never goes back over bytes of the following sync pattern
    /// that earlier windows supplied.
    pub consumed_offset: i64,
    /// The parser's offset for the next frame: the packet's end after a following
    /// sync pattern, 100 bytes before the packet's end after a flush. That value is
    /// negative for a source shorter than 100 bytes.
    pub opaque_next_offset: i64,
    /// How the parser returned the packet.
    pub origin: ParserHintOrigin,
}
impl ParserHints {
    /// The positions the parser reports with `packet`, derived from the packet's
    /// source range, its boundary and its [`Dts14CarrierPacket::provision`]. The
    /// packet itself is not changed. The result holds for a parser opened at offset
    /// zero of the source and fed the same contiguous input windows with default
    /// flags. The caller answers for that: the packet does not record it, and
    /// nothing is claimed for a parser that was reset or reopened.
    ///
    /// Fails with [`Error::Invalid`] when the packet's `provision` contradicts itself
    /// or the packet's boundary: a window that ends before it starts, a read position
    /// past the window's end, or a read position other than six bytes past a following
    /// sync pattern or the packet's end at the end of the source; [`Error::Overflow`]
    /// when an offset does not fit `i64`.
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
