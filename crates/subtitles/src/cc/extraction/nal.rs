//! The picture output of H.264 and HEVC decoding as far as captions go: which
//! pictures' caption bytes become frames, in which order and with which times.
//!
//! Each access unit (one packet) is one picture, or one field of a field pair.
//! Its slice header and parameter sets drive [`DecodedPictures`], the picture
//! output model of ITU-T H.264 Annex C.4 and H.265 Annex C.5.2, with the
//! steps that reproduce the reference's output where it was observed to
//! differ; the picture's caption bytes are its payload. Captions come from
//! `user_data_registered_itu_t_t35` messages of H.264 SEI and HEVC prefix SEI
//! NAL units; the reference takes none from HEVC suffix SEI. Each observed
//! rule names the `subtitle-cc` corpus case that shows it.
//!
//! **H.264.** Order counts follow 8.2.1 with two observed exceptions. After a
//! reference picture with `memory_management_control_operation` 5, the next
//! pictures take its coded `pic_order_cnt_lsb` as prevPicOrderCntLsb where
//! 8.2.1.1 takes 0 (`h264-mmco5-ts`); that picture orders as 0 among those
//! after it, and the pictures before it leave first, as at an IDR picture.
//! After a gap in `frame_num` (order count types 1 and 2), the previous
//! `frame_num` is the one before the picture's and FrameNumOffset stays that
//! of the last picture: no frame is inferred (`h264-gap-ts`). The
//! `no_output_of_prior_pics_flag` of an IDR picture is not honoured: the
//! pictures waiting are output (`h264-noout-ts`). Pictures do not wait for a
//! full buffer (C.4.5): one leaves, lowest order count first, as soon as more
//! pictures wait than the reorder depth (`h264-reorder-ts`). With a bitstream
//! restriction that is its `max_num_reorder_frames` (an SPS stating more than
//! 16 is not taken); among the first 20 pictures, a non-reference picture
//! below an earlier one raises a stated 0 to 1 (probe p3-h264-closed-r0), and
//! a reference picture first lets the pictures beyond the depth leave (probe
//! p53). Without a bitstream restriction the depth starts at 0 and becomes
//! the number of the last 16 pictures decoded above a picture, when that is
//! more (probes p47, p57; `h264-grow-ts`). Nothing then leaves before the
//! seventh picture when MaxDpbFrames (Table A-1) is 2 or more (probes p27,
//! p29, p47), nor before the twenty-first when the depth is 3 or more by the
//! seventh (probes p48, p50); and a picture more than 2 above the last output
//! waits for one more picture, except at the seventh, where it also holds
//! those behind it (probes p31, p34; package case0014). Among the first 20
//! pictures at least one waits (probe p27). A picture below each of the last
//! 15 decoded leaves at once, after those waiting (probes p22, p23; package
//! case0003). A picture leaving after a picture with a higher order count
//! has been output (since the last IDR picture or operation 5) is dropped
//! with its captions (`h264-reorder-ts`). The two fields of a pair, in one
//! packet or two, make one frame carrying both fields' captions in decode
//! order, with the first field's times; its VUI frame period is one tick
//! (`h264-fields-ts`).
//!
//! A stream that does not start at an IDR picture outputs nothing until it
//! recovers (`h264-recovery-*`): at an IDR picture; at the reference picture
//! whose `frame_num` a recovery point SEI names (`recovery_frame_cnt` after
//! the SEI's picture), which also recovers the pictures waiting; or when an
//! intra picture decoded before that leaves, such pictures being output.
//! Pictures decoded before recovery, and pictures with a lower order count
//! than the first of those intra pictures or the recovery point (as stored,
//! so 0 after operation 5: `h264-mmco5-start-ts`), are dropped with their
//! captions.
//!
//! **HEVC.** Order counts follow 8.3.1 and the reference picture sets 8.3.2.
//! A picture is decoded from the first IRAP picture on; RASL pictures of an
//! IRAP picture with NoRaslOutputFlag equal to 1 are not. NoRaslOutputFlag is
//! 1 for an IDR or BLA picture, the first picture, and the first after an end
//! of sequence or end of bitstream NAL unit (8.1.3), and, observed, for an
//! IRAP picture that activates another SPS or one whose content changed, the
//! pictures waiting being output first (`hevc-sps-change-ts`). A picture that
//! is not decoded, or an access unit without a picture, passes its caption
//! bytes on to the next picture decoded. Output follows C.5.2.2 and C.5.2.3
//! with `sps_max_num_reorder_pics`, `sps_max_latency_increase_plus1` and
//! NoOutputOfPriorPicsFlag: an IRAP picture with NoRaslOutputFlag equal to 1
//! discards the pictures still waiting, with their captions, when it is a CRA
//! picture (C.5.2.2) or has `no_output_of_prior_pics_flag` set, and outputs
//! them otherwise (`hevc-eos-ts`). Observed: the buffer makes room before the
//! current picture's reference picture set is applied, while it holds
//! `sps_max_dec_pic_buffering_minus1 + 2` pictures (C.5.2.2 checks `+ 1`
//! after it); `sps_max_dec_pic_buffering_minus1` is raised to
//! `sps_max_num_reorder_pics` where it is lower, and an SPS where it is then
//! above 15 is not taken (`hevc-dpb-ts`, `hevc-dpb-raise-ts`). A picture that is not an IRAP
//! picture but follows an end of sequence or end of bitstream NAL unit
//! outputs every picture waiting before it is decoded (`hevc-eos-rasl-ts`).
//!
//! **Times.** A frame keeps its packet's PTS. It lasts its packet's duration
//! when the container gives one above 0; otherwise the frame period of its
//! SPS's VUI timing (H.264 frames: two ticks; H.264 fields and HEVC: one
//! tick), truncated to the time base, or 0 when that period is 1/1000 s or
//! less (`h264-vui-fast-ts`); without VUI timing, the greatest common divisor
//! of the steps between the decode times of packets 1 to 20, the first step
//! not counting (`h264-estimate-ts`). Frames that need that value wait, with
//! those after them, until the twenty-first packet or the end.
//!
//! A picture whose parameter sets are missing or invalid is not decoded.
//!
//! A push or the finish either succeeds or leaves the decoder and its output
//! as they were: the picture buffer is copied before a step and put back when
//! it fails, buffers are appended to and cut back, and the parameter sets a
//! step replaces are kept until it succeeds.

use super::captions::Gathered;
use super::queue::FrameQueue;
use crate::cc::video::avc;
use crate::cc::video::hevc::{self, Reference};
use crate::cc::video::nal::{
    Codec, Framing, SEI_RECOVERY_POINT, SEI_USER_DATA_REGISTERED, Timing, nal_units, sei_messages,
};
use triergon_bitstream::{BitSource, Rbsp};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::picture_output::levels::max_dpb_frames;
use triergon_framing::picture_output::order::{
    AfterGap, AfterMmco5, AvcOrder, AvcPocCoding, AvcPocFields, Bottom, HevcOrder, HevcPocFields,
};
use triergon_framing::picture_output::{
    AvcStore, DecodedPictures, HevcLimits, HevcPicture, LateOutput, Leave, Marking, NewPicture,
    Parity, PictureId,
};
use triergon_framing::{AvcPocSyntax, hevc as kinds};

/// H.264 without a bitstream restriction: the index of the picture (from 0,
/// in decode order) before which the reference outputs nothing (probes p27,
/// p47).
const HOLD_UNTIL: u32 = 6;

/// H.264: the pictures before this index wait for at least one more
/// (probes p27, p51), and are all held when the depth reached 3 by
/// [`HOLD_UNTIL`] (probes p48, p50).
const START_WINDOW: u32 = 20;

/// H.264: the depth at [`HOLD_UNTIL`] from which the hold lasts the window.
const LONG_HOLD_DEPTH: u32 = 3;

/// H.264: the order counts of the last pictures decoded that a picture's
/// displacement counts (probes p47, p57).
const RECENT: usize = 16;

/// H.264: a picture with a lower order count than the last this many
/// decoded is output at once, after the pictures waiting (probes p22, p23).
const FAR_BACK: usize = 15;

/// H.264: a picture whose order count is at most this much above the last
/// output follows it without a gap (probes p31, p32, p34).
const NEXT_STEP: i32 = 2;

/// The packets whose decode times the duration estimate takes.
const ESTIMATE_PACKETS: u32 = 21;

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
}

/// An H.264 first field waiting for its second.
#[derive(Clone, Copy, Debug)]
struct FirstField {
    id: PictureId,
    frame_num: u16,
    parity: Parity,
}

/// The order counts of the last pictures decoded.
#[derive(Clone, Copy, Debug, Default)]
struct Recent {
    pocs: [i32; RECENT],
    len: usize,
    next: usize,
}

impl Recent {
    fn push(&mut self, poc: i32) {
        self.pocs[self.next] = poc;
        self.next = (self.next + 1) % RECENT;
        self.len = (self.len + 1).min(RECENT);
    }

    /// The order counts, the last `count` decoded.
    fn last(&self, count: usize) -> impl Iterator<Item = i32> + '_ {
        let count = count.min(self.len);
        (0..count).map(move |back| self.pocs[(self.next + RECENT - 1 - back) % RECENT])
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

/// H.264 output state between pictures.
#[derive(Clone, Copy, Debug, Default)]
struct AvcState {
    order: AvcOrder,
    /// The reorder depth, and whether (and which) a bitstream restriction
    /// stated it.
    depth: u32,
    stated: bool,
    stated_depth: u32,
    /// Pictures decoded, and whether the start of the stream is held.
    decoded: u32,
    holds: bool,
    /// The depth when picture [`HOLD_UNTIL`] was decoded.
    early_depth: u32,
    recent: Recent,
    recovered: bool,
    /// The `frame_num` a recovery point SEI names.
    target: Option<u32>,
    /// An intra picture has been decoded before recovery.
    keyed: bool,
    /// Pictures with a lower order count than this are dropped.
    floor: Option<i32>,
    late: LateOutput,
    first_field: Option<FirstField>,
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
    /// Frames wait in `unsettled` until the duration estimate is settled.
    holding: bool,
}

/// The frame duration estimated from the packets' decode times: the greatest
/// common divisor of the steps between packets 1 and 20 (the second to the
/// twenty-first packet).
#[derive(Clone, Copy, Debug, Default)]
struct StepTiming {
    packets: u32,
    last: Option<i64>,
    divisor: Option<i64>,
}

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
#[derive(Debug)]
enum Replaced {
    AvcSps(u8, Option<avc::Sps>),
    AvcPps(u8, Option<triergon_framing::AvcPps>),
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
    /// Frames kept until the duration estimate is settled.
    unsettled: FrameQueue,
}

/// What an access unit holds.
#[derive(Clone, Copy, Debug)]
enum Picture {
    None,
    Avc(avc::Slice),
    Hevc {
        slice: hevc::Slice,
        /// An end of sequence or end of bitstream NAL unit came before it.
        after_end: bool,
    },
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
            decode.progress.avc.first_field = None;
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
            self.restore();
        } else {
            self.compact();
        }
        self.replaced.clear();
        self.leaving.clear();
        result
    }

    /// Puts back the parameter sets of a failed step, last replaced first.
    fn restore(&mut self) {
        while let Some(replaced) = self.replaced.pop() {
            match (&mut self.tables, replaced) {
                (Tables::Avc(t), Replaced::AvcSps(id, old)) => t.restore_sps(id, old),
                (Tables::Avc(t), Replaced::AvcPps(id, old)) => t.restore_pps(id, old),
                (Tables::Hevc(t), Replaced::HevcSps(id, old)) => t.restore_sps(id, old),
                (Tables::Hevc(t), Replaced::HevcPps(id, old)) => t.restore_pps(id, old),
                _ => {}
            }
        }
        match &mut self.tables {
            Tables::Avc(t) => t.forget_repeats(),
            Tables::Hevc(t) => t.forget_repeats(),
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
                // The reference takes no captions from HEVC suffix SEI.
                if !(self.codec == Codec::Hevc && kind == kinds::SUFFIX_SEI) {
                    recovery = recovery.or(self.read_sei(payload)?);
                }
                continue;
            }
            if matches!(
                (self.codec, kind),
                (
                    Codec::H264,
                    triergon_framing::avc::SPS | triergon_framing::avc::PPS
                ) | (Codec::Hevc, kinds::SPS | kinds::PPS)
            ) {
                // Room to note the set it replaces, before it does.
                self.replaced.try_reserve(1)?;
            }
            match (&mut self.tables, kind) {
                (Tables::Avc(tables), triergon_framing::avc::SPS) => {
                    if let Some((id, old)) = tables.put_sps(payload)? {
                        keep(&mut self.replaced, Replaced::AvcSps(id, old));
                    }
                }
                (Tables::Avc(tables), triergon_framing::avc::PPS) => {
                    if let Some((id, old)) = tables.put_pps(payload)? {
                        keep(&mut self.replaced, Replaced::AvcPps(id, old));
                    }
                }
                (
                    Tables::Avc(tables),
                    triergon_framing::avc::SLICE | triergon_framing::avc::IDR,
                ) if matches!(picture, Picture::None) => {
                    if let Some(slice) = tables.slice(nal) {
                        picture = Picture::Avc(slice);
                    }
                }
                (Tables::Hevc(tables), kinds::SPS) => {
                    if let Some((id, old)) = tables.put_sps(payload)? {
                        if old.is_some() {
                            self.progress.hevc.changed_sps |= 1 << id;
                        }
                        keep(&mut self.replaced, Replaced::HevcSps(id, old));
                    }
                }
                (Tables::Hevc(tables), kinds::PPS) => {
                    if let Some((id, old)) = tables.put_pps(payload)? {
                        keep(&mut self.replaced, Replaced::HevcPps(id, old));
                    }
                }
                (Tables::Hevc(_), kinds::EOS | kinds::EOB) => {
                    if matches!(picture, Picture::None) {
                        self.progress.hevc.after_end = true;
                    } else {
                        end_after = true;
                    }
                }
                (Tables::Hevc(tables), kind)
                    if kinds::is_slice(kind) && matches!(picture, Picture::None) =>
                {
                    if let Some(slice) = tables.slice(nal, &mut self.references)? {
                        picture = Picture::Hevc {
                            slice,
                            after_end: self.progress.hevc.after_end,
                        };
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
                let Some(sps) = tables.sps(slice.sps_id) else {
                    return Ok(());
                };
                let ticks = if slice.header.bottom_field.is_some() {
                    1
                } else {
                    2
                };
                let held = held(self, sps.timing(), ticks);
                self.avc_picture(slice, recovery, held, out)
            }
            Picture::Hevc { slice, after_end } => {
                let Tables::Hevc(tables) = &self.tables else {
                    return Ok(());
                };
                let Some(sps) = tables.sps(slice.sps_id) else {
                    return Ok(());
                };
                let fields = sps.fields;
                let held = held(self, fields.timing, 1);
                self.hevc_picture(slice, fields, after_end, held, out)?;
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
        recovery: Option<u32>,
        mut held: Held,
        out: &mut FrameQueue,
    ) -> Result<()> {
        let Tables::Avc(tables) = &self.tables else {
            return Ok(());
        };
        let Some(sps) = tables.sps(slice.sps_id) else {
            return Ok(());
        };
        let fields = sps.fields;
        let header = slice.header;
        let coding = match fields.poc {
            AvcPocSyntax::Lsb { bits } => AvcPocCoding::Lsb {
                lsb: header.pic_order_cnt_lsb,
                log2_max_lsb: u32::from(bits),
                delta_bottom: header.delta_pic_order_cnt_bottom,
            },
            AvcPocSyntax::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                ..
            } => AvcPocCoding::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                offsets_for_ref_frame: &sps.cycle[..],
                delta: header.delta_pic_order_cnt,
            },
            AvcPocSyntax::FrameNum => AvcPocCoding::FrameNum,
        };
        let state = &mut self.progress.avc;
        let poc = state.order.next(&AvcPocFields {
            idr: slice.idr,
            reference: slice.reference,
            frame_num: u32::from(header.frame_num),
            log2_max_frame_num: u32::from(fields.frame_num_bits),
            field: header.bottom_field.map(Bottom),
            mmco5: header.mmco5,
            coding,
        });
        // The second field of a pair joins the first's frame.
        let parity = header
            .bottom_field
            .map(|bottom| if bottom { Parity::Bottom } else { Parity::Top });
        if let Some(first) = state.first_field.take() {
            if let Some(parity) = parity
                && parity != first.parity
                && first.frame_num == header.frame_num
                && !slice.idr
            {
                let store = AvcStore {
                    poc: poc.stored_poc,
                    marking: Marking::Unused,
                    output: true,
                    field: Some(parity),
                    hold: false,
                };
                self.leaving.try_reserve(1)?;
                let leaving = &mut self.leaving;
                let joined = self.pictures.avc_store_second_field(
                    first.id,
                    store,
                    held,
                    |leave| {
                        if let Leave::Output { poc, picture } = leave {
                            collect(leaving, poc, picture);
                        }
                    },
                    |frame, second| {
                        if frame.end == second.start {
                            frame.end = second.end;
                        }
                    },
                );
                if joined.is_ok() {
                    self.emit_avc(out)?;
                    return self.bump_avc(out);
                }
            } else if let Some(mut entry) = self.pictures.entry(first.id) {
                // An unpaired first field may leave on its own.
                entry.release();
            }
        }
        // The reorder depth: the bitstream restriction's, or one that grows.
        let state = &mut self.progress.avc;
        if let Some(stated) = fields.vui.max_num_reorder_frames {
            if !state.stated || state.stated_depth != stated {
                state.depth = stated;
            }
            state.stated = true;
            state.stated_depth = stated;
        } else {
            if state.stated {
                state.depth = 0;
            }
            state.stated = false;
        }
        state.holds = !state.stated
            && max_dpb_frames(
                fields.profile_idc,
                fields.constraint_set3,
                fields.level_idc,
                fields.frame_mbs,
            )
            .unwrap_or(16)
                >= 2;
        if slice.idr || header.mmco5 {
            self.leaving.try_reserve(self.pictures.len())?;
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
            state.recent.clear();
        }
        let state = &mut self.progress.avc;
        state.decoded = state.decoded.saturating_add(1);
        let order = poc.stored_poc;
        // Far below the last pictures decoded: the pictures waiting leave,
        // then this one at once.
        let far = state.recent.len >= FAR_BACK && state.recent.last(FAR_BACK).all(|q| order < q);
        if far {
            self.leaving.try_reserve(self.pictures.len())?;
            let leaving = &mut self.leaving;
            while self.pictures.bump(|leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            }) {}
            self.emit_avc(out)?;
            self.progress.avc.late.restart();
        } else {
            let displaced = state.recent.last(RECENT).filter(|&q| q > order).count();
            if !state.stated {
                state.depth = state
                    .depth
                    .max(u32::try_from(displaced).unwrap_or(u32::MAX));
            } else if displaced > 0 && !slice.reference && state.decoded <= START_WINDOW {
                // A stated depth of 0 becomes 1 (probe p3-h264-closed-r0).
                state.depth = state.depth.max(1);
            }
        }
        let state = &mut self.progress.avc;
        state.recent.push(order);
        if state.decoded == HOLD_UNTIL + 1 {
            state.early_depth = state.depth;
        }
        // With a bitstream restriction, a reference picture in the start
        // window takes a buffer: the pictures beyond the depth leave first
        // (probe p53).
        if !far && state.stated && slice.reference && state.decoded <= START_WINDOW {
            let depth = state.depth as usize;
            self.leaving.try_reserve(self.pictures.len())?;
            let leaving = &mut self.leaving;
            self.pictures.bump_while_waiting_more_than(depth, |leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            });
            self.emit_avc(out)?;
        }
        let state = &mut self.progress.avc;
        if slice.idr {
            state.recovered = true;
        }
        if let Some(count) = recovery
            && !state.recovered
            && state.target.is_none()
        {
            let max_frame_num = 1u32 << fields.frame_num_bits.min(16);
            state.target = Some(u32::from(header.frame_num).wrapping_add(count) % max_frame_num);
        }
        if !state.recovered && header.intra() {
            held.key = true;
            if !state.keyed {
                state.keyed = true;
                state.floor = Some(poc.stored_poc);
            }
        }
        if !state.recovered && slice.reference && state.target == Some(u32::from(header.frame_num))
        {
            // The recovery point recovers the pictures still waiting too.
            state.recovered = true;
            state.target = None;
            let floor = poc.stored_poc;
            state.floor = Some(state.floor.map_or(floor, |old| old.min(floor)));
            for mut entry in self.pictures.entries_mut() {
                entry.picture_mut().recovered = true;
            }
        }
        held.recovered = self.progress.avc.recovered;
        // Stored without the processes of C.4.5: pictures leave by the
        // reorder depth instead. A first field waits for its second.
        let new = NewPicture {
            poc: poc.stored_poc,
            output: true,
            marking: Marking::Unused,
            field: parity,
            hold: parity.is_some(),
        };
        let id = self
            .pictures
            .store(new, held)
            .map_err(|refused| refused.error)?;
        if let Some(parity) = parity {
            self.progress.avc.first_field = Some(FirstField {
                id,
                frame_num: header.frame_num,
                parity,
            });
        }
        if far {
            self.leaving.try_reserve(self.pictures.len())?;
            let leaving = &mut self.leaving;
            while self.pictures.bump(|leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            }) {}
            return self.emit_avc(out);
        }
        self.bump_avc(out)
    }

    /// Outputs H.264 pictures while more wait than the reorder depth: none
    /// while the start is held, at least one waits in the start window, and
    /// without a bitstream restriction a picture after a gap in the order
    /// counts waits for one more (none at the end of the hold).
    fn bump_avc(&mut self, out: &mut FrameQueue) -> Result<()> {
        let state = &self.progress.avc;
        let index = state.decoded.saturating_sub(1);
        if state.holds
            && (index < HOLD_UNTIL
                || (state.early_depth >= LONG_HOLD_DEPTH && index < START_WINDOW))
        {
            return Ok(());
        }
        loop {
            let state = &self.progress.avc;
            let Some(next) = self.pictures.next_output() else {
                return Ok(());
            };
            let mut depth = state.depth as usize;
            if index < START_WINDOW {
                depth = depth.max(1);
            }
            let follows = state
                .late
                .last()
                .is_none_or(|last| next <= last.saturating_add(NEXT_STEP));
            if !state.stated && !follows {
                if state.holds && index == HOLD_UNTIL {
                    return Ok(());
                }
                depth = depth.max(1);
            }
            if self.pictures.waiting() <= depth {
                return Ok(());
            }
            self.leaving.try_reserve(1)?;
            let leaving = &mut self.leaving;
            self.pictures.bump(|leave| {
                if let Leave::Output { poc, picture } = leave {
                    collect(leaving, poc, picture);
                }
            });
            self.emit_avc(out)?;
        }
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
        sps: hevc::SpsFields,
        after_end: bool,
        held: Held,
        out: &mut FrameQueue,
    ) -> Result<()> {
        let state = &mut self.progress.hevc;
        let kind = slice.nal_type;
        let irap = kinds::is_irap(kind);
        // An IRAP picture activating another SPS, or one whose content
        // changed, starts a new sequence after the pictures waiting are output.
        let new_sps = irap
            && state.started
            && !after_end
            && (state.active_sps != Some(slice.sps_id)
                || state.changed_sps & 1 << slice.sps_id != 0);
        if irap {
            state.no_rasl_output = kinds::is_idr(kind)
                || kinds::is_bla(kind)
                || !state.started
                || after_end
                || new_sps;
        }
        if !state.started && !irap || kinds::is_rasl(kind) && state.no_rasl_output {
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
        let poc = state.order.next(HevcPocFields {
            irap_no_rasl_output: no_rasl_output,
            lsb: slice.poc_lsb,
            log2_max_lsb: sps.log2_max_lsb,
            tid0_anchor: slice.temporal_id == 0
                && !kinds::is_rasl(kind)
                && !kinds::is_radl(kind)
                && !kinds::is_sub_layer_non_ref(kind),
        });
        let limits = HevcLimits {
            max_dec_pic_buffering: None,
            ..sps.limits
        };
        let holding = sps.max_dec_pic_buffering as usize + 1;
        self.leaving.try_reserve(2 * (self.pictures.len() + 1))?;
        let leaving = &mut self.leaving;
        let mut collect = |leave: Leave<'_, Held>| {
            if let Leave::Output { poc, picture } = leave {
                collect(leaving, poc, picture);
            }
        };
        // Observed: room is made before the reference picture set applies.
        self.pictures.bump_while_holding(holding, &mut collect);
        if no_rasl_output {
            self.pictures.mark_unused();
        } else {
            let max_lsb = 1i64 << sps.log2_max_lsb;
            let references = self.references.as_slice();
            self.pictures.retain_references(|stored, _| {
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
                cra: kind == kinds::CRA,
                first,
                no_output_of_prior_pics: slice.no_output_of_prior_pics,
            },
            limits,
            &mut collect,
        );
        self.pictures
            .hevc_store(poc, slice.pic_output, held, limits, &mut collect)
            .map_err(|refused| refused.error)?;
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

    /// Outputs a frame, or keeps it (and those after it) until the duration
    /// estimate it may need is settled.
    fn output(&mut self, held: Held, out: &mut FrameQueue) -> Result<()> {
        let bytes = &self.captions.bytes.as_bytes()[held.start..held.end];
        let progress = &mut self.progress;
        if progress.holding || held.duration.is_none() && !progress.timing.settled() {
            progress.holding = true;
            return self.unsettled.push(held.pts, held.duration, bytes);
        }
        out.push(held.pts, held.duration.or(progress.timing.divisor), bytes)
    }

    /// Releases the frames kept for the duration estimate once it is
    /// settled, or at the end of the stream.
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
                .entries_mut()
                .map(|entry| entry.picture().end - entry.picture().start)
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
                .entries_mut()
                .map(|entry| *entry.picture())
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
