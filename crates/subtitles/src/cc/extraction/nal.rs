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
//! names the `subtitle-cc` corpus case that shows it.
//!
//! **H.264.** Order counts follow 8.2.1 with two observed exceptions. After a
//! reference picture with `memory_management_control_operation` 5, the next
//! pictures take its coded `pic_order_cnt_lsb` as prevPicOrderCntLsb where
//! 8.2.1.1 takes 0 (`h264-mmco5-ts`); that picture orders as 0 among those
//! after it, and the pictures before it leave first, as at an IDR picture.
//! After a gap in `frame_num` (order count types 1 and 2), the previous
//! `frame_num` is the one before the picture's and FrameNumOffset stays that of
//! the last picture: no frame is inferred (`h264-gap-ts`). The
//! `no_output_of_prior_pics_flag` of an IDR picture is not honoured: the
//! pictures waiting are output (`h264-noout-ts`). Pictures do not wait for a
//! full buffer (C.4.5): one leaves, lowest order count first, as soon as more
//! pictures wait than the reorder depth (`h264-reorder-ts`). With a bitstream
//! restriction that is its `max_num_reorder_frames` (an SPS stating more than
//! 16 is not taken); among the first 21 pictures a B picture below an earlier
//! one raises a stated 0 to 1 (probes p3-h264-closed-r0, p3-h264-pyr-r0, p88;
//! `h264-stated-b20-ts`; a step does not: `h264-jump-stated-ts`), and
//! up to the twenty-first a reference picture first lets the pictures beyond
//! the depth leave (probes p53, p12-big). Without a bitstream restriction the
//! depth starts at 0 and becomes the number of the last 16 pictures decoded
//! above a picture, when that is more (probes p47, p57; `h264-grow-ts`,
//! `h264-depth-grow-ts`); after a B picture at least one picture waits
//! (probe p50; `h264-b-in-order-ts`), and at depth 0 a picture more than 2
//! above the one before it raises the depth to 1, unless a gap in `frame_num`
//! comes before it in an SPS with `gaps_in_frame_num_value_allowed_flag` 0
//! (probes p74, p88c; `h264-jump-depth-ts`, `h264-gap-jump-ts`,
//! `h264-gap-allowed-ts`; `h264-depth-two-ts`).
//! When MaxDpbFrames (Table A-1) is 2 or more, nothing leaves at first: the
//! hold ends once a decoder outputting from the first picture would have
//! output 7 pictures at a depth below 3 (its depth growing as above, a B
//! picture or such a step raising 0 to 1, and a picture below the last it
//! output dropped), at the twenty-first picture at a deeper depth, or at the
//! picture whose depth equals MaxDpbFrames (probes p27, p47, p62, p64-p69;
//! `h264-start-hold-ts`, `h264-hold-depth1-ts`, `h264-hold-jump-ts`,
//! `h264-hold-step3-ts`, `h264-hold-dpb-ts`). The output of a first picture
//! that is a non-IDR intra picture with `frame_num` 0 does not count (probes
//! p89b, p89c; `h264-start-fn0-ts`). Among the first 20 pictures at least one
//! waits (probe p27). A picture below each of the last 16 decoded makes those
//! waiting leave first, does not raise the depth and is not dropped for being
//! below the first intra picture of a stream without an IDR picture (probes
//! p22, p23, p87, p89e; `h264-far-back-ts`). A picture leaving after a
//! picture with a higher order count has been output (since the last IDR
//! picture or operation 5) is dropped with its captions (`h264-reorder-ts`).
//! The two fields of a pair, in one packet or two, make one frame carrying both
//! fields' captions in decode order, with the first field's times; its VUI
//! frame period is one tick (`h264-fields-ts`). Pictures leave by frames, none
//! while a first field waits for its second (`h264-fields-packed-ts`). A field
//! without its pair is not output; its caption bytes go to the next picture
//! (`h264-fields-lone-ts`).
//!
//! A stream that does not start at an IDR picture outputs nothing until it
//! recovers (`h264-recovery-*`): at an IDR picture, or by the pictures above
//! its first intra picture: those up to the reorder depth are not recovered,
//! and the stream recovers at the last of them (`h264-start-intra-r2-ts`;
//! probe p59), the depth counted when each of them is decoded, so pictures
//! below the intra picture decoded before them can raise it (probes
//! p6-h264-b2-norec-norestr, p6-h264-b2-norec-r0d1; `h264-norestr-ts`), and
//! during the start hold the depth at its end (probe p89;
//! `h264-start-jump-ts`). A B picture among the first 21 pictures makes that
//! depth at least 1, also when it comes after the pictures concerned have
//! left: frames wait until it is known (probes p89c-p89e;
//! `h264-start-b-window-ts`). At depth 0 the stream recovers at the first
//! picture above, and the intra picture is dropped unless it is the stream's
//! first picture with a `frame_num` other than 0 (`h264-start-intra-ts`;
//! probes p59, p60, p61). Reference intra pictures decoded before recovery
//! are output, also among those not recovered (probe p89;
//! `h264-start-intra-run-ts`), and recovery also comes when one of them
//! leaves. A non-reference intra picture is the first only as the stream's
//! first picture with a `frame_num` other than 0, and then recovers the
//! stream only at depth 0 (probe p91; `h264-start-nonref-ts`). A recovery
//! point SEI names a reference picture by its `frame_num`
//! (`recovery_frame_cnt` after the SEI's picture). Before any intra picture,
//! at the first or below it, that picture recovers the stream and the
//! pictures waiting; above it, it recovers the pictures from its order count
//! on, waiting or later, while those below it still count above the intra
//! picture (probe p90; `h264-recovery-above-ts`). Pictures decoded before
//! recovery, and pictures with a lower order count than the first intra
//! picture or the recovery point (as stored, so 0 after operation 5:
//! `h264-mmco5-start-ts`), are dropped with their captions.
//!
//! **HEVC.** Order counts follow 8.3.1 and the reference picture sets 8.3.2. A
//! picture is decoded from the first IRAP picture on; RASL pictures of an IRAP
//! picture with NoRaslOutputFlag equal to 1 are not. NoRaslOutputFlag is 1 for
//! an IDR or BLA picture, the first picture, and the first after an end of
//! sequence or end of bitstream NAL unit (8.1.3), and, observed, for an IRAP
//! picture that activates another SPS or one whose content changed, the
//! pictures waiting being output first (`hevc-sps-change-ts`). A picture that
//! is not decoded, or an access unit without a picture, passes its caption
//! bytes on to the next picture decoded. Output follows C.5.2.2 and C.5.2.3
//! with `sps_max_num_reorder_pics`, `sps_max_latency_increase_plus1` and
//! NoOutputOfPriorPicsFlag: an IRAP picture with NoRaslOutputFlag equal to 1
//! discards the pictures still waiting, with their captions, when it is a CRA
//! picture (C.5.2.2) or has `no_output_of_prior_pics_flag` set, and outputs
//! them otherwise (`hevc-eos-ts`). Observed: the buffer makes room before the
//! current picture's reference picture set is applied, while it holds
//! `sps_max_dec_pic_buffering_minus1 + 2` pictures (C.5.2.2 checks `+ 1` after
//! it); `sps_max_dec_pic_buffering_minus1` is raised to
//! `sps_max_num_reorder_pics` where it is lower, and an SPS where it is then
//! above 15 is not taken (`hevc-dpb-ts`, `hevc-dpb-raise-ts`). A picture that
//! is not an IRAP picture but follows an end of sequence or end of bitstream
//! NAL unit outputs every picture waiting before it is decoded
//! (`hevc-eos-rasl-ts`).
//!
//! **Times.** A frame keeps its packet's PTS. It lasts its packet's duration
//! when the container gives one above 0; otherwise the frame period of its
//! SPS's VUI timing (H.264 frames: two ticks; H.264 fields and HEVC: one tick),
//! truncated to the time base, or 0 when that period is 1/1000 s or less
//! (`h264-vui-fast-ts`); without VUI timing, the duration estimated from the
//! times of the first 21 packets ([`RateEstimate`]: a common divisor of their
//! steps, else the frame rate whose grid they fit best, one tick with fewer
//! than three; `h264-estimate-ts`, `h264-rate-*-ts`). A packet's time is its DTS, else its PTS; in H.264
//! without a bitstream restriction, when a picture up to the sixth raises the
//! depth from 0 by its order count step before any B picture, the packets
//! after it up to the seventh whose DTS is absent or equals their PTS give the
//! estimate no time, up to the first B picture (probes p81, p82, p84;
//! `h264-rate-jump-ts`). Frames
//! that need the estimate wait, with those after them, until it has its 21
//! times or the stream ends.
//!
//! A picture whose parameter sets are missing or invalid is not decoded.
//!
//! A push or the finish either succeeds or leaves the decoder and its output as
//! they were: the picture buffer is copied before a step and put back when it
//! fails, buffers are appended to and cut back, and the parameter sets a step
//! replaces are kept until it succeeds.

use super::captions::Gathered;
use super::queue::FrameQueue;
use super::rate::RateEstimate;
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

/// H.264: the pictures before this index wait for at least one more
/// (probes p27, p51).
const START_WINDOW: u32 = 20;

/// H.264 without a bitstream restriction: the start hold ends once the
/// early decoder ([`EarlyHold`]) has output this many pictures at a depth
/// below [`DEEP`] (probes p27, p47, p62, p64-p68).
const EARLY_OUTPUTS: u32 = 7;

/// H.264 without a bitstream restriction: from this depth the start hold
/// lasts until index [`START_WINDOW`] (probes p48, p50, p69, p70).
const DEEP: u32 = 3;

/// H.264: the order counts of the last pictures decoded that a picture's
/// displacement counts (probes p47, p57).
const RECENT: usize = 16;

/// H.264: a picture with a lower order count than the last this many
/// decoded makes the pictures waiting leave first and does not raise the
/// depth; one below only the last 15 is late (probes p22, p23, p87).
const FAR_BACK: usize = 16;

/// H.264: a picture whose order count is more than this above the picture
/// before it raises a depth of 0 to 1 (probes p31, p32, p64, p66, p74).
const NEXT_STEP: i32 = 2;

/// H.264 without a bitstream restriction: after a picture up to this index
/// that raises the depth from 0 by its order count step, the packets up to
/// this index whose DTS is absent or equals their PTS give the duration
/// estimate no time (probes p81, p82, p84).
const UNTIMED_THROUGH: u32 = 6;

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

/// H.264 streams that start without an IDR picture: whether a B picture
/// comes among the first [`START_WINDOW`] + 1 pictures (probes p89d, p89e).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BWindow {
    #[default]
    Open,
    Seen,
    Closed,
}

/// H.264: a stream that recovered at depth 0 while the [`BWindow`] was
/// open. A B picture in the window drops the first picture above the intra
/// picture and keeps the intra picture; none keeps the first picture above
/// and drops the intra picture unless it is kept (probes p89c-p89e). The
/// frames, in the frames kept until the choice is made.
#[derive(Clone, Copy, Debug, Default)]
struct StartChoice {
    key: Option<usize>,
    above: Option<usize>,
}

/// An H.264 first field waiting for its second.
#[derive(Clone, Copy, Debug)]
struct FirstField {
    id: PictureId,
    frame_num: u16,
    parity: Parity,
}

/// H.264 without a bitstream restriction: the start of the stream as a
/// decoder that outputs from the first picture would see it, which decides
/// when the reference's start hold ends. Its depth grows to each picture's
/// displacement; at depth 0 a B picture, or a picture more than
/// [`NEXT_STEP`] above the one before it, raises it to 1; pictures leave,
/// lowest order count first, while more wait than the depth, and one below
/// the last to leave is dropped (probes p64-p70).
#[derive(Clone, Copy, Debug, Default)]
struct EarlyHold {
    depth: u32,
    waiting: [i32; RECENT + 1],
    len: usize,
    last: Option<i32>,
    previous: Option<i32>,
    outputs: u32,
    /// The first output does not count: the stream starts at a non-IDR intra
    /// picture with `frame_num` 0 (probes p89b, p89c).
    uncounted_first: bool,
    /// The hold has ended.
    ended: bool,
}

impl EarlyHold {
    /// Takes the next picture: its order count, the number of recent
    /// pictures above it, whether it is a B picture and whether a gap in
    /// `frame_num` comes before it.
    fn picture(&mut self, poc: i32, displaced: u32, b: bool, gap: bool) {
        self.depth = self.depth.max(displaced.min(RECENT as u32));
        if self.depth == 0
            && (b
                || !gap
                    && self
                        .previous
                        .is_some_and(|p| poc > p.saturating_add(NEXT_STEP)))
        {
            self.depth = 1;
        }
        self.previous = Some(poc);
        self.waiting[self.len] = poc;
        self.len += 1;
        while self.len > self.depth as usize {
            let (at, &lowest) = self.waiting[..self.len]
                .iter()
                .enumerate()
                .min_by_key(|&(_, &q)| q)
                .unwrap_or((0, &poc));
            self.len -= 1;
            self.waiting[at] = self.waiting[self.len];
            if self.last.is_none_or(|last| lowest >= last) {
                if self.last.is_some() || !self.uncounted_first {
                    self.outputs += 1;
                }
                self.last = Some(lowest);
            }
        }
    }

    /// Whether the hold ends at picture `index`: once [`EARLY_OUTPUTS`]
    /// pictures have left at a depth below [`DEEP`] (probes p64, p66,
    /// p68: 7, 8, 9 or 10 pictures by the drops and raises before), at
    /// index [`START_WINDOW`] at a deeper depth (probes p69, p70), or when
    /// the depth equals MaxDpbFrames (probe p67).
    fn ends(&self, index: u32, dpb_frames: u32) -> bool {
        if self.depth < DEEP {
            self.outputs >= EARLY_OUTPUTS || self.depth == dpb_frames
        } else {
            index >= START_WINDOW || self.depth == dpb_frames
        }
    }
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
    /// A B picture has been decoded (no bitstream restriction).
    b_seen: bool,
    /// MaxDpbFrames of the SPS (Table A-1), and the start hold.
    dpb_frames: u32,
    early: EarlyHold,
    recent: Recent,
    /// The order count of the last picture decoded.
    previous: Option<i32>,
    recovered: bool,
    /// The `frame_num` a recovery point SEI names.
    target: Option<u32>,
    /// An intra picture has been decoded before recovery, its order count
    /// and the pictures above it decoded since.
    keyed: bool,
    key_poc: Option<i32>,
    above_key: u32,
    /// The first intra picture and whether it is kept when the stream
    /// recovers at depth 0.
    key_id: Option<PictureId>,
    key_kept: bool,
    key_reference: bool,
    /// Pictures above the first intra picture were counted during the start
    /// hold, their recovery not yet decided.
    deferred: bool,
    b_window: BWindow,
    choice: Option<StartChoice>,
    /// Pictures from this order count on are recovered: a recovery point
    /// above the first intra picture (probe p90).
    recovered_from: Option<i32>,
    /// Pictures with a lower order count than this are dropped.
    floor: Option<i32>,
    late: LateOutput,
    first_field: Option<FirstField>,
}

impl AvcState {
    /// Whether the first intra picture can recover the stream: a reference
    /// picture, or one kept at depth 0 (probe p91).
    fn key_recovers(&self) -> bool {
        self.key_reference || self.key_kept && self.recovery_depth() == 0
    }

    /// The reorder depth that decides recovery: at least 1 after a B picture
    /// in the [`BWindow`] (probes p89d, p89e).
    fn recovery_depth(&self) -> u32 {
        if self.b_window == BWindow::Seen {
            self.depth.max(1)
        } else {
            self.depth
        }
    }
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
    timing: RateEstimate,
    /// The duration estimate, once settled.
    estimate: Option<i64>,
    /// Frames wait in `unsettled` until the duration estimate is settled
    /// (when one of them needs it) and the start choice is made.
    holding: bool,
    needs_estimate: bool,
    /// The frame kept in `unsettled` that the start choice dropped.
    skip: Option<usize>,
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
            if decode.progress.avc.deferred {
                decode.resolve_recovery();
            }
            if decode.progress.avc.b_window == BWindow::Open {
                decode.progress.avc.b_window = BWindow::Closed;
                decode.choose_start();
            }
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
        // H.264: the other field of the frame, in the same packet.
        let mut second = None;
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
                ) => match picture {
                    Picture::None => {
                        if let Some(slice) = tables.slice(nal) {
                            picture = Picture::Avc(slice);
                        }
                    }
                    Picture::Avc(first) if second.is_none() => {
                        second = tables.slice(nal).filter(|slice| {
                            slice.header.bottom_field.is_some()
                                && slice.header.bottom_field != first.header.bottom_field
                        });
                    }
                    _ => {}
                },
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
        let b_picture = match &picture {
            Picture::Avc(slice) => slice.header.slice_type % 5 == 1,
            _ => false,
        };
        self.progress
            .timing
            .packet(dts.or(pts), dts.is_some_and(|d| Some(d) != pts), b_picture);
        let held = |decode: &Self, timing: Option<Timing>, ticks: u32| Held {
            pts,
            duration: duration
                .filter(|&d| d > 0)
                .or_else(|| timing.map(|t| decode.frame_period(t, ticks))),
            start,
            end: decode.captions.bytes.len(),
            recovered: false,
            key: false,
            above: 0,
            first_key: false,
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
                self.avc_picture(slice, recovery, held, out)?;
                // The second field, without caption bytes of its own: the
                // packet's go to the first.
                match second {
                    Some(slice) => {
                        let end = self.captions.bytes.len();
                        self.avc_picture(
                            slice,
                            None,
                            Held {
                                start: end,
                                end,
                                ..held
                            },
                            out,
                        )
                    }
                    None => Ok(()),
                }
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
        // A gap in `frame_num` (8.2.5.2) before this picture.
        let gap = !slice.idr
            && state
                .order
                .gap(
                    u32::from(header.frame_num),
                    u32::from(fields.frame_num_bits),
                )
                .next()
                .is_some();
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
            } else if let Some(lone) = self.pictures.take(first.id) {
                // An unpaired field is not output: its caption bytes go to
                // the next picture (package case0021, case0116).
                if lone.end == held.start {
                    held.start = lone.start;
                }
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
        state.dpb_frames = max_dpb_frames(
            fields.profile_idc,
            fields.constraint_set3,
            fields.level_idc,
            fields.frame_mbs,
        )
        .unwrap_or(16);
        state.holds = !state.stated && state.dpb_frames >= 2;
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
        if state.b_window == BWindow::Open {
            if header.slice_type % 5 == 1 && state.decoded <= START_WINDOW + 1 {
                state.b_window = BWindow::Seen;
                self.choose_start();
            } else if state.decoded > START_WINDOW + 1 {
                state.b_window = BWindow::Closed;
                self.choose_start();
            }
        }
        let state = &mut self.progress.avc;
        let order = poc.stored_poc;
        // Far below the last pictures decoded: the pictures waiting leave
        // first, it does not raise the depth, and no floor drops it (probe
        // p89e N19-N22 with the B picture below the intra picture).
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
            self.progress.avc.floor = None;
        } else {
            let displaced = state.recent.last(RECENT).filter(|&q| q > order).count();
            if !state.stated {
                state.depth = state
                    .depth
                    .max(u32::try_from(displaced).unwrap_or(u32::MAX));
                if header.slice_type % 5 == 1 {
                    // After a B picture at least one picture waits (probe
                    // p50 k0: a B picture in order at index 1).
                    state.b_seen = true;
                }
                // At depth 0, a picture more than NEXT_STEP above the one
                // before it raises the depth to 1, unless a gap in
                // `frame_num` comes before it (probe p74).
                if state.depth == 0
                    && (!gap || fields.gaps_in_frame_num_allowed)
                    && state
                        .previous
                        .is_some_and(|p| order > p.saturating_add(NEXT_STEP))
                {
                    state.depth = 1;
                    if !state.b_seen {
                        self.progress.timing.untime_through(UNTIMED_THROUGH);
                    }
                }
            } else if displaced > 0
                && header.slice_type % 5 == 1
                && state.decoded <= START_WINDOW + 1
            {
                // A stated depth of 0 becomes 1 (probes p3-h264-closed-r0,
                // p3-h264-pyr-r0).
                state.depth = state.depth.max(1);
            }
        }
        let state = &mut self.progress.avc;
        if state.decoded == 1 {
            state.early.uncounted_first = !slice.idr && header.intra() && header.frame_num == 0;
        }
        if !state.early.ended {
            let displaced = state.recent.last(RECENT).filter(|&q| q > order).count();
            state.early.picture(
                order,
                u32::try_from(displaced).unwrap_or(u32::MAX),
                header.slice_type % 5 == 1,
                gap,
            );
            let index = state.decoded.saturating_sub(1);
            state.early.ended = state.early.ends(index, state.dpb_frames);
        }
        state.recent.push(order);
        state.previous = Some(order);
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
        // Before recovery: the pictures above the first intra picture up to
        // the reorder depth are not recovered, and the stream recovers at the
        // last of them; at depth 0 at the first, where the intra picture is
        // kept only as the stream's first picture with `frame_num` other than
        // 0 (probes p59, p60, p61). During the start hold the depth at its end
        // decides (probe p89).
        // Intra pictures decoded before recovery are output only when they
        // are reference pictures, or a non-reference one that is the
        // stream's first picture with `frame_num` other than 0 (probe p91).
        let key = header.intra() && slice.reference;
        let first_key = !state.recovered
            && !state.keyed
            && header.intra()
            && (slice.reference || state.decoded == 1 && header.frame_num != 0);
        // The reference picture a recovery point names, and the first intra
        // picture's order count before this picture.
        let target = !state.recovered
            && slice.reference
            && state.target == Some(u32::from(header.frame_num));
        let key_poc = state.key_poc;
        let deferring = state.holds && !state.early.ended;
        if state.deferred && !deferring {
            self.resolve_recovery();
        }
        let state = &mut self.progress.avc;
        let mut unrecovered = false;
        if !state.recovered && !first_key && state.key_poc.is_some_and(|key| poc.stored_poc > key) {
            state.above_key = state.above_key.saturating_add(1);
            held.above = state.above_key;
            if deferring {
                state.deferred = true;
            } else {
                let depth = state.recovery_depth();
                unrecovered = state.above_key <= depth;
                if state.above_key >= depth.max(1) {
                    self.recover_above_key();
                }
            }
        }
        let state = &mut self.progress.avc;
        if (!state.recovered || unrecovered) && (key || first_key) {
            held.key = true;
            if !state.keyed {
                state.keyed = true;
                state.floor = Some(poc.stored_poc);
            }
        }
        if first_key {
            held.first_key = true;
            state.key_reference = slice.reference;
            state.key_poc = Some(poc.stored_poc);
            state.above_key = 0;
            state.key_kept = state.decoded == 1 && header.frame_num != 0;
        }
        if target {
            state.target = None;
            if first_key || key_poc.is_none_or(|key| poc.stored_poc < key) {
                // Before any intra picture, at the first or below it: the
                // recovery point recovers the pictures still waiting too.
                state.recovered = true;
                let floor = poc.stored_poc;
                state.floor = Some(state.floor.map_or(floor, |old| old.min(floor)));
                for mut entry in self.pictures.entries_mut() {
                    entry.picture_mut().recovered = true;
                }
            } else {
                // After the first intra picture it recovers itself and the
                // pictures above it, waiting or later; those below it still
                // count above the intra picture (probe p90).
                let floor = poc.stored_poc;
                state.recovered_from = Some(floor);
                for mut entry in self.pictures.entries_mut() {
                    if entry.poc() >= floor {
                        entry.picture_mut().recovered = true;
                    }
                }
            }
        }
        // With a bitstream restriction, a reference picture in the start
        // window takes a buffer: the pictures beyond the depth leave first
        // (probe p53).
        let state = &self.progress.avc;
        if !far && state.stated && slice.reference && state.decoded <= START_WINDOW + 1 {
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
        let state = &self.progress.avc;
        held.recovered = state.recovered && !unrecovered
            || state
                .recovered_from
                .is_some_and(|from| poc.stored_poc >= from);
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
        if first_key {
            self.progress.avc.key_id = Some(id);
        }
        if let Some(parity) = parity {
            self.progress.avc.first_field = Some(FirstField {
                id,
                frame_num: header.frame_num,
                parity,
            });
        }
        self.bump_avc(out)
    }

    /// Decides the recovery of the pictures above the first intra picture
    /// counted during the start hold, by the reorder depth at its end: those
    /// up to the depth are not recovered (probe p89).
    fn resolve_recovery(&mut self) {
        let state = &mut self.progress.avc;
        state.deferred = false;
        let depth = state.recovery_depth();
        if state.above_key < depth.max(1) {
            return;
        }
        if !state.key_recovers() {
            self.recover_above_key();
            return;
        }
        for mut entry in self.pictures.entries_mut() {
            let held = entry.picture_mut();
            if held.above > depth {
                held.recovered = true;
            }
        }
        self.recover_above_key();
    }

    /// The stream recovers by the pictures above the first intra picture;
    /// at depth 0 the intra picture is dropped unless it is kept.
    fn recover_above_key(&mut self) {
        let state = &mut self.progress.avc;
        state.key_poc = None;
        if !state.key_recovers() {
            // A non-reference intra picture recovers the stream only when it
            // is kept at depth 0; otherwise the next intra picture is the
            // first (probe p91: frame_num 0, or reorder 1, outputs nothing).
            state.keyed = false;
            state.floor = None;
            if let Some(id) = state.key_id
                && let Some(mut entry) = self.pictures.entry(id)
            {
                entry.picture_mut().key = false;
            }
            return;
        }
        state.recovered = true;
        if state.recovery_depth() == 0 {
            if state.b_window == BWindow::Open {
                state.choice = Some(StartChoice::default());
            } else {
                self.drop_key();
            }
        }
    }

    /// Drops the first intra picture unless it is kept.
    fn drop_key(&mut self) {
        let state = &self.progress.avc;
        if state.key_kept {
            return;
        }
        if let Some(id) = state.key_id
            && let Some(mut entry) = self.pictures.entry(id)
        {
            entry.picture_mut().key = false;
        }
        if let Some(choice) = state.choice {
            self.progress.skip = choice.key;
        }
    }

    /// Makes the start choice once the [`BWindow`] is decided.
    fn choose_start(&mut self) {
        let Some(choice) = self.progress.avc.choice else {
            return;
        };
        if self.progress.avc.b_window == BWindow::Seen {
            let from = self.progress.avc.recovered_from;
            for mut entry in self.pictures.entries_mut() {
                let poc = entry.poc();
                let held = entry.picture_mut();
                if held.above == 1 && from.is_none_or(|from| poc < from) {
                    held.recovered = false;
                }
            }
            self.progress.skip = choice.above;
        } else {
            self.drop_key();
        }
        self.progress.avc.choice = None;
    }

    /// Outputs H.264 pictures while more wait than the reorder depth: none
    /// while the start is held or a first field waits for its second, and at
    /// least one waits in the start window and, without a bitstream
    /// restriction, after a B picture.
    fn bump_avc(&mut self, out: &mut FrameQueue) -> Result<()> {
        let state = &self.progress.avc;
        // Pictures leave by frames: none while a first field waits for its
        // second (package case0060, case0749).
        if state.first_field.is_some() {
            return Ok(());
        }
        let index = state.decoded.saturating_sub(1);
        if state.holds && !state.early.ended {
            return Ok(());
        }
        loop {
            let state = &self.progress.avc;
            let mut depth = state.depth as usize;
            if index < START_WINDOW || (state.b_seen && !state.stated) {
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
        let needs_estimate = held.duration.is_none() && !progress.timing.settled();
        if progress.holding || needs_estimate || progress.avc.choice.is_some() {
            progress.holding = true;
            progress.needs_estimate |= held.duration.is_none();
            if let Some(choice) = &mut progress.avc.choice
                && !bytes.is_empty()
            {
                let at = Some(self.unsettled.len());
                if held.first_key {
                    choice.key = at;
                } else if held.above == 1 {
                    choice.above = at;
                }
            }
            return self.unsettled.push(held.pts, held.duration, bytes);
        }
        let duration = held.duration.or_else(|| estimate(progress, self.time_base));
        out.push(held.pts, duration, bytes)
    }

    /// Releases the frames kept for the duration estimate once it is
    /// settled, or at the end of the stream.
    fn settle(&mut self, finishing: bool, out: &mut FrameQueue) -> Result<()> {
        let progress = &self.progress;
        if !progress.holding
            || !finishing
                && (progress.avc.choice.is_some()
                    || progress.needs_estimate && !progress.timing.settled())
        {
            return Ok(());
        }
        let estimate = if self.progress.needs_estimate {
            estimate(&mut self.progress, self.time_base)
        } else {
            None
        };
        let skip = self.progress.skip;
        for (at, entry) in self.unsettled.entries().iter().enumerate() {
            if skip != Some(at) {
                out.push(
                    entry.pts,
                    entry.duration.or(estimate),
                    self.unsettled.bytes(entry),
                )?;
            }
        }
        self.unsettled.clear();
        self.progress.holding = false;
        self.progress.needs_estimate = false;
        self.progress.skip = None;
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

/// The settled duration estimate, worked out once.
fn estimate(progress: &mut Progress, time_base: (u32, u32)) -> Option<i64> {
    if progress.estimate.is_none() {
        progress.estimate = progress.timing.duration(time_base);
    }
    progress.estimate
}

#[cfg(test)]
mod tests {
    use super::EarlyHold;

    /// The index at which the start hold ends for pictures given as (order
    /// count, B picture), without frame_num gaps.
    fn hold_end(pictures: &[(i32, bool)], dpb_frames: u32) -> Option<u32> {
        hold_end_from(EarlyHold::default(), pictures, dpb_frames)
    }

    fn hold_end_from(
        mut hold: EarlyHold,
        pictures: &[(i32, bool)],
        dpb_frames: u32,
    ) -> Option<u32> {
        let mut recent: Vec<i32> = Vec::new();
        for (index, &(poc, b)) in (0u32..).zip(pictures) {
            let displaced = recent.iter().rev().take(16).filter(|&&q| q > poc).count();
            hold.picture(poc, u32::try_from(displaced).unwrap(), b, false);
            recent.push(poc);
            if hold.ends(index, dpb_frames) {
                return Some(index);
            }
        }
        None
    }

    /// P frames from order count `first` in steps of 2, with B pictures at
    /// the given indices `below` P frames under the last.
    fn stream(first: i32, bs: &[(usize, usize)], count: usize) -> Vec<(i32, bool)> {
        let mut out = vec![(0, false)];
        let mut p = first - 2;
        let mut ps = vec![0];
        while out.len() < count {
            if let Some(&(_, below)) = bs.iter().find(|&&(at, _)| at == out.len()) {
                out.push((ps[ps.len() - below] - 1, true));
            } else {
                p += 2;
                ps.push(p);
                out.push((p, false));
            }
        }
        out
    }

    #[test]
    fn the_hold_ends_by_the_early_outputs() {
        // Probes p27/p47 (depth 0), p64 (a2: 8, a4: 7), p68 (b2k1-b5k2: 10),
        // p67 (k2: 9), p69 (k3: 20).
        assert_eq!(hold_end(&stream(2, &[], 30), 16), Some(6));
        assert_eq!(hold_end(&stream(2, &[(2, 1)], 30), 16), Some(8));
        assert_eq!(hold_end(&stream(4, &[(2, 1)], 30), 16), Some(7));
        assert_eq!(hold_end(&stream(2, &[(3, 2)], 30), 16), Some(9));
        assert_eq!(hold_end(&stream(2, &[(2, 1), (5, 2)], 30), 16), Some(10));
        assert_eq!(hold_end(&stream(2, &[(4, 3)], 30), 16), Some(20));
    }

    #[test]
    fn a_first_intra_picture_with_frame_num_0_does_not_count() {
        // Probes p89b, p89c: from a non-IDR intra picture with frame_num 0
        // the hold at depth 0 ends at index 7, not 6.
        let hold = EarlyHold {
            uncounted_first: true,
            ..EarlyHold::default()
        };
        assert_eq!(hold_end_from(hold, &stream(2, &[], 30), 16), Some(7));
    }

    #[test]
    fn the_hold_ends_when_the_depth_reaches_max_dpb_frames() {
        // Probe p67: d2-k2 and d3-k3 release at the B picture itself; d2-k3
        // holds to index 20.
        assert_eq!(hold_end(&stream(2, &[(4, 2)], 30), 2), Some(4));
        assert_eq!(hold_end(&stream(2, &[(5, 3)], 30), 3), Some(5));
        assert_eq!(hold_end(&stream(2, &[(4, 3)], 30), 2), Some(20));
    }
}
