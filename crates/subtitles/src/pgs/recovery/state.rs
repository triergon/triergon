//! The receiver's stream state and how each segment changes it.
//!
//! Fixed-size fields are read the way the receiver reads them: from the packet
//! at the field's offset, past the end of their segment if it is short, and as
//! zero past the end of the packet (the decoder API's padding). Variable parts
//! (composition objects, palette entries, object data) stop at the segment end.

use super::super::{ColourMatrix, CompositionObject, Crop, DisplayLimits, PacketClock};
use super::{ReceiverObservations, Refusal};
use std::sync::Arc;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Reservation, Result};

/// The most composition objects the receiver presents from one PCS.
pub(super) const MAX_REFERENCES: usize = 2;

/// A byte of `raw` at `at`, zero past its end.
fn byte(raw: &[u8], at: usize) -> u8 {
    raw.get(at).copied().unwrap_or(0)
}

fn word(raw: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([byte(raw, at), byte(raw, at + 1)])
}

/// Whether the receiver accepts a canvas size (it validates it as an image size).
fn valid_canvas(width: u16, height: u16) -> bool {
    let (w, h) = (u64::from(width), u64::from(height));
    w > 0 && h > 0 && (w + 128) * (h + 128) < u64::from(i32::MAX.unsigned_abs()) / 8
}

/// A packet's PTS in microseconds, as the receiver rescales it: `None` when
/// unknown (absent or the unknown sentinel) or not representable.
pub(super) fn microseconds(clock: PacketClock) -> Option<i64> {
    let pts = clock.pts.filter(|&pts| pts != i64::MIN)?;
    // `pts * num / den` seconds, rounded to the nearest microsecond with ties
    // away from zero; the exact rational path covers what i128 cannot.
    let (num, den) = (clock.timebase.numerator(), clock.timebase.denominator());
    let scaled = i128::from(pts)
        .checked_mul(i128::from(num) * 1_000_000)
        .map(|n| {
            let den = i128::from(den);
            let away = 2 * (n % den).abs() >= den;
            n / den + i128::from(away) * n.signum()
        });
    let value = match scaled {
        Some(value) => i64::try_from(value).ok(),
        None => clock.receiver_microseconds().ok().flatten(),
    };
    value.filter(|&value| value != i64::MIN)
}

/// A composition object as the receiver places it: at its position, or at the
/// origin when that lies outside the canvas of its PCS.
#[derive(Clone, Copy, Debug)]
pub(super) struct Reference {
    pub(super) object: CompositionObject,
    pub(super) x: u16,
    pub(super) y: u16,
}

#[derive(Debug)]
pub(super) struct Object {
    pub(super) id: u16,
    pub(super) width: u16,
    pub(super) height: u16,
    /// RLE bytes the first fragment declared.
    declared: usize,
    pub(super) encoded: BudgetVec<u8>,
    fragments: usize,
}

/// What an ODS does to its object, decided and allocated before the object
/// cache changes.
enum Update<'a> {
    /// The receiver rejects the fragment.
    Keep,
    /// The receiver refuses the packet at the fragment.
    Refuse(Refusal),
    /// An accepted first fragment: the object it starts.
    Replace(Object),
    /// An accepted continuation: bytes for which the object has room.
    Append(&'a [u8]),
}

/// The count after one more fragment of an object.
fn next_fragment(fragments: usize, limits: &DisplayLimits) -> Result<usize> {
    if fragments >= limits.max_fragments {
        return Err(Error::Limit {
            requested: fragments + 1,
            maximum: limits.max_fragments,
        });
    }
    Ok(fragments + 1)
}

impl Object {
    /// An object slot before any fragment of it is accepted.
    fn empty(id: u16, budget: &Budget) -> Self {
        Self {
            id,
            width: 0,
            height: 0,
            declared: 0,
            encoded: BudgetVec::new(budget),
            fragments: 0,
        }
    }

    /// A first fragment (after its id, version and flags): the object it
    /// starts, when it has data and a size inside the canvas and declares at
    /// least that data.
    fn start<'a>(
        id: u16,
        fragment: &'a [u8],
        canvas: [u16; 2],
        limits: &DisplayLimits,
        budget: &Budget,
    ) -> Result<Update<'a>> {
        let [l0, l1, l2, w0, w1, h0, h1, data @ ..] = fragment else {
            return Ok(Update::Keep);
        };
        let declared = usize::from(*l0) << 16 | usize::from(*l1) << 8 | usize::from(*l2);
        let (width, height) = (
            u16::from_be_bytes([*w0, *w1]),
            u16::from_be_bytes([*h0, *h1]),
        );
        let [canvas_width, canvas_height] = canvas;
        if data.is_empty()
            || width == 0
            || height == 0
            || width > canvas_width
            || height > canvas_height
        {
            return Ok(Update::Keep);
        }
        // The receiver cannot allocate a negative data length.
        let Some(length) = declared.checked_sub(4) else {
            return Ok(Update::Refuse(Refusal::ObjectLength));
        };
        if data.len() > length {
            return Ok(Update::Keep);
        }
        let pixels = usize::from(width) * usize::from(height);
        for (requested, maximum) in [
            (pixels, limits.max_object_pixels),
            (length, limits.max_encoded),
        ] {
            if requested > maximum {
                return Err(Error::Limit { requested, maximum });
            }
        }
        let fragments = next_fragment(0, limits)?;
        let mut encoded = BudgetVec::new(budget);
        encoded.try_reserve_exact(length)?;
        encoded.try_extend_from_slice(data)?;
        Ok(Update::Replace(Self {
            id,
            width,
            height,
            declared: length,
            encoded,
            fragments,
        }))
    }

    /// A continuation fragment's data, accepted while it fits the length the
    /// first fragment declared. Room for it is reserved here.
    fn continuation<'a>(&mut self, data: &'a [u8], limits: &DisplayLimits) -> Result<Update<'a>> {
        if self.width == 0 || data.len() > self.declared - self.encoded.len() {
            return Ok(Update::Keep);
        }
        next_fragment(self.fragments, limits)?;
        self.encoded.try_reserve_exact(data.len())?;
        Ok(Update::Append(data))
    }

    /// Whether the object has a bitmap to decode.
    pub(super) fn has_data(&self) -> bool {
        self.width != 0 && !self.encoded.is_empty()
    }

    /// The object's index plane, or `None` when its RLE does not fill it.
    pub(super) fn decode(&self, budget: &Budget) -> Result<Option<Bytes>> {
        let mut plane = BudgetVec::new(budget);
        plane.try_resize(usize::from(self.width) * usize::from(self.height), 0)?;
        if !super::rle::decode(
            self.encoded.as_slice(),
            usize::from(self.height),
            plane.as_mut_slice(),
        ) {
            return Ok(None);
        }
        plane.freeze().map(Some).map_err(|failure| failure.error)
    }
}

/// A palette's ARGB words. Displays share them; a PDS changes them in place
/// unless a display still holds them, and then writes a copy.
#[derive(Debug)]
pub(super) struct PaletteWords {
    pub(super) words: [u32; 256],
    // Fields drop in declaration order: storage before its live charge.
    _charge: Reservation,
}

impl PaletteWords {
    fn new(budget: &Budget, words: [u32; 256]) -> Result<Arc<Self>> {
        let charge = budget.reserve(size_of::<Self>())?;
        Ok(Arc::new(Self {
            words,
            _charge: charge,
        }))
    }
}

#[derive(Debug)]
struct PaletteSlot {
    id: Option<u8>,
    words: Arc<PaletteWords>,
}

/// Canvas, presentation clock, the current composition and the epoch's object
/// and palette caches.
#[derive(Debug)]
pub(super) struct Receiver {
    pub(super) canvas: [u16; 2],
    /// The PTS the next display takes, in microseconds: zero before any PCS,
    /// then the subtitle PTS current when the latest PCS was read (`None`
    /// when that was unknown). A display uses it once.
    pub(super) presentation: Option<i64>,
    pub(super) palette_id: u8,
    pub(super) references: [Option<Reference>; MAX_REFERENCES],
    pub(super) objects: BudgetVec<Object>,
    palettes: BudgetVec<PaletteSlot>,
}

impl Receiver {
    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            canvas: [0, 0],
            presentation: Some(0),
            palette_id: 0,
            references: [None; MAX_REFERENCES],
            objects: BudgetVec::new(budget),
            palettes: BudgetVec::new(budget),
        }
    }

    /// Lookups a segment may make, for its work charge.
    pub(super) fn cache_entries(&self) -> usize {
        self.objects.len() + self.palettes.len()
    }

    pub(super) fn references(&self) -> impl Iterator<Item = Reference> + '_ {
        self.references.iter().map_while(|r| *r)
    }

    pub(super) fn object(&self, id: u16) -> Option<&Object> {
        self.objects.as_slice().iter().find(|o| o.id == id)
    }

    /// The words of the palette the composition selects.
    pub(super) fn palette(&self) -> Option<&Arc<PaletteWords>> {
        self.palettes
            .as_slice()
            .iter()
            .find(|p| p.id == Some(self.palette_id))
            .map(|p| &p.words)
    }

    /// PCS: the presentation PTS (`subtitle_pts`) always. A canvas the receiver rejects
    /// leaves it zero-sized and the composition as it was; with a valid one
    /// the PCS sets the composition, and an epoch start or acquisition point
    /// (either of the state's two high bits) clears the object and palette
    /// caches.
    pub(super) fn composition(
        &mut self,
        raw: &[u8],
        body: std::ops::Range<usize>,
        subtitle_pts: Option<i64>,
        limits: &DisplayLimits,
    ) -> Result<()> {
        self.presentation = subtitle_pts;
        let at = body.start;
        let (width, height) = (word(raw, at), word(raw, at + 2));
        if !valid_canvas(width, height) {
            self.canvas = [0, 0];
            return Ok(());
        }
        let count = usize::from(byte(raw, at + 10));
        if count > limits.max_references {
            return Err(Error::Limit {
                requested: count,
                maximum: limits.max_references,
            });
        }
        self.canvas = [width, height];
        self.palette_id = byte(raw, at + 9);
        if byte(raw, at + 7) & 0xc0 != 0 {
            self.objects.clear();
            for palette in self.palettes.as_mut_slice() {
                palette.id = None;
            }
        }
        self.references = [None; MAX_REFERENCES];
        let mut at = at + 11;
        for slot in self.references.iter_mut().take(count) {
            if body.end.saturating_sub(at) < 8 {
                break;
            }
            let flags = byte(raw, at + 3);
            let crop = (flags & 0x80 != 0).then(|| Crop {
                x: word(raw, at + 8),
                y: word(raw, at + 10),
                width: word(raw, at + 12),
                height: word(raw, at + 14),
            });
            let object = CompositionObject {
                id: word(raw, at),
                window: byte(raw, at + 2),
                flags,
                x: word(raw, at + 4),
                y: word(raw, at + 6),
                crop,
            };
            let (x, y) = if object.x > width || object.y > height {
                (0, 0)
            } else {
                (object.x, object.y)
            };
            *slot = Some(Reference { object, x, y });
            at += if crop.is_some() { 16 } else { 8 };
        }
        Ok(())
    }

    /// PDS: the palette with the segment's id (a new one while fewer than the
    /// limit exist, otherwise the segment is ignored) takes each 5-byte entry
    /// that starts inside the segment.
    pub(super) fn palette_definition(
        &mut self,
        raw: &[u8],
        body: std::ops::Range<usize>,
        limits: &DisplayLimits,
        budget: &Budget,
        observations: &mut ReceiverObservations,
    ) -> Result<()> {
        let id = byte(raw, body.start);
        let slots = self.palettes.as_slice();
        let found = slots
            .iter()
            .position(|p| p.id == Some(id))
            .or_else(|| slots.iter().position(|p| p.id.is_none()));
        let index = match found {
            Some(index) => index,
            None if self.palettes.len() < limits.max_palettes => {
                self.palettes
                    .try_push(PaletteSlot {
                        id: None,
                        words: PaletteWords::new(budget, [0; 256])?,
                    })
                    .map_err(|e| e.error)?;
                self.palettes.len() - 1
            }
            None => return Ok(()),
        };
        let matrix = ColourMatrix::for_canvas(self.canvas[1]);
        let slot = &mut self.palettes.as_mut_slice()[index];
        if Arc::get_mut(&mut slot.words).is_none() {
            slot.words = PaletteWords::new(budget, slot.words.words)?;
        }
        slot.id = Some(id);
        let Some(PaletteWords { words, .. }) = Arc::get_mut(&mut slot.words) else {
            return Err(Error::Invalid("PGS receiver palette shared"));
        };
        let entries = body.start + 2..body.end.max(body.start + 2);
        let whole = raw.get(entries.clone()).unwrap_or_default().chunks_exact(5);
        let tail = entries.start + 5 * whole.len();
        for &[index, y, cr, cb, alpha] in whole.map(|e| <&[u8; 5]>::try_from(e).expect("5 bytes")) {
            words[usize::from(index)] = matrix.argb(y, cr, cb, alpha);
        }
        // An entry starting inside the segment but ending past it reads on.
        if tail < entries.end {
            let [index, y, cr, cb, alpha] = std::array::from_fn(|i| byte(raw, tail + i));
            words[usize::from(index)] = matrix.argb(y, cr, cb, alpha);
            observations.palette_cross_segment_bytes += tail + 5 - entries.end;
            observations.palette_zero_padding_bytes += (tail + 5).saturating_sub(raw.len());
        }
        Ok(())
    }

    /// ODS: one fragment of an object (a new one while fewer than the limit
    /// exist, otherwise the segment is ignored). A first fragment that the
    /// receiver accepts replaces the object; a rejected one leaves it as it was,
    /// though a new id still takes a slot. The fragment is checked and its
    /// storage allocated before the cache changes, so an error leaves the
    /// cache as it was.
    pub(super) fn object_fragment(
        &mut self,
        body: &[u8],
        limits: &DisplayLimits,
        budget: &Budget,
    ) -> Result<Option<Refusal>> {
        let [id_high, id_low, _version, flags, fragment @ ..] = body else {
            return Ok(None);
        };
        let id = u16::from_be_bytes([*id_high, *id_low]);
        let index = self.objects.as_slice().iter().position(|o| o.id == id);
        if index.is_none() && self.objects.len() >= limits.max_objects {
            return Ok(None);
        }
        let update = if flags & 0x80 != 0 {
            Object::start(id, fragment, self.canvas, limits, budget)?
        } else if let Some(index) = index {
            self.objects.as_mut_slice()[index].continuation(fragment, limits)?
        } else {
            Update::Keep
        };
        let refusal = match update {
            Update::Refuse(refusal) => Some(refusal),
            _ => None,
        };
        match (index, update) {
            (Some(index), Update::Replace(object)) => self.objects.as_mut_slice()[index] = object,
            (Some(index), Update::Append(data)) => {
                let object = &mut self.objects.as_mut_slice()[index];
                object.encoded.try_extend_from_slice(data)?;
                object.fragments += 1;
            }
            (Some(_), Update::Keep | Update::Refuse(_)) => {}
            (None, Update::Replace(object)) => {
                self.objects.try_push(object).map_err(|e| e.error)?;
            }
            (None, _) => {
                self.objects
                    .try_push(Object::empty(id, budget))
                    .map_err(|e| e.error)?;
            }
        }
        Ok(refusal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::TimeBase;

    #[test]
    fn microseconds_match_the_exact_rational_rescale() {
        let bases = [
            (1, 90_000),
            (1, 1000),
            (1, 1_000_000_000),
            (1001, 30_000),
            (7, 3),
            (1, 1),
        ];
        let values = [
            0,
            1,
            -1,
            44,
            45,
            -45,
            4_294_967_295,
            i64::MAX,
            i64::MIN + 1,
            -8_589_934_591,
        ];
        for (num, den) in bases {
            for pts in values.into_iter().chain(-500..500) {
                let clock = PacketClock {
                    pts: Some(pts),
                    dts: None,
                    timebase: TimeBase::new(num, den).unwrap(),
                };
                let exact = clock.receiver_microseconds().ok().flatten();
                assert_eq!(
                    microseconds(clock),
                    exact.filter(|&v| v != i64::MIN),
                    "{pts} {num}/{den}"
                );
            }
        }
    }
}
