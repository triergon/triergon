//! Source facts and retained PGS display results. Native views are explicitly
//! receiver projections; missing source colour or clocks remain missing.
use super::indexed_argb::{ArgbStorage, ArgbView, IndexedArgb};
use super::{CompositionObject, Crop, Envelope, SupClock, Window};
use std::num::NonZeroU16;
use std::sync::Arc;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Reservation, Result, Rounding, Time, TimeBase,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketClock {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub timebase: TimeBase,
}

impl PacketClock {
    pub fn pts_time(self) -> Result<Option<Time>> {
        self.pts
            .map(|ticks| Time::from_ticks(i128::from(ticks), self.timebase))
            .transpose()
    }

    pub fn receiver_microseconds(self) -> Result<Option<i64>> {
        self.pts_time()?
            .map(|time| time.to_i64_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway))
            .transpose()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentStamp {
    pub packet_sequence: u64,
    pub packet_clock: PacketClock,
    pub sup_clock: Option<SupClock>,
    pub packet_offset: usize,
    pub envelope: Envelope,
}

#[derive(Clone, Debug)]
pub struct SegmentFact {
    pub kind: u8,
    pub stamp: SegmentStamp,
    pub raw: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositionState {
    Normal,
    Acquisition,
    EpochStart,
    /// The pinned receiver resets for state 3 too. The raw state is retained;
    /// this name does not assert a normative meaning for that state.
    ReceiverReset3,
}

impl CompositionState {
    pub(crate) fn parse(raw: u8) -> Result<Self> {
        if raw & 0x3f != 0 {
            return Err(Error::Unsupported("PGS reserved composition state bits"));
        }
        Ok(match raw {
            0 => Self::Normal,
            0x40 => Self::Acquisition,
            0x80 => Self::EpochStart,
            _ => Self::ReceiverReset3,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentationFacts {
    pub width: u16,
    pub height: u16,
    pub frame_rate: u8,
    pub number: u16,
    pub raw_state: u8,
    pub state: CompositionState,
    pub palette_update_flags: u8,
    pub palette_id: u8,
    pub stamp: SegmentStamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourMatrix {
    Sd601,
    Hd709,
}

impl ColourMatrix {
    pub fn for_canvas(height: u16) -> Self {
        if height != 0 && height <= 576 {
            Self::Sd601
        } else {
            Self::Hd709
        }
    }
    /// Comparison label of this matrix's receiver quantisation.
    #[cfg(feature = "reference-trace")]
    pub fn identifier(self) -> &'static str {
        match self {
            Self::Sd601 => "native-pgs-sd601-quantized10-v1",
            Self::Hd709 => "native-pgs-hd709-quantized10-v1",
        }
    }

    /// Original integer matrix evaluator. Coefficients are rounded rational
    /// receiver facts, independently tested through public native palettes.
    /// This is not a declaration of source colour primaries/transfer metadata.
    pub fn argb(self, y: u8, cr: u8, cb: u8, alpha: u8) -> u32 {
        const fn coefficient(n: i64, d: i64) -> i64 {
            (n * 1024 + d / 2) / d
        }
        let chroma = match self {
            Self::Sd601 => [
                coefficient(140200 * 255, 100000 * 224),
                coefficient(71414 * 255, 100000 * 224),
                coefficient(34414 * 255, 100000 * 224),
                coefficient(177200 * 255, 100000 * 224),
            ],
            Self::Hd709 => [
                coefficient(15747 * 255, 10000 * 224),
                coefficient(4682 * 255, 10000 * 224),
                coefficient(1873 * 255, 10000 * 224),
                coefficient(18556 * 255, 10000 * 224),
            ],
        };
        let luma = (i64::from(y) - 16) * coefficient(255, 219);
        let red_difference = i64::from(cr) - 128;
        let blue_difference = i64::from(cb) - 128;
        let sums = [
            luma + chroma[0] * red_difference,
            luma - chroma[1] * red_difference - chroma[2] * blue_difference,
            luma + chroma[3] * blue_difference,
        ];
        let rgb = sums.map(|v| ((v + 512).div_euclid(1024)).clamp(0, 255) as u32);
        (u32::from(alpha) << 24) | (rgb[0] << 16) | (rgb[1] << 8) | rgb[2]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceColour {
    pub y: u8,
    pub cr: u8,
    pub cb: u8,
    pub alpha: u8,
    pub palette_version: u8,
    pub epoch: u64,
    pub stamp: SegmentStamp,
    pub receiver_matrix: ColourMatrix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourOrigin {
    ExplicitSource,
    /// Source cache was invalidated. Only the explicitly named receiver
    /// projection carries its prior physical palette slot; source stays None.
    PreviousEpochSlot,
    ReceiverZeroInitialization,
}

#[derive(Debug)]
pub(crate) struct ColourProvenance {
    pub palette_version: u8,
    pub epoch: u64,
    pub stamp: SegmentStamp,
    pub receiver_matrix: ColourMatrix,
    pub _owner_charge: Reservation,
}

/// Source components belong to this entry; immutable packet provenance can be
/// shared by entries and retained palette generations without losing knownness.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StoredSourceColour {
    pub components: [u8; 4],
    /// One-based slot in this immutable palette's owner table. Absence is a
    /// source fact, independent of the receiver's initialized/carried word.
    pub provenance: Option<NonZeroU16>,
}

#[derive(Clone, Debug)]
pub(crate) struct PaletteProvenanceOwner {
    pub provenance: Arc<ColourProvenance>,
    pub entries: u16,
}

#[derive(Debug)]
pub(crate) struct PaletteStorage {
    pub budget: Budget,
    pub id: u8,
    pub version: u8,
    pub epoch: u64,
    pub colours: BudgetVec<StoredSourceColour>,
    pub words: BudgetVec<u32>,
    pub origins: BudgetVec<ColourOrigin>,
    pub first_owner: Option<PaletteProvenanceOwner>,
    pub other_owners: BudgetVec<Option<PaletteProvenanceOwner>>,
    pub _owner_charge: Reservation,
}

impl PaletteStorage {
    pub(crate) fn source_provenance(&self, index: u8) -> Option<&Arc<ColourProvenance>> {
        if self.colours.is_empty() {
            return None;
        }
        let slot = self.colours.as_slice()[usize::from(index)].provenance?;
        let owner = if slot.get() == 1 {
            self.first_owner.as_ref()
        } else {
            self.other_owners.as_slice()[usize::from(slot.get() - 2)].as_ref()
        };
        Some(&owner.expect("PGS live palette provenance slot").provenance)
    }
}

#[derive(Clone, Debug)]
pub struct PaletteSnapshot(pub(crate) Arc<PaletteStorage>);

impl PaletteSnapshot {
    pub fn id(&self) -> u8 {
        self.0.id
    }
    pub fn version(&self) -> u8 {
        self.0.version
    }
    pub fn epoch(&self) -> u64 {
        self.0.epoch
    }
    pub fn source(&self, index: u8) -> Option<SourceColour> {
        let provenance = self.0.source_provenance(index)?;
        let colour = &self.0.colours.as_slice()[usize::from(index)];
        let [y, cr, cb, alpha] = colour.components;
        Some(SourceColour {
            y,
            cr,
            cb,
            alpha,
            palette_version: provenance.palette_version,
            epoch: provenance.epoch,
            stamp: provenance.stamp,
            receiver_matrix: provenance.receiver_matrix,
        })
    }
    pub fn receiver_argb(&self, index: u8) -> u32 {
        self.0.words.as_slice()[usize::from(index)]
    }
    pub fn receiver_words(&self) -> &[u32] {
        self.0.words.as_slice()
    }
    pub fn receiver_origin(&self, index: u8) -> ColourOrigin {
        self.0.origins.as_slice()[usize::from(index)]
    }
    pub fn belongs_to(&self, budget: &Budget) -> bool {
        self.0.budget.same_pool(budget)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleGeometry {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub source_x: u16,
    pub source_y: u16,
}

pub type DisplayObject = StoredDisplayObject<Bytes>;
/// Source object retaining indices and palette without an eager ARGB plane.
pub type IndexedDisplayObject = StoredDisplayObject<IndexedArgb>;

#[derive(Debug)]
pub struct StoredDisplayObject<A: ArgbStorage> {
    pub composition: CompositionObject,
    pub window: Option<Window>,
    pub object_version: u8,
    pub object_width: u16,
    pub object_height: u16,
    pub object_first_stamp: SegmentStamp,
    pub object_last_stamp: SegmentStamp,
    pub visible: VisibleGeometry,
    pub visibility_window_known: bool,
    pub receiver_x: u16,
    pub receiver_y: u16,
    pub receiver_forced_only_suppressed: bool,
    pub(crate) pixels: Bytes,
    pub(crate) visible_pixels: Bytes,
    pub(crate) receiver_argb_pixels: A,
    pub(crate) palette: PaletteSnapshot,
}

impl<A: ArgbStorage> StoredDisplayObject<A> {
    pub fn pixels(&self) -> &Bytes {
        &self.pixels
    }
    pub fn visible_pixels(&self) -> &Bytes {
        &self.visible_pixels
    }
    /// Owned index/palette view. Missing source colour remains missing in
    /// palette(); this writer projects only the explicitly named native words.
    pub fn receiver_argb_view(&self) -> Result<ArgbView> {
        ArgbView::new(
            self.pixels.clone(),
            self.palette.clone(),
            self.receiver_forced_only_suppressed,
        )
    }
    pub fn palette(&self) -> &PaletteSnapshot {
        &self.palette
    }
    pub fn receiver_argb(&self, index: u8) -> u32 {
        if self.receiver_forced_only_suppressed {
            0
        } else {
            self.palette.receiver_argb(index)
        }
    }
}

impl StoredDisplayObject<Bytes> {
    /// Existing materialized canonical A,R,G,B bytes. The default decoder
    /// retains this complete plane exactly as before.
    pub fn receiver_argb_pixels(&self) -> &Bytes {
        &self.receiver_argb_pixels
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockSource {
    Presentation,
    EndFallback,
    Unknown,
}

pub type DisplaySet = StoredDisplaySet<Bytes>;
/// Complete strict Source facts with indexed, explicitly writable ARGB views.
pub type IndexedDisplaySet = StoredDisplaySet<IndexedArgb>;

#[derive(Debug)]
pub struct StoredDisplaySet<A: ArgbStorage> {
    pub generation: u64,
    pub epoch: u64,
    pub presentation: PresentationFacts,
    pub end_stamp: SegmentStamp,
    pub receiver_pts_microseconds: Option<i64>,
    pub receiver_time_error: Option<Error>,
    pub receiver_clock_source: ClockSource,
    /// No source end/duration is encoded. The native API UINT32_MAX marker
    /// is a separate receiver convention, never an invented source duration.
    pub source_end: Option<Time>,
    pub(crate) objects: BudgetVec<StoredDisplayObject<A>>,
    pub(crate) segments: BudgetVec<SegmentFact>,
}

impl<A: ArgbStorage> StoredDisplaySet<A> {
    pub fn objects(&self) -> &[StoredDisplayObject<A>] {
        self.objects.as_slice()
    }
    pub fn segments(&self) -> &[SegmentFact] {
        self.segments.as_slice()
    }
    pub fn is_clear(&self) -> bool {
        self.objects.is_empty()
    }
}

pub(crate) fn geometry(
    reference: CompositionObject,
    window: Option<Window>,
    object: (u16, u16),
    canvas: (u16, u16),
) -> Result<VisibleGeometry> {
    let crop = reference.crop.unwrap_or(Crop {
        x: 0,
        y: 0,
        width: object.0,
        height: object.1,
    });
    let right = usize::from(crop.x)
        .checked_add(usize::from(crop.width))
        .ok_or(Error::Overflow)?;
    let bottom = usize::from(crop.y)
        .checked_add(usize::from(crop.height))
        .ok_or(Error::Overflow)?;
    if right > usize::from(object.0) || bottom > usize::from(object.1) {
        return Err(Error::Invalid("PGS crop outside object"));
    }
    let mut left = usize::from(reference.x);
    let mut top = usize::from(reference.y);
    let mut right = (left + usize::from(crop.width)).min(usize::from(canvas.0));
    let mut bottom = (top + usize::from(crop.height)).min(usize::from(canvas.1));
    if let Some(w) = window {
        left = left.max(usize::from(w.x));
        top = top.max(usize::from(w.y));
        right = right.min(usize::from(w.x) + usize::from(w.width));
        bottom = bottom.min(usize::from(w.y) + usize::from(w.height));
    }
    let width = right.saturating_sub(left);
    let height = bottom.saturating_sub(top);
    let source_x = usize::from(crop.x) + (left - usize::from(reference.x));
    let source_y = usize::from(crop.y) + (top - usize::from(reference.y));
    Ok(VisibleGeometry {
        x: u16::try_from(left).map_err(|_| Error::Overflow)?,
        y: u16::try_from(top).map_err(|_| Error::Overflow)?,
        width: u16::try_from(width).map_err(|_| Error::Overflow)?,
        height: u16::try_from(height).map_err(|_| Error::Overflow)?,
        source_x: u16::try_from(source_x.min(usize::from(object.0)))
            .map_err(|_| Error::Overflow)?,
        source_y: u16::try_from(source_y.min(usize::from(object.1)))
            .map_err(|_| Error::Overflow)?,
    })
}
