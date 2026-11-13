//! The receiver's stream state and how each segment changes it.
//!
//! Fixed-size fields are read the way the receiver reads them: from the packet
//! at the field's offset, past the end of their segment if it is short, and as
//! zero past the end of the packet (the decoder API's padding). Variable parts
//! (composition objects, palette entries, object data) stop at the segment end.

use super::super::{CompositionObject, Crop};
use super::{DisplayLimits, PacketClock, ReceiverObservations, Refusal, reserve_amortised};
use std::sync::Arc;
use triergon_core::{Budget, BudgetVec, Error, Reservation, Result};
use triergon_pixel::{ChannelRounding, YcbcrMatrix};

/// The most composition objects the receiver presents from one PCS.
pub(super) const MAX_REFERENCES: usize = 2;

/// A byte of `raw` at `at`, zero past its end.
fn byte(raw: &[u8], at: usize) -> u8 {
    raw.get(at).copied().unwrap_or(0)
}

fn word(raw: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([byte(raw, at), byte(raw, at + 1)])
}

/// Whether the receiver takes a PCS's video size as its canvas
/// ([`crate::canvas::accepts`]): 65,535 × 1 and 63,422 × 4,096 are taken,
/// 0 × 0, 63,423 × 4,096 and 65,535 × 65,535 are not.
fn valid_canvas(width: u16, height: u16) -> bool {
    crate::canvas::accepts(width, height)
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
    /// The RLE bytes received so far.
    pub(super) encoded: BudgetVec<u8>,
    fragments: usize,
}

/// What an ODS does to its object, decided and allocated before the object
/// cache changes.
enum Update<'a> {
    /// The receiver rejects the fragment.
    Keep,
    /// A first fragment whose length leaves no room for its size fields: the
    /// object it starts has that size and no data, and the packet is refused.
    Unallocated(Object),
    /// An accepted first fragment: the object it starts.
    Replace(Object),
    /// An accepted continuation: bytes for which the object has room.
    Append(&'a [u8]),
}

/// Room for `additional` more RLE bytes of an object that declared
/// `declared`. Storage grows with the bytes that arrive, never past the
/// declared length, so a declared length is a claim, not an allocation; the
/// bytes held stay within `limits.max_encoded`.
fn stored(
    encoded: &mut BudgetVec<u8>,
    additional: usize,
    declared: usize,
    limits: &DisplayLimits,
) -> Result<()> {
    let requested = encoded
        .len()
        .checked_add(additional)
        .ok_or(Error::Overflow)?;
    if requested > limits.max_encoded {
        return Err(Error::Limit {
            requested,
            maximum: limits.max_encoded,
        });
    }
    reserve_amortised(encoded, additional, declared.min(limits.max_encoded))
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

    /// A first fragment: the object data length (24 bits, counting the four
    /// bytes of the width and height that follow it), the width, the height,
    /// then RLE bytes. Observed on a 16 × 4 canvas, in this order:
    ///
    /// - a fragment without an RLE byte after its size fields is rejected,
    ///   whatever the length declares; its fields are never read past the
    ///   segment end;
    /// - a width or height of zero or beyond the canvas (17 × 1, 1 × 5) is
    ///   rejected; 16 × 4 and 1 × 4 are accepted;
    /// - a declared length below 4 (no room for the size fields) refuses the
    ///   packet, and the object takes the new size without data;
    /// - more RLE bytes than the length declares reject the fragment (a length
    ///   of 8 with 6 bytes, or of 4 with any);
    /// - otherwise the fragment replaces the object with its bytes, however
    ///   many more the length declares, up to 2²⁴ − 5.
    ///
    /// The ODS syntax (BD-ROM presentation graphics) has the length count the
    /// size fields; the rejections and the refusal are the reference's.
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
        let (width, height) = (
            u16::from_be_bytes([*w0, *w1]),
            u16::from_be_bytes([*h0, *h1]),
        );
        if data.is_empty() || width == 0 || height == 0 || width > canvas[0] || height > canvas[1] {
            return Ok(Update::Keep);
        }
        let mut object = Object {
            width,
            height,
            ..Self::empty(id, budget)
        };
        let length = usize::from(*l0) << 16 | usize::from(*l1) << 8 | usize::from(*l2);
        let Some(declared) = length.checked_sub(4) else {
            return Ok(Update::Unallocated(object));
        };
        if data.len() > declared {
            return Ok(Update::Keep);
        }
        object.fragments = next_fragment(0, limits)?;
        stored(&mut object.encoded, data.len(), declared, limits)?;
        object.encoded.try_extend_from_slice(data)?;
        object.declared = declared;
        Ok(Update::Replace(object))
    }

    /// A continuation fragment's data, accepted while it fits the length the
    /// first fragment declared. Room for it is reserved here.
    fn continuation<'a>(&mut self, data: &'a [u8], limits: &DisplayLimits) -> Result<Update<'a>> {
        if self.width == 0 || data.len() > self.declared - self.encoded.len() {
            return Ok(Update::Keep);
        }
        next_fragment(self.fragments, limits)?;
        stored(&mut self.encoded, data.len(), self.declared, limits)?;
        Ok(Update::Append(data))
    }

    /// Whether the object has a bitmap to decode.
    pub(super) fn has_data(&self) -> bool {
        self.width != 0 && !self.encoded.is_empty()
    }

    /// Decodes the object into `plane` (`width * height` bytes), returning
    /// whether its RLE wrote every pixel. Only then is the plane output, so no
    /// byte it left unwritten is ever read.
    #[inline(never)]
    pub(super) fn fill(&self, plane: &mut [u8]) -> bool {
        super::rle::decode(self.encoded.as_slice(), usize::from(self.height), plane)
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
    /// though a new id still takes a slot; one that declares a negative data
    /// length leaves it with the new size and no data. The fragment is
    /// checked and its storage allocated before the cache changes, so an
    /// error leaves the cache as it was.
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
        let refusal = matches!(update, Update::Unallocated(_)).then_some(Refusal::ObjectLength);
        match (index, update) {
            (Some(index), Update::Replace(object) | Update::Unallocated(object)) => {
                self.objects.as_mut_slice()[index] = object;
            }
            (Some(index), Update::Append(data)) => {
                let object = &mut self.objects.as_mut_slice()[index];
                object.encoded.try_extend_from_slice(data)?;
                object.fragments += 1;
            }
            (Some(_), Update::Keep) => {}
            (None, Update::Replace(object) | Update::Unallocated(object)) => {
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

/// The matrix the receiver converts palette entries with, chosen by canvas height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColourMatrix {
    Sd601,
    Hd709,
}

impl ColourMatrix {
    fn for_canvas(height: u16) -> Self {
        if height != 0 && height <= 576 {
            Self::Sd601
        } else {
            Self::Hd709
        }
    }

    /// The receiver's integer conversion: Y/Cr/Cb scaled by rounded 10-bit
    /// fixed-point coefficients of the matrix, each channel rounded and
    /// clamped. This states how the receiver converts, not the source's
    /// colour primaries or transfer.
    fn argb(self, y: u8, cr: u8, cb: u8, alpha: u8) -> u32 {
        let matrix = match self {
            Self::Sd601 => YcbcrMatrix::BT601,
            Self::Hd709 => YcbcrMatrix::BT709,
        };
        matrix.argb(y, cr, cb, alpha, ChannelRounding::Nearest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::TimeBase;

    fn first(length: u32, width: u16, height: u16, data: &[u8]) -> Vec<u8> {
        let mut fragment = length.to_be_bytes()[1..].to_vec();
        fragment.extend(width.to_be_bytes());
        fragment.extend(height.to_be_bytes());
        fragment.extend_from_slice(data);
        fragment
    }

    /// What a first fragment does to object 1 on a 16 × 4 canvas, as the
    /// reference showed it (`ods_first` probe): `keep` leaves the object as it
    /// was, `refuse` refuses the packet, and a replacement shows its declared
    /// RLE length.
    fn outcome(fragment: &[u8]) -> String {
        let budget = Budget::new(1 << 20).unwrap();
        let update = Object::start(1, fragment, [16, 4], &DisplayLimits::default(), &budget);
        match update.unwrap() {
            Update::Keep => "keep".into(),
            Update::Unallocated(object) => {
                assert_eq!(object.declared, 0);
                assert!(!object.has_data());
                format!("refuse {}x{}", object.width, object.height)
            }
            Update::Replace(object) => {
                assert_eq!(object.fragments, 1);
                assert!(object.encoded.capacity() <= 16);
                let data = object.encoded.as_slice();
                assert_eq!(data, &fragment[7..]);
                format!("{}x{} {}", object.width, object.height, object.declared)
            }
            Update::Append(_) => unreachable!(),
        }
    }

    #[test]
    fn a_first_fragment_is_checked_against_the_canvas_then_its_length() {
        let rle = [2, 2, 0, 0];
        for (w, h) in [(0, 1), (1, 0), (0, 0), (17, 1), (1, 5), (17, 5)] {
            assert_eq!(outcome(&first(8, w, h, &rle)), "keep", "{w}x{h}");
        }
        for (w, h) in [(2, 1), (16, 4), (16, 1), (1, 4)] {
            assert_eq!(outcome(&first(8, w, h, &rle)), format!("{w}x{h} 4"));
        }
        for length in 0..4 {
            assert_eq!(outcome(&first(length, 2, 1, &[])), "keep");
            assert_eq!(outcome(&first(length, 2, 1, &[2])), "refuse 2x1");
            assert_eq!(outcome(&first(length, 2, 1, &rle)), "refuse 2x1");
            assert_eq!(outcome(&first(length, 17, 1, &rle)), "keep");
            assert_eq!(outcome(&first(length, 0, 1, &rle)), "keep");
        }
        let six = [2, 2, 0, 0, 1, 1];
        for (length, data) in [(4, &[][..]), (4, &rle), (5, &rle), (7, &rle), (8, &six)] {
            assert_eq!(outcome(&first(length, 2, 1, data)), "keep", "{length}");
        }
        for (length, data) in [(8, &rle[..]), (9, &rle), (12, &rle), (8, &rle[..2])] {
            assert_eq!(
                outcome(&first(length, 2, 1, data)),
                format!("2x1 {}", length - 4)
            );
        }
        assert_eq!(outcome(&first(0xff_ffff, 2, 1, &rle)), "2x1 16777211");
        // Cut inside the size fields, or right after them.
        let whole = first(8, 2, 1, &rle);
        for cut in 0..8 {
            assert_eq!(outcome(&whole[..cut]), "keep", "{cut}");
        }
    }

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
