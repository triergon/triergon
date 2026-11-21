//! The PES feed of a single-program TS188 source, which the progressive and
//! segment readers of `triergon-hls` assemble access units from.
//!
//! A feed consumes the validated carriers of its tracks directly and delivers
//! completed PES packets with their demux clock; it delivers only the rare
//! table, clock and fault events of the source whole.

use crate::{Engine, Result};
use triergon_core::InputCursor;
use triergon_mpegts::ParseLimits;

pub use crate::engines::{FeedPacket, FeedStep, TsEngine};
pub use crate::ts_payload::{PesFacts, PesInput, SpentLists};

/// A player engine over one independent TS188 segment with an in-band PAT and
/// PMT, delivering demux PES of the tracks its PMT names: AVC and AAC, or with
/// `media` the HLS audio carriage rules (AC-3 and E-AC-3 as well).
pub fn open_segment_feed(input: InputCursor, limits: ParseLimits, media: bool) -> Result<Engine> {
    Engine::ts_hls_segment(input, limits, media)
}
