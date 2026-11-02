//! DVB display-set types are format-owned, above core time/bytes/work.
use super::boundary::PacketSourceFacts;
use super::colour::{PaletteEntry, SOURCE_PROJECTION, STUDIO_UNREPAIRED_PROJECTION};
use super::{Depth, IndexedPlane, Point, Size};
use safe_media_core::{BudgetVec, Bytes, Time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Service {
    pub composition_page: u16,
    pub ancillary_page: u16,
    pub subtitling_type: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketEnvelope {
    Segments,
    PesDataField,
}

/// EN300743 models cumulative region pixels and renders ODS as decoded. It does
/// not establish mandatory cross-display compressed-object replay. Keep that
/// bounded research extension explicit instead of treating its oracle as native
/// or normative conformance evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectRetention {
    CurrentDisplay,
    EpochExtension,
}

#[derive(Clone, Debug)]
pub struct DvbPacket {
    pub payload: Bytes,
    pub pts: Option<Time>,
    pub generation: u64,
    pub sequence: u64,
    pub envelope: PacketEnvelope,
    /// Original configuration, raw clock values and container locator when the
    /// configured boundary is used. Manually declared legacy packets use None.
    pub source: Option<PacketSourceFacts>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayWindow {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct DisplayDefinition {
    pub size: Size,
    pub window: Option<DisplayWindow>,
    pub version: Option<u8>,
    pub original: Option<Bytes>,
}

impl Default for DisplayDefinition {
    fn default() -> Self {
        Self {
            size: Size {
                width: 720,
                height: 576,
            },
            window: None,
            version: None,
            original: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayAction {
    Present,
    Update,
    Clear,
}

#[derive(Debug)]
pub struct PaletteSnapshot {
    pub depth: Depth,
    pub projection: &'static str,
    pub rgba: Bytes,
    /// Separately charged explicit projection, never substituted for raw facts
    /// or the original source-nearest view. General native equality is open.
    pub studio_unrepaired_rgba: Bytes,
    pub studio_unrepaired_projection: &'static str,
    pub(crate) entries: BudgetVec<PaletteEntry>,
}

impl PaletteSnapshot {
    pub fn entries(&self) -> &[PaletteEntry] {
        self.entries.as_slice()
    }
    pub(crate) fn projection() -> &'static str {
        SOURCE_PROJECTION
    }
    pub(crate) fn studio_projection() -> &'static str {
        STUDIO_UNREPAIRED_PROJECTION
    }
}

#[derive(Debug)]
pub struct DisplayRegion {
    pub id: u8,
    pub position: Point,
    pub source_address: Point,
    pub size: Size,
    pub plane: IndexedPlane,
    pub palette: PaletteSnapshot,
    pub original_composition: Bytes,
}

#[derive(Clone, Debug)]
pub struct OpaqueSegment {
    pub kind: u8,
    pub page_id: u16,
    pub original: Bytes,
    /// Known extension needing a separately frozen capability, rather than
    /// ordinary forward-compatible/private opaque data.
    pub unsupported_feature: bool,
}

/// Immutable by public accessors. Source/caches/output all keep their pool charges.
#[derive(Debug)]
pub struct DisplayEvent {
    pub service: Service,
    pub object_retention: ObjectRetention,
    pub generation: u64,
    pub epoch: u64,
    pub sequence: u64,
    pub pts: Option<Time>,
    pub timeout_seconds: u8,
    pub expiry: Option<Time>,
    pub action: DisplayAction,
    pub definition: DisplayDefinition,
    pub page_composition: Option<Bytes>,
    pub page_version: Option<u8>,
    pub page_state: Option<u8>,
    pub(crate) regions: BudgetVec<DisplayRegion>,
    pub(crate) packets: BudgetVec<DvbPacket>,
    pub(crate) opaque: BudgetVec<OpaqueSegment>,
}

impl DisplayEvent {
    pub fn regions(&self) -> &[DisplayRegion] {
        self.regions.as_slice()
    }
    pub fn source_packets(&self) -> &[DvbPacket] {
        self.packets.as_slice()
    }
    pub fn opaque_segments(&self) -> &[OpaqueSegment] {
        self.opaque.as_slice()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeStatus {
    NeedPacket,
    Yield,
    Event,
    NeedOutput,
    Finished,
}

/// A struct avoids a large payload-versus-empty enum or uncharged boxing.
#[derive(Debug)]
pub struct DisplayProgress {
    pub status: DecodeStatus,
    pub event: Option<DisplayEvent>,
    pub pressure: Option<safe_media_core::Error>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayLimits {
    pub packet_bytes: usize,
    pub packets_per_display: usize,
    pub segments_per_display: usize,
    pub regions: usize,
    pub objects: usize,
    pub clut_families: usize,
    pub object_references: usize,
    pub aggregate_region_pixels: usize,
    pub work_per_display: usize,
}

impl Default for DisplayLimits {
    fn default() -> Self {
        Self {
            packet_bytes: 16 * 1024 * 1024,
            packets_per_display: 64,
            segments_per_display: 16384,
            regions: 256,
            objects: 4096,
            clut_families: 256,
            object_references: 4096,
            aggregate_region_pixels: 8 * 1024 * 1024,
            work_per_display: 128 * 1024 * 1024,
        }
    }
}
