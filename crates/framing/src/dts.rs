//! DTS: core frame headers and frames, declared frame extents, header
//! inspection field by field, and the 14-bit and little-endian word packings.
//!
//! | Need | Entry points |
//! |---|---|
//! | Header of a core frame | [`probe_core_header`], [`DtsCoreHeader`] |
//! | Core frames of an aligned stream | [`DtsCoreFrameReader`] |
//! | Declared core and extension substream sizes | [`probe_frame_extent`], [`probe_extension_substream_extent`] |
//! | Every header field with its source bits | [`DtsHeaderInspector`], [`inspect_header_bits`] |
//! | Packed streams as canonical bytes | [`Dts14BitUnpacker`], [`DtsPackedPayloadJob`] |
//! | Frames of a packed stream | [`Dts14SyncReader`], [`Dts14CarrierReader`], [`DtsNormalSourceReader`] |

use triergon_core::SourceId;

mod carrier14;
mod core_frame;
mod extent;
mod inspect;
mod normal;
mod packed;
mod packed_header;
mod packing;
pub(crate) mod sync14;

pub use carrier14::{
    Dts14CarrierBoundary, Dts14CarrierLimits, Dts14CarrierPacket, Dts14CarrierProvision,
    Dts14CarrierReader, Dts14CarrierSyntax,
};
pub use core_frame::{
    DtsCoreFrame, DtsCoreFrameFacts, DtsCoreFrameLimits, DtsCoreFrameReader, DtsCoreHeader,
    probe_core_header,
};
pub use extent::{
    DtsExtensionSubstreamExtent, DtsFrameExtent, DtsFrameExtentLimits,
    probe_extension_substream_extent, probe_frame_extent,
};
pub use inspect::{
    DtsCoreField, DtsHeaderInspector, DtsInspection, DtsInspectionIssues,
    DtsInspectionSourceStatus, DtsInspectionViews, FieldObservation,
};
pub use normal::{
    DtsCarrierTail, DtsExtentRelation, DtsNormalBoundary, DtsNormalFacts, DtsNormalLimits,
    DtsNormalSourceFrame, DtsNormalSourceReader,
};
pub use packed::{DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob, DtsPacking};
pub use packed_header::{DtsPackedHeader, inspect_header_bits};
pub use packing::{
    Dts14BitPayload, Dts14BitUnpacker, DtsPackedBitLocation, DtsPackingLimits, DtsWordOrder,
};
pub use sync14::{
    Dts14BoundaryEvidence, Dts14SyncFacts, Dts14SyncFrame, Dts14SyncLimits, Dts14SyncReader,
};

/// Where a complete DTS record a caller owns came from. The readers carry it
/// through to their output unchanged; it is not checked against a transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsSourceIdentity {
    /// The source the record was read from.
    pub source: SourceId,
    /// The generation of the input the record was read from, as
    /// [`triergon_core::InputToken::generation`] gives it.
    pub generation: u64,
    /// Offset of the record's first byte in `source`, in bytes.
    pub offset: u64,
}
