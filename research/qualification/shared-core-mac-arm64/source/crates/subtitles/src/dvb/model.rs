use safe_media_core::{Budget, Bytes, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Two,
    Four,
    Eight,
}

impl Depth {
    pub(crate) fn bits(self) -> u8 {
        match self {
            Self::Two => 2,
            Self::Four => 4,
            Self::Eight => 8,
        }
    }

    pub(crate) fn maximum(self) -> u8 {
        match self {
            Self::Two => 3,
            Self::Four => 15,
            Self::Eight => 255,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

/// Region-local source coordinates. Negative clip origins are permitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipRect {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasterSpec {
    pub region: Size,
    pub object_origin: Point,
    pub depth: Depth,
    pub non_modifying: bool,
    /// A cropped raster target, not a mutation of the source region geometry.
    /// Decoded pixels outside this intersection are charged and validated too.
    pub clip: Option<ClipRect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasterLimits {
    pub max_axis: u32,
    pub max_pixels: usize,
    pub max_field_bytes: usize,
    /// Total byte/token/pixel work across initialization and both fields.
    pub max_work: usize,
}

impl Default for RasterLimits {
    fn default() -> Self {
        Self {
            max_axis: 4096,
            max_pixels: 8 * 1024 * 1024,
            max_field_bytes: 65535,
            max_work: 128 * 1024 * 1024,
        }
    }
}

/// Tables are explicit per-field initial state. The enclosing object/state decoder
/// chooses their lifetime; this seam never invents page/epoch cache semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapTables {
    pub two_to_four: [u8; 4],
    pub two_to_eight: [u8; 4],
    pub four_to_eight: [u8; 16],
}

impl Default for MapTables {
    fn default() -> Self {
        // Primary numeric mappings in EN 300 743 tables 39–41.
        Self {
            two_to_four: [0, 7, 8, 15],
            two_to_eight: [0, 119, 136, 255],
            four_to_eight: [
                0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255,
            ],
        }
    }
}

impl MapTables {
    pub(crate) fn validate(self) -> Result<()> {
        if self.two_to_four.iter().any(|&index| index > 15) {
            return Err(Error::Invalid("DVB 2-to-4 map entry"));
        }
        Ok(())
    }

    pub(crate) fn map(self, source: Depth, target: Depth, index: u8) -> Result<u8> {
        match (source, target) {
            (Depth::Two, Depth::Four) => Ok(self.two_to_four[usize::from(index)]),
            (Depth::Two, Depth::Eight) => Ok(self.two_to_eight[usize::from(index)]),
            (Depth::Four, Depth::Eight) => Ok(self.four_to_eight[usize::from(index)]),
            _ if source == target => Ok(index),
            _ => Err(Error::Unsupported("DVB receiver depth reduction")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldParity {
    Top,
    Bottom,
}

impl FieldParity {
    pub(crate) fn bit(self) -> u8 {
        match self {
            Self::Top => 1,
            Self::Bottom => 2,
        }
    }
    pub(crate) fn line(self) -> u32 {
        match self {
            Self::Top => 0,
            Self::Bottom => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSpec {
    pub parity: FieldParity,
    /// Exact field payload size, excluding ODS header and optional stuffing byte.
    pub byte_length: usize,
    pub maps: MapTables,
}

impl FieldSpec {
    pub fn new(parity: FieldParity, byte_length: usize) -> Self {
        Self {
            parity,
            byte_length,
            maps: MapTables::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum InitialPlane {
    Fill(u8),
    /// Immutable previous pixels for precisely the same stored rectangle/stride.
    /// Copying them to a private charged builder is work-yielding and cancellable.
    Copy(Bytes),
}

/// Exact source facts only. CDS parsing/merging/colour conversion is not part of
/// this raster seam. `original` may retain the complete bounded definition chain.
#[derive(Clone, Debug)]
pub enum PaletteFacts {
    Default {
        depth: Depth,
    },
    Encoded {
        depth: Depth,
        page_id: u16,
        clut_id: u8,
        version: u8,
        original: Bytes,
    },
}

impl PaletteFacts {
    pub(crate) fn validate(&self, budget: &Budget, depth: Depth) -> Result<()> {
        let actual = match self {
            Self::Default { depth } => *depth,
            Self::Encoded {
                depth, original, ..
            } => {
                if !original.belongs_to(budget) {
                    return Err(Error::Invalid("DVB palette uses another budget"));
                }
                *depth
            }
        };
        if actual != depth {
            return Err(Error::Invalid("DVB palette depth"));
        }
        Ok(())
    }
}

impl PartialEq for PaletteFacts {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Default { depth: a }, Self::Default { depth: b }) => a == b,
            (
                Self::Encoded {
                    depth: a,
                    page_id: ap,
                    clut_id: ac,
                    version: av,
                    original: ao,
                },
                Self::Encoded {
                    depth: b,
                    page_id: bp,
                    clut_id: bc,
                    version: bv,
                    original: bo,
                },
            ) => a == b && ap == bp && ac == bc && av == bv && ao.as_ref() == bo.as_ref(),
            _ => false,
        }
    }
}
impl Eq for PaletteFacts {}

#[derive(Clone, Debug)]
pub struct SourceFacts {
    pub object: Option<Bytes>,
    pub palette: PaletteFacts,
}

impl PartialEq for SourceFacts {
    fn eq(&self, other: &Self) -> bool {
        self.palette == other.palette
            && self.object.as_ref().map(AsRef::as_ref) == other.object.as_ref().map(AsRef::as_ref)
    }
}
impl Eq for SourceFacts {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredGeometry {
    pub region: Size,
    pub object_origin: Point,
    pub requested_clip: Option<ClipRect>,
    pub stored: ClipRect,
    pub stride: usize,
}

#[derive(Clone, Debug)]
pub struct IndexedPlane {
    pub geometry: StoredGeometry,
    pub depth: Depth,
    pub pixels: Bytes,
    pub source: SourceFacts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NeedInput,
    Yield,
    FieldComplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub consumed: usize,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub consumed: usize,
    pub field_offset: usize,
    pub error: Error,
}
