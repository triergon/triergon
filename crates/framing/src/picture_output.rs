//! The output order of decoded H.264 and HEVC pictures: the decoded picture
//! buffer of ITU-T H.264 Annex C.4 and H.265 Annex C.5.2 with the picture
//! order counts of H.264 8.2.1 and H.265 8.3.1, without decoding pixels.
//!
//! [`DecodedPictures`] holds the pictures a decoder keeps, each with a payload
//! of the caller's choosing: a decoded picture for a video decoder, the
//! picture's caption bytes for caption extraction. The caller drives it with
//! the same events for every picture, in decode order.
//!
//! H.264 (C.4):
//!
//! 1. the frames of a gap in `frame_num`, each stored by
//!    [`DecodedPictures::avc_store_gap_frame`] (C.4.2, 8.2.5.2);
//! 2. after the picture is decoded, its reference marking (8.2.5), applied
//!    through [`DecodedPictures::entries_mut`] or
//!    [`DecodedPictures::retain_references`];
//! 3. removal before it is stored: [`DecodedPictures::avc_remove`] (C.4.4);
//! 4. storage: [`DecodedPictures::avc_store`] (C.4.5.1, C.4.5.2), or
//!    [`DecodedPictures::avc_store_second_field`] for the second field of a
//!    pair whose first field is stored.
//!
//! H.265 (C.5.2):
//!
//! 1. before the picture is decoded, the marking of its reference picture set
//!    (8.3.2) and output and removal: [`DecodedPictures::hevc_remove`]
//!    (C.5.2.2);
//! 2. after it is decoded, storage and additional bumping:
//!    [`DecodedPictures::hevc_store`] (C.3.4, C.5.2.3).
//!
//! At the end of the stream, [`DecodedPictures::flush`]. Every picture that
//! leaves is reported to a sink as a [`Leave`]: output (the "bumping" process,
//! H.264 C.4.5.3, H.265 C.5.2.4), then removed, or removed without output.
//! [`DecodedPictures::store`] stores a picture without any process, for a
//! caller that drives output itself.
//!
//! The processes follow the specifications. FFmpeg's decoders were observed
//! to output in other ways (see the caption extraction of `triergon-subtitles`
//! for the observations); the steps that reproduce them are separate, so the
//! specified processes stay intact:
//!
//! * [`DecodedPictures::bump_while_waiting_more_than`]: a picture leaves as
//!   soon as more pictures wait for output than a reorder depth, as H.265
//!   C.5.2.3 does with `sps_max_num_reorder_pics`; observed for H.264, where
//!   C.4.5 waits until the buffer is full.
//! * [`DecodedPictures::bump_while_holding`]: room is made while the buffer
//!   holds a number of pictures, before the reference picture set applies;
//!   observed for HEVC at `sps_max_dec_pic_buffering_minus1 + 2` pictures, one
//!   more than C.5.2.2 allows after it applies (pass
//!   [`HevcLimits::max_dec_pic_buffering`] as `None` to leave the specified
//!   check out).
//! * [`LateOutput`]: a picture that leaves after a picture with a higher
//!   order count has been output is dropped; observed for H.264.
//!
//! [`order`] derives the picture order counts, with the observed H.264 rules
//! as options: [`order::AfterMmco5::CodedLsb`] and
//! [`order::AfterGap::OffsetKept`].

pub mod levels;
pub mod order;

use triergon_core::{Budget, BudgetVec, Error, Result};

/// A picture leaving the buffer, in the order the processes produce them.
#[derive(Debug, PartialEq, Eq)]
pub enum Leave<'a, P> {
    /// Output by the bumping process: cropped and output, and marked as not
    /// needed for output. It stays stored while it is used for reference.
    Output { poc: i32, picture: &'a P },
    /// Its buffer is emptied. `output` says whether it was output before;
    /// a picture removed while still needed for output is discarded.
    Removed { poc: i32, picture: P, output: bool },
}

/// A stored picture, named for as long as it is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PictureId(u32);

/// How a field or frame is marked for reference (H.264 8.2.5, H.265 8.3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Marking {
    #[default]
    Unused,
    ShortTerm,
    LongTerm,
}

/// A field of a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    Top,
    Bottom,
}

impl Parity {
    const fn index(self) -> usize {
        match self {
            Self::Top => 0,
            Self::Bottom => 1,
        }
    }
}

/// One stored frame, complementary field pair or non-paired field.
#[derive(Clone, Copy, Debug)]
struct Stored<P> {
    id: PictureId,
    /// PicOrderCnt( ) of the frame or pair (8-1), or of the field.
    poc: i32,
    /// The marking of the top and the bottom field (both alike for a frame).
    marking: [Marking; 2],
    /// Marked as "needed for output".
    waiting: bool,
    /// Output by the bumping process.
    output: bool,
    /// Kept out of bumping by the caller, such as a first field waiting for
    /// its second.
    held: bool,
    /// PicLatencyCount (H.265 C.5.2.3).
    latency: u32,
    picture: P,
}

impl<P> Stored<P> {
    fn reference(&self) -> bool {
        self.marking != [Marking::Unused; 2]
    }
}

/// A stored picture, for the caller to read and mark.
#[derive(Debug)]
pub struct Entry<'a, P>(&'a mut Stored<P>);

impl<P> Entry<'_, P> {
    #[must_use]
    pub fn id(&self) -> PictureId {
        self.0.id
    }

    /// PicOrderCnt( ) of the frame, pair or field.
    #[must_use]
    pub fn poc(&self) -> i32 {
        self.0.poc
    }

    #[must_use]
    pub fn marking(&self, field: Parity) -> Marking {
        self.0.marking[field.index()]
    }

    /// Marks one field, or with `None` both fields of the frame.
    pub fn set_marking(&mut self, field: Option<Parity>, marking: Marking) {
        match field {
            Some(field) => self.0.marking[field.index()] = marking,
            None => self.0.marking = [marking; 2],
        }
    }

    /// Either field is used for reference.
    #[must_use]
    pub fn is_reference(&self) -> bool {
        self.0.reference()
    }

    /// Marked as "needed for output".
    #[must_use]
    pub fn is_waiting(&self) -> bool {
        self.0.waiting
    }

    #[must_use]
    pub fn was_output(&self) -> bool {
        self.0.output
    }

    #[must_use]
    pub fn is_held(&self) -> bool {
        self.0.held
    }

    /// Lets bumping output the picture: a held first field whose second
    /// field the caller no longer expects.
    pub fn release(&mut self) {
        self.0.held = false;
    }

    #[must_use]
    pub fn picture(&self) -> &P {
        &self.0.picture
    }

    pub fn picture_mut(&mut self) -> &mut P {
        &mut self.0.picture
    }
}

/// A picture to store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewPicture {
    /// PicOrderCnt( ) (for H.264 after decoding: 0 after
    /// `memory_management_control_operation` 5).
    pub poc: i32,
    /// OutputFlag (H.264) or PicOutputFlag (H.265).
    pub output: bool,
    pub marking: Marking,
    /// The field, for a field picture.
    pub field: Option<Parity>,
    /// Kept out of bumping until [`Entry::release`] or its second field.
    pub hold: bool,
}

/// A picture the buffer did not take, given back.
#[derive(Debug)]
pub struct Refused<P> {
    pub error: Error,
    pub picture: P,
}

/// Why an H.264 picture was not stored.
#[derive(Debug)]
pub enum StoreError<P> {
    /// Every frame buffer holds a picture used for reference and none waits
    /// for output: the stream overflows the buffer.
    Full(P),
    Refused(Refused<P>),
}

/// The decoded picture buffer.
#[derive(Debug)]
pub struct DecodedPictures<P> {
    stored: BudgetVec<Stored<P>>,
    next_id: u32,
}

/// The H.265 values that bound the buffer, those of the highest temporal
/// sub-layer (`HighestTid`) of the active SPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcLimits {
    /// `sps_max_dec_pic_buffering_minus1 + 1`, or `None` to leave the
    /// fullness condition of C.5.2.2 out.
    pub max_dec_pic_buffering: Option<u32>,
    /// `sps_max_num_reorder_pics`.
    pub max_num_reorder: u32,
    /// `SpsMaxLatencyPictures` (7-9), `None` when
    /// `sps_max_latency_increase_plus1` is 0.
    pub max_latency_pictures: Option<u32>,
}

impl HevcLimits {
    /// The limits of the SPS values of one sub-layer (7.4.3.2).
    #[must_use]
    pub fn from_sps(
        max_dec_pic_buffering_minus1: u32,
        max_num_reorder_pics: u32,
        max_latency_increase_plus1: u32,
    ) -> Self {
        Self {
            max_dec_pic_buffering: Some(max_dec_pic_buffering_minus1.saturating_add(1)),
            max_num_reorder: max_num_reorder_pics,
            max_latency_pictures: (max_latency_increase_plus1 != 0).then(|| {
                max_num_reorder_pics
                    .saturating_add(max_latency_increase_plus1)
                    .saturating_sub(1)
            }),
        }
    }
}

/// What C.5.2.2 needs of the current H.265 picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HevcPicture {
    /// An IRAP picture with NoRaslOutputFlag equal to 1 (8.1.3).
    pub irap_no_rasl_output: bool,
    /// A CRA picture.
    pub cra: bool,
    /// The first picture of the bitstream ("picture 0").
    pub first: bool,
    /// `no_output_of_prior_pics_flag`, or 1 where the decoder infers it
    /// from a change of picture size or buffer size (C.5.2.2).
    pub no_output_of_prior_pics: bool,
}

/// What C.4.4 needs of the current H.264 picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvcPicture {
    pub idr: bool,
    /// The picture has `memory_management_control_operation` equal to 5.
    pub mmco5: bool,
    /// `no_output_of_prior_pics_flag` of an IDR picture, or 1 where it is
    /// inferred from a change of picture or buffer size (C.4.4).
    pub no_output_of_prior_pics: bool,
}

/// How the current H.264 picture is stored (C.4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcStore {
    /// `PicOrderCnt( )` of the picture (after decoding: 0 after
    /// `memory_management_control_operation` 5).
    pub poc: i32,
    /// Its marking after decoding; `Unused` for a non-reference picture.
    pub marking: Marking,
    /// OutputFlag.
    pub output: bool,
    /// The field, for a field picture.
    pub field: Option<Parity>,
    /// Keep the picture out of bumping, as a first field waiting for its
    /// second.
    pub hold: bool,
}

impl<P> DecodedPictures<P> {
    #[must_use]
    pub fn new(pool: &Budget) -> Self {
        Self {
            stored: BudgetVec::new(pool),
            next_id: 0,
        }
    }

    /// The pictures stored: the DPB fullness.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stored.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }

    /// The pictures needed for output.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.stored.iter().filter(|p| p.waiting).count()
    }

    /// The stored pictures, in storage order.
    pub fn entries_mut(&mut self) -> impl Iterator<Item = Entry<'_, P>> {
        self.stored.iter_mut().map(Entry)
    }

    /// The stored picture `id`.
    pub fn entry(&mut self, id: PictureId) -> Option<Entry<'_, P>> {
        self.stored.iter_mut().find(|p| p.id == id).map(Entry)
    }

    /// Removes the stored picture `id` without output and gives its payload
    /// back.
    pub fn take(&mut self, id: PictureId) -> Option<P> {
        let at = self.stored.iter().position(|p| p.id == id)?;
        self.stored.as_mut_slice()[at..].rotate_left(1);
        self.stored.pop().map(|p| p.picture)
    }

    /// Marks both fields of the pictures `keep` refuses (given each picture's
    /// order count and payload) as unused for reference (H.265 8.3.2; H.264
    /// 8.2.5).
    pub fn retain_references(&mut self, mut keep: impl FnMut(i32, &P) -> bool) {
        for p in self.stored.iter_mut() {
            if !keep(p.poc, &p.picture) {
                p.marking = [Marking::Unused; 2];
            }
        }
    }

    /// Marks every picture as unused for reference (an IDR picture, an IRAP
    /// picture with NoRaslOutputFlag equal to 1,
    /// `memory_management_control_operation` 5).
    pub fn mark_unused(&mut self) {
        self.retain_references(|_, _| false);
    }

    /// Stores a picture without bumping or removal, and names it.
    ///
    /// # Errors
    ///
    /// When the pool refuses the memory; the picture is given back.
    pub fn store(
        &mut self,
        new: NewPicture,
        picture: P,
    ) -> std::result::Result<PictureId, Refused<P>> {
        let mut marking = [new.marking; 2];
        if let Some(field) = new.field {
            marking[1 - field.index()] = Marking::Unused;
        }
        let id = PictureId(self.next_id);
        self.stored
            .try_push(Stored {
                id,
                poc: new.poc,
                marking,
                waiting: new.output,
                output: false,
                held: new.hold,
                latency: 0,
                picture,
            })
            .map_err(|rejected| Refused {
                error: rejected.error,
                picture: rejected.value.picture,
            })?;
        self.next_id = self.next_id.wrapping_add(1);
        Ok(id)
    }

    /// Empties the buffers of pictures neither needed for output nor used for
    /// reference.
    pub fn remove_unused(&mut self, mut leave: impl FnMut(Leave<'_, P>)) {
        let mut at = 0;
        while at < self.stored.len() {
            if self.stored[at].waiting || self.stored[at].reference() {
                at += 1;
            } else {
                self.remove(at, &mut leave);
            }
        }
    }

    /// The "bumping" process (H.264 C.4.5.3, H.265 C.5.2.4): outputs the
    /// picture needed for output with the smallest order count, among those
    /// not held, and empties its buffer when it is unused for reference.
    /// False when no such picture waits.
    pub fn bump(&mut self, mut leave: impl FnMut(Leave<'_, P>)) -> bool {
        let Some(at) = self.first_waiting() else {
            return false;
        };
        let p = &mut self.stored[at];
        p.waiting = false;
        p.output = true;
        leave(Leave::Output {
            poc: p.poc,
            picture: &p.picture,
        });
        if !p.reference() && !p.held {
            self.remove(at, &mut leave);
        }
        true
    }

    /// Outputs every picture needed for output, then empties every buffer.
    pub fn flush(&mut self, mut leave: impl FnMut(Leave<'_, P>)) {
        for p in self.stored.iter_mut() {
            p.held = false;
        }
        while self.bump(&mut leave) {}
        self.discard(leave);
    }

    /// Empties every buffer without output.
    pub fn discard(&mut self, mut leave: impl FnMut(Leave<'_, P>)) {
        while !self.stored.is_empty() {
            self.remove(0, &mut leave);
        }
    }

    /// Bumps while more than `pictures` pictures are needed for output.
    pub fn bump_while_waiting_more_than(
        &mut self,
        pictures: usize,
        mut leave: impl FnMut(Leave<'_, P>),
    ) {
        while self.waiting() > pictures && self.bump(&mut leave) {}
    }

    /// Bumps while the buffer holds at least `pictures` pictures and one of
    /// them is needed for output.
    pub fn bump_while_holding(&mut self, pictures: usize, mut leave: impl FnMut(Leave<'_, P>)) {
        while self.stored.len() >= pictures && self.bump(&mut leave) {}
    }

    /// H.264 C.4.4: removal of pictures before the current picture is stored,
    /// after it is decoded and the marking of 8.2.5 applied. An IDR picture
    /// marks every picture unused here.
    pub fn avc_remove(&mut self, current: AvcPicture, mut leave: impl FnMut(Leave<'_, P>)) {
        if current.idr {
            self.mark_unused();
            if current.no_output_of_prior_pics {
                self.discard(leave);
                return;
            }
        }
        self.remove_unused(&mut leave);
        if current.mmco5 || current.idr {
            self.flush(leave);
        }
    }

    /// H.264 C.4.5: stores the current picture in a buffer of `dpb_frames`
    /// frame buffers, bumping while it is full. A non-reference picture with a
    /// lower order count than every picture needed for output is output
    /// without being stored when the buffer is full (C.4.5.2): then `None`.
    /// A non-reference picture that is not output is not stored either.
    ///
    /// # Errors
    ///
    /// [`StoreError::Full`] when every buffer holds a reference picture and
    /// none waits for output; [`StoreError::Refused`] when the pool refuses
    /// the memory. The picture is given back.
    pub fn avc_store(
        &mut self,
        current: AvcStore,
        picture: P,
        dpb_frames: usize,
        mut leave: impl FnMut(Leave<'_, P>),
    ) -> std::result::Result<Option<PictureId>, StoreError<P>> {
        let reference = current.marking != Marking::Unused;
        if !reference && !current.output {
            return Ok(None);
        }
        while self.stored.len() >= dpb_frames.max(1) {
            if !reference
                && current.output
                && self
                    .stored
                    .iter()
                    .filter(|p| p.waiting)
                    .all(|p| current.poc < p.poc)
            {
                leave(Leave::Output {
                    poc: current.poc,
                    picture: &picture,
                });
                leave(Leave::Removed {
                    poc: current.poc,
                    picture,
                    output: true,
                });
                return Ok(None);
            }
            if !self.bump(&mut leave) {
                return Err(StoreError::Full(picture));
            }
        }
        let new = NewPicture {
            poc: current.poc,
            output: current.output,
            marking: current.marking,
            field: current.field,
            hold: current.hold,
        };
        self.store(new, picture)
            .map(Some)
            .map_err(StoreError::Refused)
    }

    /// H.264 C.4.5.1/C.4.5.2 for the second field of a complementary pair
    /// whose first field is stored as `first`: the field joins its frame
    /// buffer, `merge` taking its payload into the first field's. When the
    /// first field has already been output, the second is output on its
    /// own before it joins; otherwise the pair waits for output when the
    /// field's OutputFlag is 1. The pair is no longer held.
    ///
    /// # Errors
    ///
    /// When `first` is not stored; the field is given back.
    pub fn avc_store_second_field(
        &mut self,
        first: PictureId,
        current: AvcStore,
        picture: P,
        mut leave: impl FnMut(Leave<'_, P>),
        merge: impl FnOnce(&mut P, P),
    ) -> std::result::Result<(), P> {
        let Some(p) = self.stored.iter_mut().find(|p| p.id == first) else {
            return Err(picture);
        };
        if let Some(field) = current.field {
            p.marking[field.index()] = current.marking;
        }
        p.held = false;
        if current.output {
            if p.output {
                leave(Leave::Output {
                    poc: current.poc,
                    picture: &picture,
                });
            } else {
                p.waiting = true;
                p.poc = p.poc.min(current.poc);
            }
        }
        merge(&mut p.picture, picture);
        Ok(())
    }

    /// H.264 C.4.2 for one frame of a gap in `frame_num` (8.2.5.2): a
    /// "non-existing" frame, marked as used for short-term reference and not
    /// needed for output, stored after bumping while the buffer of
    /// `dpb_frames` frame buffers is full.
    ///
    /// # Errors
    ///
    /// As [`DecodedPictures::avc_store`].
    pub fn avc_store_gap_frame(
        &mut self,
        poc: i32,
        picture: P,
        dpb_frames: usize,
        mut leave: impl FnMut(Leave<'_, P>),
    ) -> std::result::Result<PictureId, StoreError<P>> {
        while self.stored.len() >= dpb_frames.max(1) {
            if !self.bump(&mut leave) {
                return Err(StoreError::Full(picture));
            }
        }
        let new = NewPicture {
            poc,
            output: false,
            marking: Marking::ShortTerm,
            field: None,
            hold: false,
        };
        self.store(new, picture).map_err(StoreError::Refused)
    }

    /// H.265 C.5.2.2: output and removal before the current picture is
    /// decoded, after the caller has applied its reference picture set. An
    /// IRAP picture with NoRaslOutputFlag equal to 1 that is not the first
    /// picture empties the buffer, discarding the pictures it holds when
    /// NoOutputOfPriorPicsFlag is 1, which C.5.2.2 sets for a CRA picture
    /// whatever its `no_output_of_prior_pics_flag`.
    pub fn hevc_remove(
        &mut self,
        current: HevcPicture,
        limits: HevcLimits,
        mut leave: impl FnMut(Leave<'_, P>),
    ) {
        if current.irap_no_rasl_output && !current.first {
            if current.cra || current.no_output_of_prior_pics {
                self.discard(leave);
            } else {
                self.remove_unused(&mut leave);
                self.flush(leave);
            }
            return;
        }
        self.remove_unused(&mut leave);
        loop {
            let full = limits
                .max_dec_pic_buffering
                .is_some_and(|size| self.stored.len() >= size as usize);
            if !(full || self.over_reorder_or_latency(limits)) || !self.bump(&mut leave) {
                break;
            }
        }
    }

    /// H.265 C.3.4 and C.5.2.3: stores the current picture (marked as used
    /// for short-term reference, and needed for output when PicOutputFlag is
    /// 1), then bumps while too many pictures wait or one waits too long.
    ///
    /// # Errors
    ///
    /// When the pool refuses the memory; the picture is given back.
    pub fn hevc_store(
        &mut self,
        poc: i32,
        output: bool,
        picture: P,
        limits: HevcLimits,
        mut leave: impl FnMut(Leave<'_, P>),
    ) -> std::result::Result<PictureId, Refused<P>> {
        if output {
            for p in self.stored.iter_mut().filter(|p| p.waiting && p.poc > poc) {
                p.latency = p.latency.saturating_add(1);
            }
        }
        let new = NewPicture {
            poc,
            output,
            marking: Marking::ShortTerm,
            field: None,
            hold: false,
        };
        let id = self.store(new, picture)?;
        while self.over_reorder_or_latency(limits) && self.bump(&mut leave) {}
        Ok(id)
    }

    fn over_reorder_or_latency(&self, limits: HevcLimits) -> bool {
        self.waiting() > limits.max_num_reorder as usize
            || limits
                .max_latency_pictures
                .is_some_and(|most| self.stored.iter().any(|p| p.waiting && p.latency >= most))
    }

    fn first_waiting(&self) -> Option<usize> {
        self.stored
            .iter()
            .enumerate()
            .filter(|(_, p)| p.waiting && !p.held)
            .min_by_key(|(_, p)| p.poc)
            .map(|(at, _)| at)
    }

    fn remove(&mut self, at: usize, leave: &mut impl FnMut(Leave<'_, P>)) {
        let slice = self.stored.as_mut_slice();
        slice[at..].rotate_left(1);
        if let Some(p) = self.stored.pop() {
            leave(Leave::Removed {
                poc: p.poc,
                picture: p.picture,
                output: p.output,
            });
        }
    }
}

impl<P: Copy> DecodedPictures<P> {
    /// Makes this buffer a copy of `other`, reusing its allocation where it
    /// is large enough. A refused allocation leaves it as it was.
    ///
    /// # Errors
    ///
    /// When the pool refuses the memory.
    pub fn copy_from(&mut self, other: &Self) -> Result<()> {
        if self.stored.capacity() < other.stored.len() {
            self.stored
                .try_reserve(other.stored.len() - self.stored.len())?;
        }
        self.stored.clear();
        self.stored.try_extend_from_slice(other.stored.as_slice())?;
        self.next_id = other.next_id;
        Ok(())
    }
}

/// A picture leaving after one with a higher order count has been output is
/// dropped (observed for H.264). The order restarts at an IDR picture and at
/// `memory_management_control_operation` 5.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LateOutput {
    last: Option<i32>,
}

impl LateOutput {
    /// True when a picture of order count `poc` may be output now; it then
    /// becomes the last picture output.
    pub fn admit(&mut self, poc: i32) -> bool {
        if self.is_late(poc) {
            return false;
        }
        self.last = Some(poc);
        true
    }

    /// True when a picture of order count `poc` would be dropped.
    #[must_use]
    pub fn is_late(&self, poc: i32) -> bool {
        self.last.is_some_and(|last| poc < last)
    }

    /// The order count of the last picture admitted.
    #[must_use]
    pub fn last(&self) -> Option<i32> {
        self.last
    }

    /// A new output order begins.
    pub fn restart(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests;
