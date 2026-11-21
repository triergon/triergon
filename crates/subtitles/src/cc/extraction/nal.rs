//! The picture output of H.264 and HEVC decoding as far as captions go: which
//! pictures' caption bytes become frames, in which order and with which times.
//!
//! Each access unit (one packet) is one picture, or one field of a field pair.
//! Its slice header and parameter sets drive [`DecodedPictures`], the picture
//! output model of ITU-T H.264 Annex C.4 and H.265 Annex C.5.2, with the steps
//! that reproduce the reference's output where it was observed to differ; the
//! picture's caption bytes are its payload. Captions come from
//! `user_data_registered_itu_t_t35` messages of H.264 SEI and HEVC prefix SEI
//! NAL units; the reference takes none from HEVC suffix SEI. Each observed rule
//! names the `subtitle-cc` corpus case that shows it. The output rules of each
//! codec are in [`avc`] and [`hevc`].
//!
//! **Times.** A frame keeps its packet's PTS. It lasts its packet's duration
//! when the container gives one above 0; otherwise the frame period of its
//! SPS's VUI timing (H.264 frames: two ticks; H.264 fields and HEVC: one tick),
//! truncated to the time base, or 0 when that period is 1/1000 s or less
//! (`h264-vui-fast-ts`); without VUI timing, the duration estimated from the
//! times of the first packets ([`RateEstimate`]: 21 times, or 41 in a time
//! base coarser than 1/2000 s; a common divisor of their steps, else the frame
//! rate whose grid they fit best, one tick with fewer than three;
//! `h264-estimate-ts`, `h264-rate-*-ts`). A packet's time is its DTS, else its
//! PTS. In H.264 without a bitstream restriction, once the start hold's depth
//! rises from 0 to 1 (by an order count step or a B picture in order or one
//! below), the packets after it until the hold ends give the estimate no time
//! when their DTS is absent or equals their PTS, but for those of B pictures;
//! a rise above 1 ends the run (`h264-rate-jump-ts`, `h264-rate-pts-b-ts`,
//! `h264-rate-pts-run-ts`). Frames that need the estimate wait, with those
//! after them, until it has its times, 5,000,000 bytes of packets have been
//! read or the stream ends; without any time it is one tick
//! (`h264-untimed-ts`, `h264-untimed-far-ts`, `h264-untimed-bytes-ts`).
//!
//! H.264 in length-prefixed framing without packet durations or VUI timing
//! (Matroska) takes the presentation times of the packets after the first
//! picture output, in presentation order ([`Shown`]); until that estimate is
//! settled each frame lasts 0, or one tick in a time base coarser than
//! 1/1000 s, and at least one picture waits in the buffer (`h264-nodur-mkv`,
//! `h264-nodur-fine-mkv`, `h264-nodur-coarse-mkv`). Not reproduced: start
//! holds displaced by three or more pictures settle 1 to 5 packets later in
//! the reference, a second B picture dropped as late adds a packet, and at
//! ticks between 1 and 2 ms the reference picks a multiple of a 10 fps rate.
//!
//! A picture whose parameter sets are missing or invalid is not decoded. The
//! sets come from the access units or, before the first, from the track's
//! decoder configuration record (`avcC`, `hvcC`), as the reference's decoders
//! take the container's extradata.
//!
//! A push or the finish either succeeds or leaves the decoder and its output as
//! they were. A step first reads its access unit, which only adds caption bytes
//! and parameter sets (cut back and put back when the step fails), then
//! reserves all the memory the rest of it can need: room for its pictures, for
//! every picture to leave, and for their frames. Only then does it change the
//! picture buffer and the output, and from there on nothing can fail.

mod avc;
mod hevc;

use super::captions::Gathered;
use super::queue::FrameQueue;
use super::rate::{RateEstimate, Shown};
use crate::cc::video;
use crate::cc::video::hevc::Reference;
use crate::cc::video::nal::{
    Codec, Framing, SEI_RECOVERY_POINT, SEI_USER_DATA_REGISTERED, Timing, configuration_units,
    nal_units, sei_messages,
};
use avc::{AvcState, BWindow};
use hevc::HevcState;
use triergon_bitstream::{BitSource, Rbsp};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::hevc as kinds;
use triergon_framing::picture_output::{DecodedPictures, Leave};

/// A picture waiting in the buffer: its times and its caption bytes in the
/// gathered bytes.
#[derive(Clone, Copy, Debug)]
struct Held {
    pts: Option<i64>,
    /// The packet's duration, or the VUI's frame period; `None` for the
    /// estimate from the decode times at output.
    duration: Option<i64>,
    start: usize,
    end: usize,
    /// H.264: the stream had recovered when it was decoded.
    recovered: bool,
    /// H.264: an intra picture decoded before the stream recovered.
    key: bool,
    /// H.264: decoded before recovery, the how-manyth picture above the first
    /// intra picture (0 for none).
    above: u32,
    /// H.264: the stream's first intra picture before recovery.
    first_key: bool,
}

/// The decoder's state between steps, besides its buffers.
#[derive(Clone, Copy, Debug)]
struct Progress {
    avc: AvcState,
    hevc: HevcState,
    /// Bytes of pictures not decoded, for the next picture decoded (HEVC):
    /// they run from here to the end of the gathered bytes.
    pending: Option<usize>,
    timing: RateEstimate,
    /// H.264 in length-prefixed framing with presentation times only: the
    /// estimate from those (decided at the first picture).
    shown: Option<Shown>,
    /// The duration estimate, once settled.
    estimate: Option<i64>,
    /// Frames are kept in the output until the duration estimate is settled
    /// (when one of them needs it) and the start choice is made.
    holding: bool,
    needs_estimate: bool,
    /// The frame kept that the start choice dropped, by its place among them.
    skip: Option<usize>,
}

impl Progress {
    fn new(time_base: (u32, u32)) -> Self {
        Self {
            avc: AvcState::new(),
            hevc: HevcState::default(),
            pending: None,
            timing: RateEstimate::new(time_base),
            shown: None,
            estimate: None,
            holding: false,
            needs_estimate: false,
            skip: None,
        }
    }

    /// Whether the duration estimate has every time it takes.
    fn settled(&self) -> bool {
        self.shown
            .map_or_else(|| self.timing.settled(), |shown| shown.duration().is_some())
    }
}

/// The parameter sets of either codec.
#[derive(Debug)]
enum Tables {
    Avc(video::avc::Tables),
    Hevc(video::hevc::Tables),
}

/// A parameter set a step replaced, to put back when it fails.
#[derive(Debug)]
enum Replaced {
    AvcSps(u8, Option<video::avc::Sps>),
    AvcPps(u8, Option<triergon_framing::avc::AvcPps>),
    HevcSps(video::hevc::ReplacedSps),
    HevcPps(u8, Option<video::hevc::Pps>),
}

/// Access units in decode order.
#[derive(Debug)]
pub(super) struct NalDecode {
    codec: Codec,
    framing: Framing,
    time_base: (u32, u32),
    tables: Tables,
    pictures: DecodedPictures<Held>,
    captions: Gathered,
    progress: Progress,
    replaced: BudgetVec<Replaced>,
    references: BudgetVec<Reference>,
    leaving: BudgetVec<(i32, Held)>,
    sei: BudgetVec<u8>,
}

/// What an access unit holds.
#[derive(Clone, Copy, Debug)]
enum Picture {
    None,
    Avc {
        slice: video::avc::Slice,
        /// The other field of the frame, in the same packet.
        second: Option<video::avc::Slice>,
        /// A recovery point SEI's `recovery_frame_cnt`.
        recovery: Option<u32>,
    },
    Hevc {
        slice: video::hevc::Slice,
        /// An end of sequence or end of bitstream NAL unit follows it.
        end_after: bool,
    },
}

/// An access unit as read, before it changes the decoder.
#[derive(Clone, Copy, Debug)]
struct Unit {
    picture: Picture,
    /// Where its caption bytes start in the gathered bytes (HEVC: with those
    /// of the pictures not decoded before it).
    start: usize,
    /// HEVC: an end of sequence or end of bitstream NAL unit came after the
    /// last picture decoded and before this one's, and the ids of the SPSs
    /// whose content the unit changed.
    after_end: bool,
    changed_sps: u16,
}

impl NalDecode {
    pub(super) fn new(
        pool: &Budget,
        codec: Codec,
        framing: Framing,
        time_base: (u32, u32),
    ) -> Self {
        let progress = Progress::new(time_base);
        Self {
            codec,
            framing,
            time_base,
            tables: match codec {
                Codec::H264 => Tables::Avc(video::avc::Tables::new(pool)),
                Codec::Hevc => Tables::Hevc(video::hevc::Tables::new(pool)),
            },
            pictures: DecodedPictures::new(pool),
            captions: Gathered::new(pool),
            progress,
            replaced: BudgetVec::new(pool),
            references: BudgetVec::new(pool),
            leaving: BudgetVec::new(pool),
            sei: BudgetVec::new(pool),
        }
    }

    pub(super) fn push(
        &mut self,
        data: &[u8],
        pts: Option<i64>,
        dts: Option<i64>,
        duration: Option<i64>,
        out: &mut FrameQueue,
    ) -> Result<()> {
        self.step(|decode| {
            let unit = decode.read(data)?;
            let bytes = decode.captions.bytes.len() - unit.start;
            decode.reserve(bytes, out)?;
            decode.access_unit(unit, data.len(), pts, dts, duration, out);
            decode.settle(out);
            Ok(())
        })
    }

    pub(super) fn finish(&mut self, out: &mut FrameQueue) -> Result<()> {
        self.step(|decode| {
            decode.reserve(0, out)?;
            if decode.progress.avc.deferred {
                decode.resolve_recovery();
            }
            if decode.progress.avc.b_window == BWindow::Open {
                decode.progress.avc.b_window = BWindow::Closed;
                decode.choose_start();
            }
            // The estimate has all the times it gets: the frames kept leave,
            // and those still waiting follow them.
            decode.progress.timing.end();
            decode.settle(out);
            decode.pictures.flush(collect(&mut decode.leaving));
            match decode.codec {
                Codec::H264 => decode.emit_avc(out),
                Codec::Hevc => decode.emit(out),
            }
            decode.progress.pending = None;
            decode.progress.avc.first_field = None;
            Ok(())
        })
    }

    /// Runs `step`, which fails only before it changes anything but the
    /// caption bytes and the parameter sets; those are put back when it does.
    fn step(&mut self, step: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let captions = self.captions.mark();
        let result = step(self);
        if result.is_err() {
            self.captions.rollback(captions);
            self.restore();
        } else {
            self.compact();
        }
        self.replaced.clear();
        result
    }

    /// Reserves all the memory the rest of a step can need, `bytes` being the
    /// caption bytes of its access unit: room for two more pictures (the
    /// fields of a frame), for every picture to leave and for the frames they
    /// make.
    fn reserve(&mut self, bytes: usize, out: &mut FrameQueue) -> Result<()> {
        let frames = self.pictures.len() + 2;
        let bytes = bytes
            + self
                .pictures
                .payloads()
                .map(|held| held.end - held.start)
                .sum::<usize>();
        self.pictures.try_reserve(2)?;
        self.leaving.try_reserve(frames)?;
        out.reserve(frames, bytes)
    }

    /// Puts back the parameter sets of a failed step, last replaced first.
    fn restore(&mut self) {
        while let Some(replaced) = self.replaced.pop() {
            match (&mut self.tables, replaced) {
                (Tables::Avc(t), Replaced::AvcSps(id, old)) => t.restore_sps(id, old),
                (Tables::Avc(t), Replaced::AvcPps(id, old)) => t.restore_pps(id, old),
                (Tables::Hevc(t), Replaced::HevcSps(old)) => t.restore_sps(old),
                (Tables::Hevc(t), Replaced::HevcPps(id, old)) => t.restore_pps(id, old),
                _ => {}
            }
        }
        match &mut self.tables {
            Tables::Avc(t) => t.forget_repeats(),
            Tables::Hevc(t) => t.forget_repeats(),
        }
    }

    /// Reads an access unit: its parameter sets (kept in the tables), its
    /// caption bytes (gathered) and its picture's first slice.
    fn read(&mut self, data: &[u8]) -> Result<Unit> {
        let mut unit = Unit {
            picture: Picture::None,
            start: self.progress.pending.unwrap_or(self.captions.bytes.len()),
            after_end: self.progress.hevc.after_end,
            changed_sps: 0,
        };
        let mut recovery = None;
        for nal in nal_units(data, self.framing) {
            let Some(kind) = self.codec.nal_type(nal) else {
                continue;
            };
            let payload = self.codec.payload(nal);
            if self.codec.is_sei(kind) {
                // The reference takes no captions from HEVC suffix SEI.
                if !(self.codec == Codec::Hevc && kind == kinds::SUFFIX_SEI) {
                    recovery = recovery.or(self.read_sei(payload)?);
                }
                continue;
            }
            unit.changed_sps |= self.parameter_set(kind, payload)?;
            match (&mut self.tables, kind) {
                (
                    Tables::Avc(tables),
                    triergon_framing::avc::SLICE | triergon_framing::avc::IDR,
                ) => match &mut unit.picture {
                    Picture::None => {
                        if let Some(slice) = tables.slice(nal) {
                            unit.picture = Picture::Avc {
                                slice,
                                second: None,
                                recovery: None,
                            };
                        }
                    }
                    Picture::Avc {
                        slice: first,
                        second: second @ None,
                        ..
                    } => {
                        *second = tables.slice(nal).filter(|slice| {
                            slice.header.bottom_field.is_some()
                                && slice.header.bottom_field != first.header.bottom_field
                        });
                    }
                    _ => {}
                },
                (Tables::Hevc(_), kinds::EOS | kinds::EOB) => match &mut unit.picture {
                    Picture::Hevc { end_after, .. } => *end_after = true,
                    _ => unit.after_end = true,
                },
                (Tables::Hevc(tables), kind)
                    if kinds::is_slice(kind) && matches!(unit.picture, Picture::None) =>
                {
                    if let Some(slice) = tables.slice(nal, &mut self.references)? {
                        unit.picture = Picture::Hevc {
                            slice,
                            end_after: false,
                        };
                    }
                }
                _ => {}
            }
        }
        if let Picture::Avc { recovery: read, .. } = &mut unit.picture {
            *read = recovery;
        }
        Ok(unit)
    }

    /// Stores the set of an SPS or PPS NAL unit, noting the set it replaces;
    /// returns the bit of an HEVC SPS id whose content it changed. Other NAL
    /// units change nothing.
    fn parameter_set(&mut self, kind: u8, payload: &[u8]) -> Result<u16> {
        let is_set = matches!(
            (self.codec, kind),
            (
                Codec::H264,
                triergon_framing::avc::SPS | triergon_framing::avc::PPS
            ) | (Codec::Hevc, kinds::SPS | kinds::PPS)
        );
        if !is_set {
            return Ok(0);
        }
        // Room to note the set it replaces, before it does.
        self.replaced.try_reserve(1)?;
        let mut changed = 0;
        let replaced = match (&mut self.tables, kind) {
            (Tables::Avc(tables), triergon_framing::avc::SPS) => tables
                .put_sps(payload)?
                .map(|(id, old)| Replaced::AvcSps(id, old)),
            (Tables::Avc(tables), _) => tables
                .put_pps(payload)?
                .map(|(id, old)| Replaced::AvcPps(id, old)),
            (Tables::Hevc(tables), kinds::SPS) => tables.put_sps(payload)?.map(|replaced| {
                if replaced.sps.is_some() {
                    changed = 1 << replaced.id;
                }
                Replaced::HevcSps(replaced)
            }),
            (Tables::Hevc(tables), _) => tables
                .put_pps(payload)?
                .map(|(id, old)| Replaced::HevcPps(id, old)),
        };
        if let Some(replaced) = replaced {
            keep(&mut self.replaced, replaced);
        }
        Ok(changed)
    }

    /// Takes the parameter sets of a decoder configuration record before the
    /// first access unit.
    pub(super) fn configure(&mut self, record: &[u8]) -> Result<()> {
        self.step(|decode| {
            for nal in configuration_units(decode.codec, record) {
                if let Some(kind) = decode.codec.nal_type(nal) {
                    let changed = decode.parameter_set(kind, decode.codec.payload(nal))?;
                    decode.progress.hevc.changed_sps |= changed;
                }
            }
            Ok(())
        })
    }

    /// Decodes an access unit read by [`read`](Self::read), with its
    /// packet's times.
    fn access_unit(
        &mut self,
        unit: Unit,
        bytes: usize,
        pts: Option<i64>,
        dts: Option<i64>,
        duration: Option<i64>,
        out: &mut FrameQueue,
    ) {
        let hevc = &mut self.progress.hevc;
        hevc.after_end = unit.after_end;
        hevc.changed_sps |= unit.changed_sps;
        // A packet's time is its DTS, else its PTS; one whose DTS is absent or
        // equals its PTS gives none in an untimed run.
        let time = dts.or(pts);
        let distinct_dts = dts.is_some_and(|d| Some(d) != pts);
        let held = |decode: &Self, timing: Option<Timing>, ticks: u32| Held {
            pts,
            duration: duration
                .filter(|&d| d > 0)
                .or_else(|| timing.map(|t| decode.frame_period(t, ticks))),
            start: unit.start,
            end: decode.captions.bytes.len(),
            recovered: false,
            key: false,
            above: 0,
            first_key: false,
        };
        let mut untimed = false;
        match unit.picture {
            Picture::None => {
                if self.codec == Codec::Hevc {
                    self.progress.pending = Some(unit.start);
                } else {
                    self.captions.bytes.truncate(unit.start);
                }
            }
            Picture::Avc {
                slice,
                second,
                recovery,
            } => {
                let Tables::Avc(tables) = &self.tables else {
                    return;
                };
                let Some(sps) = tables.sps(slice.sps_id) else {
                    return;
                };
                let ticks = if slice.header.bottom_field.is_some() {
                    1
                } else {
                    2
                };
                let mut held = held(self, sps.timing(), ticks);
                if self.progress.avc.decoded == 0
                    && held.duration.is_none()
                    && matches!(self.framing, Framing::Lengths(_))
                {
                    self.progress.shown = Some(Shown::new(self.time_base));
                }
                if let Some(shown) = &self.progress.shown {
                    held.duration = held.duration.or(shown.early());
                }
                untimed = self.avc_picture(slice, recovery, held, out);
                if let Some(shown) = &mut self.progress.shown {
                    shown.settle(self.time_base);
                }
                // The second field, without caption bytes of its own: the
                // packet's go to the first.
                if let Some(slice) = second {
                    let end = self.captions.bytes.len();
                    let held = Held {
                        start: end,
                        end,
                        ..held
                    };
                    self.avc_picture(slice, None, held, out);
                }
            }
            Picture::Hevc { slice, end_after } => {
                let Tables::Hevc(tables) = &self.tables else {
                    return;
                };
                let Some(sps) = tables.sps(slice.sps_id) else {
                    return;
                };
                let fields = sps.fields;
                let held = held(self, fields.timing, 1);
                self.hevc_picture(slice, fields, unit.after_end, held, out);
                if end_after {
                    self.progress.hevc.after_end = true;
                }
            }
        }
        self.progress
            .timing
            .packet(bytes, time.filter(|_| distinct_dts || !untimed));
    }

    /// Gathers an SEI NAL unit's caption bytes; returns a recovery point's
    /// `recovery_frame_cnt` (H.264).
    fn read_sei(&mut self, payload: &[u8]) -> Result<Option<u32>> {
        if self.sei.len() < payload.len() {
            self.sei.try_resize(payload.len(), 0)?;
        }
        let mut recovery = None;
        let mut result = Ok(());
        let captions = &mut self.captions;
        let h264 = self.codec == Codec::H264;
        let wanted = |kind| kind == SEI_USER_DATA_REGISTERED || h264 && kind == SEI_RECOVERY_POINT;
        sei_messages(
            payload,
            self.sei.as_mut_slice(),
            wanted,
            |message| match message.payload_type {
                SEI_USER_DATA_REGISTERED if result.is_ok() => {
                    result = captions.sei(message.payload);
                }
                SEI_RECOVERY_POINT if h264 && recovery.is_none() => {
                    recovery = Rbsp::lenient(message.payload).ue().ok();
                }
                _ => {}
            },
        );
        result.map(|()| recovery)
    }

    /// The VUI frame period of `ticks` clock ticks in the time base: 0 when it
    /// is 1/1000 s or less.
    fn frame_period(&self, timing: Timing, ticks: u32) -> i64 {
        let units = u128::from(timing.num_units_in_tick) * u128::from(ticks);
        let scale = u128::from(timing.time_scale);
        if units * 1000 <= scale {
            return 0;
        }
        let (num, den) = self.time_base;
        let ticks = units * u128::from(den) / (scale * u128::from(num.max(1)));
        i64::try_from(ticks).unwrap_or(i64::MAX)
    }

    /// Outputs the pictures that left.
    fn emit(&mut self, out: &mut FrameQueue) {
        for at in 0..self.leaving.len() {
            let (_, held) = self.leaving[at];
            self.output(held, out);
        }
        self.leaving.clear();
    }

    /// Outputs a frame, or keeps it (and those after it) in the output until
    /// the duration estimate it may need is settled and the start choice made.
    fn output(&mut self, held: Held, out: &mut FrameQueue) {
        let bytes = &self.captions.bytes.as_bytes()[held.start..held.end];
        let progress = &mut self.progress;
        let needs_estimate = held.duration.is_none() && !progress.settled();
        if progress.holding || needs_estimate || progress.avc.choice.is_some() {
            progress.holding = true;
            progress.needs_estimate |= held.duration.is_none();
            if let Some(choice) = &mut progress.avc.choice
                && !bytes.is_empty()
            {
                let at = Some(out.kept());
                if held.first_key {
                    choice.key = at;
                } else if held.above == 1 {
                    choice.above = at;
                }
            }
            out.push_kept(held.pts, held.duration, bytes);
            return;
        }
        let duration = held
            .duration
            .unwrap_or_else(|| estimate(progress, self.time_base));
        out.push_reserved(held.pts, Some(duration), bytes);
    }

    /// Releases the frames kept once the duration estimate they need is
    /// settled and the start choice made.
    fn settle(&mut self, out: &mut FrameQueue) {
        let progress = &self.progress;
        if !progress.holding
            || progress.avc.choice.is_some()
            || progress.needs_estimate && !progress.settled()
        {
            return;
        }
        let estimate = self
            .progress
            .needs_estimate
            .then(|| estimate(&mut self.progress, self.time_base));
        out.release(estimate, self.progress.skip);
        self.progress.holding = false;
        self.progress.needs_estimate = false;
        self.progress.skip = None;
    }

    /// Drops the bytes of pictures that have left once they outweigh the
    /// rest: the bytes still needed move to the front, in order.
    fn compact(&mut self) {
        let total = self.captions.bytes.len();
        if total < 4096 {
            return;
        }
        let pending = self.progress.pending.map_or(0, |start| total - start);
        let live: usize = pending
            + self
                .pictures
                .payloads()
                .map(|held| held.end - held.start)
                .sum::<usize>();
        if total < 2 * live {
            return;
        }
        let mut cursor = 0;
        let mut from = 0;
        loop {
            // The next range still needed, by start.
            let next = self
                .pictures
                .payloads()
                .filter(|held| held.start >= from && held.end > held.start)
                .map(|held| held.start)
                .min();
            let Some(start) = next else { break };
            let mut moved = None;
            for mut entry in self.pictures.entries_mut() {
                let held = entry.picture_mut();
                if held.start == start && held.end > held.start {
                    let len = held.end - held.start;
                    moved.get_or_insert((start, len));
                    held.start = cursor;
                    held.end = cursor + len;
                }
            }
            if let Some((start, len)) = moved {
                self.captions.bytes.copy_within(start..start + len, cursor);
                cursor += len;
                from = start + len;
            }
        }
        if let Some(start) = self.progress.pending {
            self.captions.bytes.copy_within(start..total, cursor);
            self.progress.pending = Some(cursor);
            cursor += total - start;
        }
        for mut entry in self.pictures.entries_mut() {
            let held = entry.picture_mut();
            if held.end == held.start {
                held.start = 0;
                held.end = 0;
            }
        }
        self.captions.bytes.truncate(cursor);
    }
}

/// A sink that adds the pictures output to `list`, whose capacity the step
/// reserved.
fn collect(list: &mut BudgetVec<(i32, Held)>) -> impl FnMut(Leave<'_, Held>) + '_ {
    |leave| {
        if let Leave::Output { poc, picture } = leave {
            debug_assert!(list.len() < list.capacity());
            // Within the reserved capacity, the push neither allocates nor
            // fails.
            let _ = list.try_push((poc, *picture));
        }
    }
}

/// Adds `item` to `list`, whose capacity the caller reserved.
fn keep<T>(list: &mut BudgetVec<T>, item: T) {
    debug_assert!(list.len() < list.capacity());
    // Within the reserved capacity, the push neither allocates nor fails.
    let _ = list.try_push(item);
}

/// The settled duration estimate, worked out once.
fn estimate(progress: &mut Progress, time_base: (u32, u32)) -> i64 {
    let shown = progress.shown.and_then(|shown| shown.duration());
    *progress
        .estimate
        .get_or_insert_with(|| shown.unwrap_or_else(|| progress.timing.duration(time_base)))
}
