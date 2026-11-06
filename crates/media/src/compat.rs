//! Reference-compatible policies.
//!
//! The reader and the types elsewhere in this crate follow the specifications and
//! keep exact source facts. Where FFmpeg accepts or reports an input differently and
//! a consumer needs FFmpeg's result, the policies here reproduce FFmpeg's behaviour
//! for that input. Each module is one policy family; its documentation names the
//! FFmpeg build it reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies. They
//! are compiled only with the `reference-trace` feature; see `reference_trace`.

/// The packet clock FFmpeg's Matroska demuxer reports.
///
/// [`BlockTiming::source`](crate::timing::BlockTiming::source) keeps exact rational
/// nanoseconds with CodecDelay subtracted, including negative and fractional lace
/// times. [`packet_timing`](clock::packet_timing) instead reports a block's
/// timestamp and duration in whole Segment ticks, as FFmpeg's packets carry them,
/// including the timestamps FFmpeg omits. A player that schedules or matches packets
/// against FFmpeg-derived clocks uses this view; the source view is unchanged.
///
/// Reference: the `matroska` demuxer at FFmpeg git revision `d32b387f` on the 9.0
/// branch.
pub mod clock {
    pub use crate::packet_clock::packet_timing;
}

/// Block additions as FFmpeg's Matroska demuxer accepts and exports them.
///
/// The reader refuses a BlockAddID above the track's MaxBlockAdditionID.
/// [`new_reader`](block_additions::new_reader) and
/// [`open_owned_seekable_in`](block_additions::open_owned_seekable_in) build readers
/// that also admit WebM VP9's implicit ITU-T T.35 addition: BlockAddID 4 as the only
/// addition of a `V_VP9` track that declares neither MaxBlockAdditionID nor a
/// BlockAdditionMapping, on a source whose DocType is `webm`.
///
/// [`packet_side_data`](block_additions::packet_side_data) projects a packet's
/// additions to the side data FFmpeg attaches to it
/// ([`PacketSideData`](block_additions::PacketSideData)): mapped and default
/// additions as opaque entries prefixed with their type, and LCEVC payloads from
/// T.35 additions. [`hdr10_plus`](block_additions::hdr10_plus) decodes the HDR10+
/// event of a packet the same way. On Matroska an addition other than BlockAddID 1
/// needs a BlockAdditionMapping; on WebM an unmapped BlockAddID is its own type.
/// The source [`BlockGroupSource`](crate::typed::BlockGroupSource) is unchanged.
///
/// Reference: the `matroska` demuxer at FFmpeg git revision `d32b387f` on the 9.0
/// branch.
pub mod block_additions {
    pub use crate::matroska::typed::block_additions::{
        PacketSideData, hdr10_plus, packet_side_data,
    };
    pub use crate::typed::reader::{blocking::open_owned_seekable_in, new_reader};
}
