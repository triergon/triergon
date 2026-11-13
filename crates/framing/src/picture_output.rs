//! The output order of decoded H.264 and HEVC pictures: the decoded picture
//! buffer of ITU-T H.264 Annex C.4 and H.265 Annex C.5.2 with the picture
//! order counts of H.264 8.2.1 and H.265 8.3.1, without decoding pixels.
//!
//! [`DecodedPictures`] holds the pictures a decoder keeps, each with a payload
//! of the caller's choosing: a decoded picture for a video decoder, the
//! picture's caption bytes for caption extraction. The caller drives it with
//! the same events for every picture, in decode order:
//!
//! 1. marking: [`DecodedPictures::retain_references`] (H.265 8.3.2, H.264
//!    8.2.5) or [`DecodedPictures::mark_unused`];
//! 2. output and removal before the picture is decoded:
//!    [`DecodedPictures::avc_remove`] (H.264 C.4.4) or
//!    [`DecodedPictures::hevc_remove`] (H.265 C.5.2.2);
//! 3. storage after it is decoded: [`DecodedPictures::avc_store`] (H.264
//!    C.4.5) or [`DecodedPictures::hevc_store`] (H.265 C.3.4 and C.5.2.3);
//! 4. at the end of the stream, [`DecodedPictures::flush`].
//!
//! Every picture that leaves is reported to a sink as a [`Leave`]: output
//! (the "bumping" process, H.264 C.4.5.3, H.265 C.5.2.4), then removed, or
//! removed without output.
//!
//! The buffer follows the specifications. FFmpeg's decoders depart from them
//! in places; each departure is a separate step here, so the specified
//! process stays intact:
//!
//! * [`DecodedPictures::bump_while_waiting_more_than`]: FFmpeg's H.264
//!   decoder outputs a picture as soon as more pictures wait for output than
//!   its reorder depth (as H.265 C.5.2.3 does with
//!   `sps_max_num_reorder_pics`), not when the buffer is full.
//! * [`DecodedPictures::bump_while_holding`]: FFmpeg's HEVC decoder makes room
//!   before it applies the current picture's reference picture set, while the
//!   buffer holds `sps_max_dec_pic_buffering_minus1 + 2` pictures, one more
//!   than C.5.2.2 allows (pass [`HevcLimits::max_dec_pic_buffering`] as `None`
//!   to leave the specified check out).
//! * [`LateOutput`]: FFmpeg's H.264 decoder drops a picture that leaves after
//!   a picture with a higher order count has been output.
//!
//! [`order`] derives the picture order counts, with FFmpeg's H.264 departures
//! as options: [`order::AfterMmco5::CodedLsb`] and [`order::AfterGap::OffsetKept`].

pub mod order;

use triergon_core::{Budget, BudgetVec, Result};

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

/// One stored picture.
#[derive(Clone, Copy, Debug)]
struct Stored<P> {
    poc: i32,
    /// Marked as "needed for output".
    waiting: bool,
    /// Output by the bumping process.
    output: bool,
    /// Marked as used for (short- or long-term) reference.
    reference: bool,
    /// PicLatencyCount (H.265 C.5.2.3).
    latency: u32,
    picture: P,
}

/// The decoded picture buffer.
#[derive(Debug)]
pub struct DecodedPictures<P> {
    stored: BudgetVec<Stored<P>>,
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
    /// `PicOrderCnt( )` of the picture (after decoding; 0 after
    /// `memory_management_control_operation` 5).
    pub poc: i32,
    /// `nal_ref_idc` is not 0.
    pub reference: bool,
    /// OutputFlag.
    pub output: bool,
    /// The second field of a complementary field pair whose first field is
    /// the last picture stored: it joins that field's frame buffer.
    pub second_field: bool,
}

impl<P> DecodedPictures<P> {
    #[must_use]
    pub fn new(pool: &Budget) -> Self {
        Self {
            stored: BudgetVec::new(pool),
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

    /// The order counts of the pictures needed for output that follow `poc`.
    #[must_use]
    pub fn waiting_after(&self, poc: i32) -> usize {
        self.stored
            .iter()
            .filter(|p| p.waiting && p.poc > poc)
            .count()
    }

    /// The stored pictures' payloads, in storage order.
    pub fn pictures_mut(&mut self) -> impl Iterator<Item = &mut P> {
        self.stored.iter_mut().map(|p| &mut p.picture)
    }

    /// Marks the pictures whose order count `keep` refuses as unused for
    /// reference (the reference picture set of H.265 8.3.2; H.264 8.2.5).
    pub fn retain_references(&mut self, mut keep: impl FnMut(i32) -> bool) {
        for p in self.stored.iter_mut() {
            p.reference &= keep(p.poc);
        }
    }

    /// Marks every picture as unused for reference (an IDR picture, an IRAP
    /// picture with NoRaslOutputFlag equal to 1,
    /// `memory_management_control_operation` 5).
    pub fn mark_unused(&mut self) {
        self.retain_references(|_| false);
    }

    /// Empties the buffers of pictures neither needed for output nor used for
    /// reference.
    pub fn remove_unused(&mut self, mut leave: impl FnMut(Leave<'_, P>)) {
        let mut at = 0;
        while at < self.stored.len() {
            if self.stored[at].waiting || self.stored[at].reference {
                at += 1;
            } else {
                self.remove(at, &mut leave);
            }
        }
    }

    /// The "bumping" process (H.264 C.4.5.3, H.265 C.5.2.4): outputs the
    /// picture needed for output with the smallest order count and empties
    /// its buffer when it is unused for reference. False when no picture is
    /// needed for output.
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
        if !p.reference {
            self.remove(at, &mut leave);
        }
        true
    }

    /// Outputs every picture needed for output, then empties every buffer.
    pub fn flush(&mut self, mut leave: impl FnMut(Leave<'_, P>)) {
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

    /// H.264 C.4.4: removal of pictures before the current picture is
    /// inserted. The caller has applied the marking of 8.2.5 for a non-IDR
    /// picture; an IDR picture marks every picture unused here.
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
    /// without being stored when the buffer is full (C.4.5.2).
    pub fn avc_store(
        &mut self,
        current: AvcStore,
        picture: P,
        dpb_frames: usize,
        mut leave: impl FnMut(Leave<'_, P>),
    ) -> Result<()> {
        if current.second_field
            && let Some(first) = self.stored.as_mut_slice().last_mut()
        {
            first.waiting |= current.output;
            first.reference |= current.reference;
            first.poc = first.poc.min(current.poc);
            return Ok(());
        }
        if !current.reference && !current.output {
            return Ok(());
        }
        while self.stored.len() >= dpb_frames.max(1) {
            if !current.reference
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
                return Ok(());
            }
            if !self.bump(&mut leave) {
                // Every buffer holds a reference picture: the stream
                // overflows the buffer, which keeps the picture regardless.
                break;
            }
        }
        self.push(current.poc, current.output, current.reference, picture)
    }

    /// H.265 C.5.2.2: output and removal before the current picture is
    /// decoded, after the caller has applied its reference picture set.
    pub fn hevc_remove(
        &mut self,
        current: HevcPicture,
        limits: HevcLimits,
        mut leave: impl FnMut(Leave<'_, P>),
    ) {
        if current.irap_no_rasl_output && !current.first {
            // The CRA rule is the decoder under test's: NoOutputOfPriorPicsFlag
            // is 1 regardless of no_output_of_prior_pics_flag.
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
    pub fn hevc_store(
        &mut self,
        poc: i32,
        output: bool,
        picture: P,
        limits: HevcLimits,
        mut leave: impl FnMut(Leave<'_, P>),
    ) -> Result<()> {
        if output {
            for p in self.stored.iter_mut().filter(|p| p.waiting && p.poc > poc) {
                p.latency = p.latency.saturating_add(1);
            }
        }
        self.push(poc, output, true, picture)?;
        while self.over_reorder_or_latency(limits) && self.bump(&mut leave) {}
        Ok(())
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
            .filter(|(_, p)| p.waiting)
            .min_by_key(|(_, p)| p.poc)
            .map(|(at, _)| at)
    }

    fn push(&mut self, poc: i32, output: bool, reference: bool, picture: P) -> Result<()> {
        self.stored
            .try_push(Stored {
                poc,
                waiting: output,
                output: false,
                reference,
                latency: 0,
                picture,
            })
            .map_err(|rejected| rejected.error)
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
    pub fn copy_from(&mut self, other: &Self) -> Result<()> {
        if self.stored.capacity() < other.stored.len() {
            self.stored
                .try_reserve(other.stored.len() - self.stored.len())?;
        }
        self.stored.clear();
        self.stored.try_extend_from_slice(other.stored.as_slice())
    }
}

/// FFmpeg's H.264 output filter: a picture leaving after a picture with a
/// higher order count has been output is dropped. The order restarts at an
/// IDR picture and at `memory_management_control_operation` 5.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LateOutput {
    last: Option<i32>,
}

impl LateOutput {
    /// True when a picture of order count `poc` may be output now; it then
    /// becomes the last picture output.
    pub fn admit(&mut self, poc: i32) -> bool {
        if self.last.is_some_and(|last| poc < last) {
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

    /// A new output order begins.
    pub fn restart(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests;
