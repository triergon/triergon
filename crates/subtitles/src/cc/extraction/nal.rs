//! The picture output of H.264 and HEVC decoding as far as captions go: which
//! pictures' caption bytes become frames, in which order and with which times.
//!
//! Each access unit (one packet) is one picture. Its slice header and
//! parameter sets drive [`DecodedPictures`], the picture output model of ITU-T
//! H.264 Annex C.4 and H.265 Annex C.5.2, with the steps by which FFmpeg's
//! decoders depart from them; the picture's caption bytes are its payload.
//! Captions come from `user_data_registered_itu_t_t35` messages of H.264 SEI
//! and HEVC prefix SEI NAL units; HEVC suffix SEI carries none for FFmpeg.
//!
//! **H.264.** Order counts follow 8.2.1 with two exceptions. After a
//! reference picture with `memory_management_control_operation` 5, the next
//! pictures take its coded `pic_order_cnt_lsb` as prevPicOrderCntLsb
//! (8.2.1.1 takes 0); that picture orders as 0 among those after it, and the
//! pictures before it leave first, as at an IDR picture. After a gap in
//! `frame_num` (with order count type 1 or 2), the previous `frame_num` is the
//! one before the picture's and FrameNumOffset stays that of the last picture,
//! so a picture whose `frame_num` lies below the last reference picture's,
//! other than 0, counts lower than the pictures before it and is dropped as
//! late (below). `no_output_of_prior_pics_flag` is ignored: the pictures
//! waiting at an IDR picture are all output. Pictures do not wait for a full
//! buffer (C.4.5): one leaves, lowest order count first, as soon as more
//! pictures wait than the reorder depth. That is the VUI's
//! `max_num_reorder_frames`, or without a bitstream restriction the smaller of
//! Table A-1's MaxDpbMbs in frames of the picture size (at most 16) and
//! `max_num_ref_frames - 1`; with order count type 0 it is at least 1. A
//! picture leaving after a picture with a higher order count has been output
//! (since the last IDR picture or operation 5) is dropped with its captions;
//! without a bitstream restriction the depth then grows by one.
//!
//! A stream that does not start at an IDR picture outputs nothing until it
//! recovers: at an IDR picture; at the reference picture whose `frame_num` a
//! recovery point SEI names (`recovery_frame_cnt` after the SEI's picture),
//! which also recovers the pictures waiting; or when its first intra picture
//! leaves, that picture being output. Pictures decoded before recovery, and
//! pictures with a lower order count than the first intra picture or the
//! recovery point, are dropped with their captions.
//!
//! **HEVC.** Order counts follow 8.3.1 and the reference picture sets 8.3.2.
//! A picture is decoded from the first IRAP picture on; RASL pictures of an
//! IRAP picture with NoRaslOutputFlag equal to 1 are not. NoRaslOutputFlag is
//! 1 for an IDR or BLA picture, the first picture, the first after an end of
//! sequence or end of bitstream NAL unit, and an IRAP picture that activates
//! another SPS or one whose content changed (the pictures waiting are then
//! output first). A picture that is not decoded, or an access unit without a
//! picture, passes its caption bytes on to the next picture decoded. Output
//! follows C.5.2.2 and C.5.2.3 with `sps_max_num_reorder_pics`,
//! `sps_max_latency_increase_plus1` and `no_output_of_prior_pics_flag`: an
//! IRAP picture with NoRaslOutputFlag equal to 1 discards the pictures still
//! waiting, with their captions, when it is a CRA picture or has the flag
//! set, and outputs them otherwise. The buffer makes room before the current
//! picture's reference picture set is applied, while it holds
//! `sps_max_dec_pic_buffering_minus1 + 2` pictures (C.5.2.2 checks `+ 1`
//! after it). A picture that is not an IRAP picture but follows an end of
//! sequence or end of bitstream NAL unit outputs every picture waiting before
//! it is decoded.
//!
//! **Times.** A frame keeps its packet's PTS. It lasts its packet's duration
//! when the container gives one above 0; otherwise the frame period of its
//! SPS's VUI timing (H.264: two ticks, HEVC: one tick), truncated to the time
//! base, or 0 when that period is 1/1000 s or less; without VUI timing, the
//! greatest common divisor of the steps between the decode times of packets 1
//! to 20 (the first step does not count), as FFmpeg's demuxer estimates the
//! frame rate. Frames that need that estimate wait, with those after them,
//! until the twenty-first packet or the end.
//!
//! A picture whose parameter sets are missing or invalid is not decoded.
//!
//! A push or the finish either succeeds or leaves the decoder and its output
//! as they were: the picture buffer is copied before a step and put back when
//! it fails, buffers are appended to and cut back, and the parameter sets a
//! step replaces are kept until it succeeds.

use super::captions::Gathered;
use super::queue::FrameQueue;
use crate::cc::video::avc::{self, PocType};
use crate::cc::video::hevc::{self, Reference};
use crate::cc::video::nal::{
    Codec, Framing, SEI_RECOVERY_POINT, SEI_USER_DATA_REGISTERED, Timing, nal_units, sei_messages,
};
use triergon_bitstream::{BitSource, Rbsp};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::hevc::SUFFIX_SEI;
use triergon_framing::picture_output::order::{
    AfterGap, AfterMmco5, AvcOrder, AvcPocCoding, AvcPocFields, Bottom, HevcOrder, HevcPocFields,
};
use triergon_framing::picture_output::{
    AvcStore, DecodedPictures, HevcLimits, HevcPicture, LateOutput, Leave,
};

/// A picture waiting in the buffer: its times and its caption bytes in the
/// gathered bytes.
#[derive(Clone, Copy, Debug)]
struct Held {
    pts: Option<i64>,
    /// The packet's duration, or the VUI's frame period; `None` for the
    /// least decode time step at output.
    duration: Option<i64>,
    start: usize,
    end: usize,
    /// H.264: the stream had recovered when it was decoded.
    recovered: bool,
    /// H.264: the first intra picture of a stream that had not recovered.
    key: bool,
}

/// H.264 output state between pictures.
#[derive(Clone, Copy, Debug, Default)]
struct AvcState {
    order: AvcOrder,
    /// The reorder depth, once a picture set it, and whether a bitstream
    /// restriction stated it.
    depth: Option<u32>,
    stated: bool,
    recovered: bool,
    /// The `frame_num` a recovery point SEI names.
    target: Option<u32>,
    /// A first intra picture has been taken as the key picture.
    keyed: bool,
    /// Pictures with a lower order count than this are dropped.
    floor: Option<i32>,
    late: LateOutput,
}

/// HEVC decoding state between pictures.
#[derive(Clone, Copy, Debug, Default)]
struct HevcState {
    order: HevcOrder,
    /// An IRAP picture has been decoded.
    started: bool,
    /// An end of sequence or end of bitstream NAL unit came after the last
    /// picture decoded.
    after_end: bool,
    /// NoRaslOutputFlag of the last IRAP picture.
    no_rasl_output: bool,
    /// The SPS of the last picture decoded, and the ids of the SPSs whose
    /// content changed since.
    active_sps: Option<u8>,
    changed_sps: u16,
}

/// The decoder's state between steps, besides its buffers.
#[derive(Clone, Copy, Debug, Default)]
struct Progress {
    avc: AvcState,
    hevc: HevcState,
    /// Bytes of pictures not decoded, for the next picture decoded (HEVC):
    /// they run from here to the end of the gathered bytes.
    pending: Option<usize>,
    timing: StepTiming,
    /// Frames wait in `unsettled` until the step estimate is settled.
    holding: bool,
}

/// The frame duration FFmpeg's demuxer estimates from the packets' decode
/// times: the greatest common divisor of the steps between packets 1 and 20
/// (the second to the twenty-first packet).
#[derive(Clone, Copy, Debug, Default)]
struct StepTiming {
    packets: u32,
    last: Option<i64>,
    divisor: Option<i64>,
}

/// The packets whose decode times the estimate takes.
const ESTIMATE_PACKETS: u32 = 21;

impl StepTiming {
    fn packet(&mut self, time: Option<i64>) {
        if let (Some(time), Some(last)) = (time, self.last)
            && (2..ESTIMATE_PACKETS).contains(&self.packets)
            && let Some(step) = time.checked_sub(last).filter(|&step| step > 0)
        {
            self.divisor = Some(self.divisor.map_or(step, |divisor| gcd(divisor, step)));
        }
        self.last = time.or(self.last);
        self.packets = self.packets.saturating_add(1);
    }

    fn settled(&self) -> bool {
        self.packets >= ESTIMATE_PACKETS
    }
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The parameter sets of either codec.
#[derive(Debug)]
enum Tables {
    Avc(avc::Tables),
    Hevc(hevc::Tables),
}

/// A parameter set a step replaced, to put back when it fails.
#[derive(Clone, Copy, Debug)]
enum Replaced {
    AvcSps(u8, Option<avc::Sps>),
    AvcPps(u8, Option<avc::Pps>),
    HevcSps(u8, Option<hevc::Sps>),
    HevcPps(u8, Option<hevc::Pps>),
}

/// Access units in decode order.
#[derive(Debug)]
pub(super) struct NalDecode {
    codec: Codec,
    framing: Framing,
    time_base: (u32, u32),
    tables: Tables,
    pictures: DecodedPictures<Held>,
    /// The buffer before the step, to go back to.
    saved: DecodedPictures<Held>,
    captions: Gathered,
    progress: Progress,
    replaced: BudgetVec<Replaced>,
    references: BudgetVec<Reference>,
    leaving: BudgetVec<(i32, Held)>,
    sei: BudgetVec<u8>,
    /// Frames kept until the step estimate is settled.
    unsettled: FrameQueue,
}

/// What an access unit holds.
#[derive(Clone, Copy, Debug)]
enum Picture {
    None,
    Avc(avc::Slice),
    Hevc(hevc::Slice, bool),
}

impl NalDecode {
    pub(super) fn new(
        pool: &Budget,
        codec: Codec,
        framing: Framing,
        time_base: (u32, u32),
    ) -> Self {
        let mut progress = Progress::default();
        progress.avc.order = AvcOrder::new(AfterMmco5::CodedLsb, AfterGap::OffsetKept);
        Self {
            codec,
            framing,
            time_base,
            tables: match codec {
                Codec::H264 => Tables::Avc(avc::Tables::new(pool)),
                Codec::Hevc => Tables::Hevc(hevc::Tables::new(pool)),
            },
            pictures: DecodedPictures::new(pool),
            saved: DecodedPictures::new(pool),
            captions: Gathered::new(pool),
            progress,
            replaced: BudgetVec::new(pool),
            references: BudgetVec::new(pool),
            leaving: BudgetVec::new(pool),
            sei: BudgetVec::new(pool),
            unsettled: FrameQueue::new(pool),
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
        self.step(out, |decode, out| {
            decode.access_unit(data, pts, dts, duration, out)?;
            decode.settle(false, out)
        })
    }

    pub(super) fn finish(&mut self, out: &mut FrameQueue) -> Result<()> {
        self.step(out, |decode, out| {
            decode.leaving.try_reserve(decode.pictures.len())?;
            let leaving = &mut decode.leaving;
            decode.pictures.flush(|leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            });
            match decode.codec {
                Codec::H264 => decode.emit_avc(out)?,
                Codec::Hevc => decode.emit(out)?,
            }
            decode.settle(true, out)?;
            decode.progress.pending = None;
            Ok(())
        })
    }

    /// Runs `step`; when it fails, puts the decoder and `out` back as they
    /// were.
    fn step(
        &mut self,
        out: &mut FrameQueue,
        step: impl FnOnce(&mut Self, &mut FrameQueue) -> Result<()>,
    ) -> Result<()> {
        self.saved.copy_from(&self.pictures)?;
        let progress = self.progress;
        let captions = self.captions.mark();
        let frames = out.len();
        let unsettled = self.unsettled.len();
        let pools = self.pool_marks();
        self.replaced.clear();
        let result = step(self, out);
        if result.is_err() {
            std::mem::swap(&mut self.pictures, &mut self.saved);
            self.progress = progress;
            self.captions.rollback(captions);
            out.truncate(frames);
            if self.unsettled.len() >= unsettled {
                self.unsettled.truncate(unsettled);
            }
            self.restore(pools);
        } else {
            self.compact();
        }
        self.leaving.clear();
        result
    }

    fn pool_marks(&self) -> (usize, Option<hevc::Mark>) {
        match &self.tables {
            Tables::Avc(tables) => (tables.offsets_len(), None),
            Tables::Hevc(tables) => (0, Some(tables.mark())),
        }
    }

    /// Puts back the parameter sets of a failed step, last replaced first.
    fn restore(&mut self, (offsets, mark): (usize, Option<hevc::Mark>)) {
        while let Some(replaced) = self.replaced.pop() {
            match (&mut self.tables, replaced) {
                (Tables::Avc(t), Replaced::AvcSps(id, old)) => t.restore_sps(id, old),
                (Tables::Avc(t), Replaced::AvcPps(id, old)) => t.restore_pps(id, old),
                (Tables::Hevc(t), Replaced::HevcSps(id, old)) => t.restore_sps(id, old),
                (Tables::Hevc(t), Replaced::HevcPps(id, old)) => t.restore_pps(id, old),
                _ => {}
            }
        }
        match (&mut self.tables, mark) {
            (Tables::Avc(t), _) => {
                t.truncate_offsets(offsets);
                t.forget_repeats();
            }
            (Tables::Hevc(t), mark) => {
                if let Some(mark) = mark {
                    t.rollback(mark);
                }
                t.forget_repeats();
            }
        }
    }

    fn access_unit(
        &mut self,
        data: &[u8],
        pts: Option<i64>,
        dts: Option<i64>,
        duration: Option<i64>,
        out: &mut FrameQueue,
    ) -> Result<()> {
        let start = self.progress.pending.unwrap_or(self.captions.bytes.len());
        let mut picture = Picture::None;
        let mut recovery = None;
        let mut end_after = false;
        for nal in nal_units(data, self.framing) {
            let Some(kind) = self.codec.nal_type(nal) else {
                continue;
            };
            let payload = self.codec.payload(nal);
            if self.codec.is_sei(kind) {
                // FFmpeg takes no captions from HEVC suffix SEI.
                if !(self.codec == Codec::Hevc && kind == SUFFIX_SEI) {
                    recovery = recovery.or(self.read_sei(payload)?);
                }
                continue;
            }
            if matches!(
                (self.codec, kind),
                (Codec::H264, 7 | 8) | (Codec::Hevc, 33 | 34)
            ) {
                // Room to note the set it replaces, before it does.
                self.replaced.try_reserve(1)?;
            }
            match (&mut self.tables, kind) {
                (Tables::Avc(tables), 7) => {
                    if let Some((id, old)) = tables.put_sps(payload)? {
                        keep(&mut self.replaced, Replaced::AvcSps(id, old));
                    }
                }
                (Tables::Avc(tables), 8) => {
                    if let Some((id, old)) = tables.put_pps(payload)? {
                        keep(&mut self.replaced, Replaced::AvcPps(id, old));
                    }
                }
                (Tables::Avc(tables), 1 | 5) if matches!(picture, Picture::None) => {
                    if let Some(slice) = tables.slice(nal) {
                        picture = Picture::Avc(slice);
                    }
                }
                (Tables::Hevc(tables), 33) => {
                    if let Some((id, old)) = tables.put_sps(payload)? {
                        keep(&mut self.replaced, Replaced::HevcSps(id, old));
                        if old.is_some() {
                            self.progress.hevc.changed_sps |= 1 << id;
                        }
                    }
                }
                (Tables::Hevc(tables), 34) => {
                    if let Some((id, old)) = tables.put_pps(payload)? {
                        keep(&mut self.replaced, Replaced::HevcPps(id, old));
                    }
                }
                (Tables::Hevc(_), 36 | 37) => {
                    if matches!(picture, Picture::None) {
                        self.progress.hevc.after_end = true;
                    } else {
                        end_after = true;
                    }
                }
                (Tables::Hevc(tables), kind)
                    if self.codec.is_slice(kind) && matches!(picture, Picture::None) =>
                {
                    if let Some(slice) = tables.slice(nal, &mut self.references)? {
                        picture = Picture::Hevc(slice, self.progress.hevc.after_end);
                    }
                }
                _ => {}
            }
        }
        self.progress.timing.packet(dts.or(pts));
        let held = |decode: &Self, timing: Option<Timing>, ticks: u32| Held {
            pts,
            duration: duration
                .filter(|&d| d > 0)
                .or_else(|| timing.map(|t| decode.frame_period(t, ticks))),
            start,
            end: decode.captions.bytes.len(),
            recovered: false,
            key: false,
        };
        match picture {
            Picture::None => {
                if self.codec == Codec::Hevc {
                    self.progress.pending = Some(start);
                } else {
                    self.captions.bytes.truncate(start);
                }
                Ok(())
            }
            Picture::Avc(slice) => {
                let Tables::Avc(tables) = &self.tables else {
                    return Ok(());
                };
                let Some(&sps) = tables.sps(slice.sps_id) else {
                    return Ok(());
                };
                let held = held(self, sps.timing, 2);
                self.avc_picture(slice, sps, recovery, held, out)
            }
            Picture::Hevc(slice, after_end) => {
                let Tables::Hevc(tables) = &self.tables else {
                    return Ok(());
                };
                let Some(&sps) = tables.sps(slice.sps_id) else {
                    return Ok(());
                };
                let held = held(self, sps.timing, 1);
                self.hevc_picture(slice, sps, after_end, held, out)?;
                if end_after {
                    self.progress.hevc.after_end = true;
                }
                Ok(())
            }
        }
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

    fn avc_picture(
        &mut self,
        slice: avc::Slice,
        sps: avc::Sps,
        recovery: Option<u32>,
        mut held: Held,
        out: &mut FrameQueue,
    ) -> Result<()> {
        let Tables::Avc(tables) = &self.tables else {
            return Ok(());
        };
        let coding = match sps.poc {
            PocType::Lsb { log2_max_lsb } => AvcPocCoding::Lsb {
                lsb: slice.poc_lsb,
                log2_max_lsb,
                delta_bottom: slice.delta_bottom,
            },
            PocType::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                cycle_start,
                cycle_len,
                ..
            } => AvcPocCoding::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                offsets_for_ref_frame: tables.cycle(cycle_start, cycle_len),
                delta: slice.delta,
            },
            PocType::FrameNum => AvcPocCoding::FrameNum,
        };
        let state = &mut self.progress.avc;
        let poc = state.order.next(&AvcPocFields {
            idr: slice.idr,
            reference: slice.reference,
            frame_num: slice.frame_num,
            log2_max_frame_num: sps.log2_max_frame_num,
            field: slice.field.map(Bottom),
            mmco5: slice.mmco5,
            coding,
        });
        // The reorder depth.
        let mut depth = match sps.max_num_reorder_frames {
            Some(stated) => {
                state.stated = true;
                stated
            }
            None if state.depth.is_some() && !state.stated => state.depth.unwrap_or(0),
            None => {
                state.stated = false;
                level_dpb_frames(sps.level_idc, sps.frame_mbs)
                    .min(sps.max_num_ref_frames.saturating_sub(1))
            }
        };
        if matches!(sps.poc, PocType::Lsb { .. }) {
            depth = depth.max(1);
        }
        state.depth = Some(depth);
        self.leaving.try_reserve(self.pictures.len() + 1)?;
        if slice.idr || slice.mmco5 {
            let leaving = &mut self.leaving;
            while self.pictures.bump(|leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            }) {}
            self.emit_avc(out)?;
            let state = &mut self.progress.avc;
            state.late.restart();
            state.floor = None;
        }
        let state = &mut self.progress.avc;
        if slice.idr {
            state.recovered = true;
        }
        if let Some(count) = recovery
            && !state.recovered
            && state.target.is_none()
        {
            state.target =
                Some(slice.frame_num.wrapping_add(count) % (1 << sps.log2_max_frame_num));
        }
        if !state.recovered && slice.intra && !state.keyed {
            state.keyed = true;
            held.key = true;
            state.floor = Some(poc.poc);
        }
        if !state.recovered && slice.reference && state.target == Some(slice.frame_num) {
            // The recovery point recovers the pictures still waiting too.
            state.recovered = true;
            state.target = None;
            state.floor = Some(state.floor.map_or(poc.poc, |floor| floor.min(poc.poc)));
            for waiting in self.pictures.pictures_mut() {
                waiting.recovered = true;
            }
        }
        held.recovered = state.recovered;
        // Stored as a non-reference picture in a buffer that is never full:
        // pictures leave by the reorder depth instead.
        let store = AvcStore {
            poc: poc.stored_poc,
            reference: false,
            output: true,
            second_field: false,
        };
        self.pictures.avc_store(store, held, usize::MAX, |_| {})?;
        self.leaving.try_reserve(self.pictures.len())?;
        loop {
            let depth = self.progress.avc.depth.unwrap_or(0) as usize;
            if self.pictures.waiting() <= depth {
                break;
            }
            let leaving = &mut self.leaving;
            self.pictures.bump(|leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            });
            self.emit_avc(out)?;
        }
        Ok(())
    }

    /// Outputs the H.264 pictures that left, dropping those not recovered,
    /// below the floor or late.
    fn emit_avc(&mut self, out: &mut FrameQueue) -> Result<()> {
        let mut at = 0;
        while at < self.leaving.len() {
            let (poc, held) = self.leaving[at];
            at += 1;
            let state = &mut self.progress.avc;
            if !held.recovered && !held.key {
                continue;
            }
            if state.floor.is_some_and(|floor| poc < floor) {
                continue;
            }
            if state.late.is_late(poc) {
                if !state.stated {
                    state.depth = state.depth.map(|depth| depth + 1);
                }
                continue;
            }
            state.late.admit(poc);
            if held.key {
                state.recovered = true;
            }
            self.output(held, out)?;
        }
        self.leaving.clear();
        Ok(())
    }

    fn hevc_picture(
        &mut self,
        slice: hevc::Slice,
        sps: hevc::Sps,
        after_end: bool,
        held: Held,
        out: &mut FrameQueue,
    ) -> Result<()> {
        let state = &mut self.progress.hevc;
        let kind = slice.nal_type;
        let irap = (16..=23).contains(&kind);
        let rasl = matches!(kind, 8 | 9);
        // An IRAP picture activating another SPS, or one whose content
        // changed, starts a new sequence after the pictures waiting are output.
        let new_sps = irap
            && state.started
            && !after_end
            && (state.active_sps != Some(slice.sps_id)
                || state.changed_sps & 1 << slice.sps_id != 0);
        if irap {
            state.no_rasl_output = kind <= 20 || !state.started || after_end || new_sps;
        }
        if !state.started && !irap || rasl && state.no_rasl_output {
            // Not decoded: its bytes go to the next picture decoded.
            self.progress.pending = Some(held.start);
            return Ok(());
        }
        self.progress.pending = None;
        let first = !state.started;
        state.started = true;
        state.after_end = false;
        state.active_sps = Some(slice.sps_id);
        state.changed_sps &= !(1 << slice.sps_id);
        let no_rasl_output = irap && state.no_rasl_output;
        let sub_layer_non_reference = kind <= 14 && kind.is_multiple_of(2);
        let poc = state.order.next(HevcPocFields {
            irap_no_rasl_output: no_rasl_output,
            lsb: slice.poc_lsb,
            log2_max_lsb: sps.log2_max_lsb,
            tid0_anchor: slice.temporal_id == 0
                && !(6..=9).contains(&kind)
                && !sub_layer_non_reference,
        });
        let limits = HevcLimits {
            max_dec_pic_buffering: None,
            ..sps.limits
        };
        let holding = sps.limits.max_dec_pic_buffering.unwrap_or(1) as usize + 1;
        self.leaving.try_reserve(2 * (self.pictures.len() + 1))?;
        let leaving = &mut self.leaving;
        let mut collect = |leave: Leave<'_, Held>| {
            if let Leave::Output { poc, picture } = leave {
                collect(leaving, poc, picture);
            }
        };
        // FFmpeg makes room before the reference picture set applies.
        self.pictures.bump_while_holding(holding, &mut collect);
        if no_rasl_output {
            self.pictures.mark_unused();
        } else {
            let max_lsb = 1i64 << sps.log2_max_lsb;
            let references = self.references.as_slice();
            self.pictures.retain_references(|stored| {
                references.iter().any(|&reference| match reference {
                    Reference::Delta(delta) => {
                        i64::from(poc) + i64::from(delta) == i64::from(stored)
                    }
                    Reference::LongDelta(delta) => i64::from(poc) + delta == i64::from(stored),
                    Reference::Lsb(lsb) => i64::from(stored) & (max_lsb - 1) == i64::from(lsb),
                })
            });
        }
        if after_end && !irap || new_sps {
            while self.pictures.bump(&mut collect) {}
        }
        self.pictures.hevc_remove(
            HevcPicture {
                irap_no_rasl_output: no_rasl_output,
                cra: kind == 21,
                first,
                no_output_of_prior_pics: slice.no_output_of_prior_pics,
            },
            limits,
            &mut collect,
        );
        self.pictures
            .hevc_store(poc, slice.pic_output, held, limits, &mut collect)?;
        self.emit(out)
    }

    /// Outputs the pictures that left.
    fn emit(&mut self, out: &mut FrameQueue) -> Result<()> {
        for at in 0..self.leaving.len() {
            let (_, held) = self.leaving[at];
            self.output(held, out)?;
        }
        self.leaving.clear();
        Ok(())
    }

    /// Outputs a frame, or keeps it (and those after it) until the step
    /// estimate its duration may need is settled.
    fn output(&mut self, held: Held, out: &mut FrameQueue) -> Result<()> {
        let bytes = &self.captions.bytes.as_bytes()[held.start..held.end];
        let progress = &mut self.progress;
        if progress.holding || held.duration.is_none() && !progress.timing.settled() {
            progress.holding = true;
            return self.unsettled.push(held.pts, held.duration, bytes);
        }
        out.push(held.pts, held.duration.or(progress.timing.divisor), bytes)
    }

    /// Releases the frames kept for the step estimate once it is settled, or
    /// at the end of the stream.
    fn settle(&mut self, finishing: bool, out: &mut FrameQueue) -> Result<()> {
        if !self.progress.holding || !(finishing || self.progress.timing.settled()) {
            return Ok(());
        }
        let estimate = self.progress.timing.divisor;
        for entry in self.unsettled.entries() {
            out.push(
                entry.pts,
                entry.duration.or(estimate),
                self.unsettled.bytes(entry),
            )?;
        }
        self.unsettled.clear();
        self.progress.holding = false;
        Ok(())
    }

    /// Drops the bytes of pictures that have left once they outweigh the
    /// rest: the bytes still needed move to the front, in order.
    fn compact(&mut self) {
        let total = self.captions.bytes.len();
        let pending = self.progress.pending.map_or(0, |start| total - start);
        let live: usize = pending
            + self
                .pictures
                .pictures_mut()
                .map(|held| held.end - held.start)
                .sum::<usize>();
        if total < 4096 || total < 2 * live {
            return;
        }
        let mut cursor = 0;
        let mut from = 0;
        loop {
            // The next range still needed, by start.
            let next = self
                .pictures
                .pictures_mut()
                .filter(|held| held.start >= from && held.end > held.start)
                .map(|held| held.start)
                .min();
            let Some(start) = next else { break };
            let mut moved = None;
            for held in self.pictures.pictures_mut() {
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
        for held in self.pictures.pictures_mut() {
            if held.end == held.start {
                held.start = 0;
                held.end = 0;
            }
        }
        self.captions.bytes.truncate(cursor);
    }
}

/// Adds a left picture to `list`, whose capacity the caller reserved.
fn collect(list: &mut BudgetVec<(i32, Held)>, poc: i32, picture: &Held) {
    debug_assert!(list.len() < list.capacity());
    // Within the reserved capacity, the push neither allocates nor fails.
    let _ = list.try_push((poc, *picture));
}

/// Adds `item` to `list`, whose capacity the caller reserved.
fn keep<T>(list: &mut BudgetVec<T>, item: T) {
    debug_assert!(list.len() < list.capacity());
    // Within the reserved capacity, the push neither allocates nor fails.
    let _ = list.try_push(item);
}

/// The frames of Table A-1's MaxDpbMbs for `level_idc` at `frame_mbs`
/// macroblocks a frame, at most 16 (A.3.1).
fn level_dpb_frames(level_idc: u8, frame_mbs: u32) -> u32 {
    let max_dpb_mbs: u32 = match level_idc {
        9 | 10 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        _ => 696320,
    };
    (max_dpb_mbs / frame_mbs.max(1)).min(16)
}
