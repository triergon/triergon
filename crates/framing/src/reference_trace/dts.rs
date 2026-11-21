//! The raw 14-bit DTS parser's packets, clock and byte positions.
//!
//! [`ParserOperation`] reads one raw 14-bit DTS entity through two cursors fed
//! from the same bytes: the complete source frames
//! ([`Dts14SyncReader::per_frame`](crate::dts::Dts14SyncReader::per_frame)) and the
//! packets FFmpeg's `dca` parser cuts
//! ([`Dts14CarrierReader`](crate::dts::Dts14CarrierReader)). Each [`ParserPacket`]
//! carries the parser's clock ([`ParserClock`]): the first accepted core sample
//! rate is latched and later coding windows are rounded in it. That clock is not a
//! source duration. [`ParserHints`] are the parser's internal byte counters, not
//! source positions. [`FactAssociationJob`] relates a source frame to the parser
//! packet that contains it.
//!
//! Reference: the `dca` parser with default flags at FFmpeg git revision
//! `d32b387f` on the 9.0 branch.

mod operation;
mod parser_view;

pub use operation::{
    FactAssociation, FactAssociationJob, ParserClock, ParserClockDisposition, ParserOperation,
    ParserOperationContext, ParserOperationEvent, ParserOperationLane, ParserOperationLimits,
    ParserOperationStep, ParserOperationTerminal, ParserPacket,
};
pub use parser_view::{ParserHintOrigin, ParserHints};
