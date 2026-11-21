//! H.264 picture output: order counts, the reorder depth, the start hold, field
//! pairs and the recovery of a stream that does not start at an IDR picture.
//!
//! Order counts follow 8.2.1 with two observed exceptions. After a reference
//! picture with `memory_management_control_operation` 5, the next pictures take
//! its coded `pic_order_cnt_lsb` as prevPicOrderCntLsb where 8.2.1.1 takes 0
//! (`h264-mmco5-ts`); that picture orders as 0 among those after it, and the
//! pictures before it leave first, as at an IDR picture. After a gap in
//! `frame_num` (order count types 1 and 2), the previous `frame_num` is the one
//! before the picture's and FrameNumOffset stays that of the last picture: no
//! frame is inferred (`h264-gap-ts`). The `no_output_of_prior_pics_flag` of an
//! IDR picture is not honoured: the pictures waiting are output
//! (`h264-noout-ts`). Pictures do not wait for a full buffer (C.4.5): one
//! leaves, lowest order count first, as soon as more pictures wait than the
//! reorder depth (`h264-reorder-ts`). With a bitstream restriction that is its
//! `max_num_reorder_frames` (an SPS stating more than 16 is not taken); among
//! the first 21 pictures a B picture below an earlier one raises a stated 0 to
//! 1 (`h264-stated-b20-ts`; a step does not: `h264-jump-stated-ts`), and up to
//! the twenty-first a reference picture first lets the pictures beyond the
//! depth leave. Without a bitstream restriction the depth starts at 0 and
//! becomes the number of the last 16 pictures decoded above a picture, when
//! that is more (`h264-grow-ts`, `h264-depth-grow-ts`); after a B picture at
//! least one picture waits (`h264-b-in-order-ts`), and at depth 0 a picture
//! more than 2 above the one before it raises the depth to 1, unless a gap in
//! `frame_num` comes before it in an SPS with
//! `gaps_in_frame_num_value_allowed_flag` 0 (`h264-jump-depth-ts`,
//! `h264-gap-jump-ts`, `h264-gap-allowed-ts`; `h264-depth-two-ts`). When
//! MaxDpbFrames (Table A-1) is 2 or more, nothing leaves at first: the hold
//! ends once a decoder outputting from the first picture would have output 7
//! pictures at a depth below 3 (its depth growing as above, a B picture or such
//! a step raising 0 to 1, and a picture below the last it output dropped), at
//! the twenty-first picture at a deeper depth, or at the picture whose depth
//! equals MaxDpbFrames (`h264-start-hold-ts`, `h264-hold-depth1-ts`,
//! `h264-hold-jump-ts`, `h264-hold-step3-ts`, `h264-hold-dpb-ts`). Where those
//! outputs keep being dropped, the hold ends once more pictures are stored than
//! MaxDpbFrames and 32 (order counts 0, 600, 620, then 100 up by two, where the
//! reference outputs every picture in order; no observation pins the 32). The
//! output of a first picture that is a non-IDR intra picture with `frame_num` 0
//! does not count (`h264-start-fn0-ts`). Among the first 20 pictures at least
//! one waits. A picture below each of the last 16 decoded makes those waiting
//! leave first, does not raise the depth and is not dropped for being below the
//! first intra picture of a stream without an IDR picture (`h264-far-back-ts`).
//! A picture leaving after a picture with a higher order count has been output
//! (since the last IDR picture or operation 5) is dropped with its captions
//! (`h264-reorder-ts`). The two fields of a pair, in one packet or two, make
//! one frame carrying both fields' captions in decode order, with the first
//! field's times; its VUI frame period is one tick (`h264-fields-ts`). Pictures
//! leave by frames, none while a first field waits for its second
//! (`h264-fields-packed-ts`). A field without its pair is not output; its
//! caption bytes go to the next picture (`h264-fields-lone-ts`).
//!
//! A stream that does not start at an IDR picture outputs nothing until it
//! recovers (`h264-recovery-*`): at an IDR picture, or by the pictures above
//! its first intra picture: those up to the reorder depth are not recovered,
//! and the stream recovers at the last of them (`h264-start-intra-r2-ts`), the
//! depth counted when each of them is decoded, so pictures below the intra
//! picture decoded before them can raise it (`h264-norestr-ts`), and during the
//! start hold the depth at its end (`h264-start-jump-ts`). A B picture among
//! the first 21 pictures makes that depth at least 1, also when it comes after
//! the pictures concerned have left: frames wait until it is known
//! (`h264-start-b-window-ts`). At depth 0 the stream recovers at the first
//! picture above, and the intra picture is dropped unless it is the stream's
//! first picture with a `frame_num` other than 0 (`h264-start-intra-ts`).
//! Reference intra pictures decoded before recovery are output, also among
//! those not recovered (`h264-start-intra-run-ts`), and recovery also comes
//! when one of them leaves. A non-reference intra picture is the first only as
//! the stream's first picture with a `frame_num` other than 0, and then
//! recovers the stream only at depth 0 (`h264-start-nonref-ts`). A recovery
//! point SEI names a reference picture by its `frame_num` (`recovery_frame_cnt`
//! after the SEI's picture). Before any intra picture, at the first or below
//! it, that picture recovers the stream and the pictures waiting; above it, it
//! recovers the pictures from its order count on, waiting or later, while those
//! below it still count above the intra picture (`h264-recovery-above-ts`).
//! Pictures decoded before recovery, and pictures with a lower order count than
//! the first intra picture or the recovery point (as stored, so 0 after
//! operation 5: `h264-mmco5-start-ts`), are dropped with their captions.

use super::{Held, NalDecode, Tables, collect};
use crate::cc::extraction::queue::FrameQueue;
use crate::cc::video::avc;
use triergon_framing::avc::AvcPocSyntax;
use triergon_framing::picture_output::levels::max_dpb_frames;
use triergon_framing::picture_output::order::{
    AfterGap, AfterMmco5, AvcOrder, AvcPocCoding, AvcPocFields,
};
use triergon_framing::picture_output::{LateOutput, Marking, NewPicture, Parity, PictureId};

/// H.264: the pictures before this index wait for at least one more.
const START_WINDOW: u32 = 20;

/// H.264 without a bitstream restriction: the start hold ends once the
/// early decoder ([`EarlyHold`]) has output this many pictures at a depth
/// below [`DEEP`].
const EARLY_OUTPUTS: u32 = 7;

/// H.264 without a bitstream restriction: from this depth the start hold
/// lasts until index [`START_WINDOW`].
const DEEP: u32 = 3;

/// H.264 without a bitstream restriction: the start hold ends at the latest
/// once more pictures are stored than MaxDpbFrames and this many. A bound
/// for streams whose early outputs keep being dropped, so that the seven
/// outputs never come, such as order counts 0, 600, 620, then 100 up by two,
/// which the reference outputs in order. No observation pins the margin;
/// every other observed hold ends by index 24.
const HOLD_MARGIN: usize = 32;

/// H.264: the order counts of the last pictures decoded that a picture's
/// displacement counts.
const RECENT: usize = 16;

/// H.264: a picture with a lower order count than the last this many
/// decoded makes the pictures waiting leave first and does not raise the
/// depth; one below only the last 15 is late.
const FAR_BACK: usize = 16;

/// H.264: a picture whose order count is more than this above the picture
/// before it raises a depth of 0 to 1.
const NEXT_STEP: i32 = 2;

/// H.264 streams that start without an IDR picture: whether a B picture
/// comes among the first [`START_WINDOW`] + 1 pictures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum BWindow {
    #[default]
    Open,
    Seen,
    Closed,
}

/// H.264: a stream that recovered at depth 0 while the [`BWindow`] was
/// open. A B picture in the window drops the first picture above the intra
/// picture and keeps the intra picture; none keeps the first picture above
/// and drops the intra picture unless it is kept. The frames, in the frames
/// kept until the choice is made.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StartChoice {
    pub(super) key: Option<usize>,
    pub(super) above: Option<usize>,
}

/// An H.264 first field waiting for its second.
#[derive(Clone, Copy, Debug)]
pub(super) struct FirstField {
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
/// the last to leave is dropped.
#[derive(Clone, Copy, Debug, Default)]
struct EarlyHold {
    depth: u32,
    waiting: [i32; RECENT + 1],
    len: usize,
    last: Option<i32>,
    previous: Option<i32>,
    outputs: u32,
    /// The first output does not count: the stream starts at a non-IDR intra
    /// picture with `frame_num` 0.
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

    /// Whether the hold ends at picture `index`, with `stored` pictures
    /// stored before it: once [`EARLY_OUTPUTS`] pictures have left at a
    /// depth below [`DEEP`] (7, 8, 9 or 10 pictures by the drops and raises
    /// before), at index [`START_WINDOW`] at a deeper depth, when the depth
    /// equals MaxDpbFrames, and at the latest once more than MaxDpbFrames and
    /// [`HOLD_MARGIN`] pictures are stored.
    fn ends(&self, index: u32, dpb_frames: u32, stored: usize) -> bool {
        if stored > dpb_frames as usize + HOLD_MARGIN {
            return true;
        }
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

    /// How many of the order counts are above `poc`: the pictures it is
    /// displaced by.
    fn above(&self, poc: i32) -> u32 {
        // Until they wrap, the order counts fill the array from the start.
        let mut above = 0;
        for &q in &self.pocs[..self.len] {
            above += u32::from(q > poc);
        }
        above
    }

    fn clear(&mut self) {
        self.len = 0;
        self.next = 0;
    }
}

/// H.264 output state between pictures.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AvcState {
    order: AvcOrder,
    /// The reorder depth, and whether (and which) a bitstream restriction
    /// stated it.
    depth: u32,
    stated: bool,
    stated_depth: u32,
    /// Pictures decoded, and whether the start of the stream is held.
    pub(super) decoded: u32,
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
    pub(super) deferred: bool,
    pub(super) b_window: BWindow,
    pub(super) choice: Option<StartChoice>,
    /// Pictures from this order count on are recovered: a recovery point
    /// above the first intra picture.
    recovered_from: Option<i32>,
    /// Pictures with a lower order count than this are dropped.
    floor: Option<i32>,
    late: LateOutput,
    pub(super) first_field: Option<FirstField>,
    /// Packets give the duration estimate no time: the start hold's depth
    /// rose from 0 to 1 and the hold has not ended.
    untimed_run: bool,
}

impl AvcState {
    /// The state before the first picture. After a picture with
    /// `memory_management_control_operation` 5 the order counts take its coded
    /// `pic_order_cnt_lsb`, and after a gap in `frame_num` FrameNumOffset stays
    /// that of the last picture, as the module documentation describes.
    pub(super) fn new() -> Self {
        Self {
            order: AvcOrder::new(AfterMmco5::CodedLsb, AfterGap::OffsetKept),
            ..Self::default()
        }
    }

    /// Whether the first intra picture can recover the stream: a reference
    /// picture, or one kept at depth 0.
    fn key_recovers(&self) -> bool {
        self.key_reference || self.key_kept && self.recovery_depth() == 0
    }

    /// The reorder depth that decides recovery: at least 1 after a B picture
    /// in the [`BWindow`].
    fn recovery_depth(&self) -> u32 {
        if self.b_window == BWindow::Seen {
            self.depth.max(1)
        } else {
            self.depth
        }
    }
}

impl NalDecode {
    /// Decodes an H.264 picture; returns whether its packet is in an untimed
    /// run.
    pub(super) fn avc_picture(
        &mut self,
        slice: avc::Slice,
        recovery: Option<u32>,
        mut held: Held,
        out: &mut FrameQueue,
    ) -> bool {
        let Tables::Avc(tables) = &self.tables else {
            return false;
        };
        let Some(sps) = tables.sps(slice.sps_id) else {
            return false;
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
        let parity = header
            .bottom_field
            .map(|bottom| if bottom { Parity::Bottom } else { Parity::Top });
        let poc = state.order.next(&AvcPocFields {
            idr: slice.idr,
            reference: slice.reference,
            frame_num: u32::from(header.frame_num),
            log2_max_frame_num: u32::from(fields.frame_num_bits),
            field: parity,
            mmco5: header.mmco5,
            coding,
        });
        // The second field of a pair joins the first's frame.
        if let Some(first) = state.first_field.take() {
            if let Some(parity) = parity
                && parity != first.parity
                && first.frame_num == header.frame_num
                && !slice.idr
            {
                let store = NewPicture {
                    poc: poc.stored_poc,
                    marking: Marking::Unused,
                    output: true,
                    field: Some(parity),
                    hold: false,
                };
                let joined = self.pictures.avc_store_second_field(
                    first.id,
                    store,
                    held,
                    collect(&mut self.leaving),
                    |frame, second| {
                        if frame.end == second.start {
                            frame.end = second.end;
                        }
                    },
                );
                if joined.is_ok() {
                    self.emit_avc(out);
                    self.bump_avc(out);
                    return false;
                }
            } else if let Some(lone) = self.pictures.take(first.id) {
                // An unpaired field is not output: its caption bytes go to
                // the next picture (`h264-fields-lone-ts`).
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
            while self.pictures.bump(collect(&mut self.leaving)) {}
            self.emit_avc(out);
            let state = &mut self.progress.avc;
            state.late.restart();
            state.floor = None;
            state.recent.clear();
        }
        let state = &mut self.progress.avc;
        state.decoded = state.decoded.saturating_add(1);
        if state.b_window == BWindow::Open {
            if header.bipredictive() && state.decoded <= START_WINDOW + 1 {
                state.b_window = BWindow::Seen;
                self.choose_start();
            } else if state.decoded > START_WINDOW + 1 {
                state.b_window = BWindow::Closed;
                self.choose_start();
            }
        }
        let state = &mut self.progress.avc;
        let order = poc.stored_poc;
        let displaced = state.recent.above(order);
        // Far below the last pictures decoded: the pictures waiting leave
        // first, it does not raise the depth, and no floor drops it (observed
        // with a B picture below the intra picture).
        let far = state.recent.len >= FAR_BACK && state.recent.last(FAR_BACK).all(|q| order < q);
        if far {
            while self.pictures.bump(collect(&mut self.leaving)) {}
            self.emit_avc(out);
            self.progress.avc.late.restart();
            self.progress.avc.floor = None;
        } else if !state.stated {
            state.depth = state.depth.max(displaced);
            if header.bipredictive() {
                // After a B picture at least one picture waits (observed
                // with a B picture in order at index 1).
                state.b_seen = true;
            }
            // At depth 0, a picture more than NEXT_STEP above the one
            // before it raises the depth to 1, unless a gap in
            // `frame_num` comes before it.
            if state.depth == 0
                && (!gap || fields.gaps_in_frame_num_allowed)
                && state
                    .previous
                    .is_some_and(|p| order > p.saturating_add(NEXT_STEP))
            {
                state.depth = 1;
            }
        } else if displaced > 0 && header.bipredictive() && state.decoded <= START_WINDOW + 1 {
            // A stated depth of 0 becomes 1.
            state.depth = state.depth.max(1);
        }
        if let Some(shown) = &mut self.progress.shown {
            shown.picture(held.pts, header.bipredictive().then_some(displaced > 0));
        }
        let state = &mut self.progress.avc;
        if state.decoded == 1 {
            state.early.uncounted_first = !slice.idr && header.intra() && header.frame_num == 0;
        }
        let mut untimed = false;
        if !state.early.ended {
            let before = state.early.depth;
            state
                .early
                .picture(order, displaced, header.bipredictive(), gap);
            let index = state.decoded.saturating_sub(1);
            state.early.ended = state
                .early
                .ends(index, state.dpb_frames, self.pictures.len());
            // Without a bitstream restriction, once the hold's depth rises
            // from 0 to 1 the packets until the hold ends give no time, but
            // for those of B pictures; a rise above 1 ends the run.
            let after = state.early.depth;
            if after > 1 || state.early.ended {
                state.untimed_run = false;
            }
            untimed = state.untimed_run && !header.bipredictive();
            if !state.stated && before == 0 && after == 1 && !state.early.ended {
                state.untimed_run = true;
            }
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
        // 0. During the start hold the depth at its end decides.
        // Intra pictures decoded before recovery are output only when they
        // are reference pictures, or a non-reference one that is the
        // stream's first picture with `frame_num` other than 0.
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
                // count above the intra picture.
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
        // window takes a buffer: the pictures beyond the depth leave first.
        let state = &self.progress.avc;
        if !far && state.stated && slice.reference && state.decoded <= START_WINDOW + 1 {
            let depth = state.depth as usize;
            self.pictures
                .bump_while_waiting_more_than(depth, collect(&mut self.leaving));
            self.emit_avc(out);
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
        // Within the room reserved, the store neither allocates nor fails.
        let Ok(id) = self.pictures.store(new, held) else {
            return untimed;
        };
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
        self.bump_avc(out);
        untimed
    }

    /// Decides the recovery of the pictures above the first intra picture
    /// counted during the start hold, by the reorder depth at its end: those
    /// up to the depth are not recovered.
    pub(super) fn resolve_recovery(&mut self) {
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
            // first (one with `frame_num` 0, or one at reorder depth 1,
            // outputs nothing).
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
    pub(super) fn choose_start(&mut self) {
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
    fn bump_avc(&mut self, out: &mut FrameQueue) {
        let state = &self.progress.avc;
        // Pictures leave by frames: none while a first field waits for its
        // second (`h264-fields-packed-ts`).
        if state.first_field.is_some() || state.holds && !state.early.ended {
            return;
        }
        let index = state.decoded.saturating_sub(1);
        let mut depth = state.depth as usize;
        let analysing = self.progress.shown.is_some_and(|shown| shown.waits());
        if index < START_WINDOW || state.b_seen && !state.stated || analysing {
            depth = depth.max(1);
        }
        self.pictures
            .bump_while_waiting_more_than(depth, collect(&mut self.leaving));
        self.emit_avc(out);
    }

    /// Outputs the H.264 pictures that left, dropping those not recovered,
    /// below the floor or late.
    pub(super) fn emit_avc(&mut self, out: &mut FrameQueue) {
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
            if let Some(shown) = &mut self.progress.shown {
                shown.output();
            }
            self.output(held, out);
        }
        self.leaving.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{EarlyHold, HOLD_MARGIN};

    /// The index at which the start hold ends for pictures given as (order
    /// count, B picture), without frame_num gaps, every picture before it
    /// stored.
    fn hold_end(pictures: &[(i32, bool)], dpb_frames: u32) -> Option<u32> {
        hold_end_from(EarlyHold::default(), pictures, dpb_frames)
    }

    fn hold_end_from(
        mut hold: EarlyHold,
        pictures: &[(i32, bool)],
        dpb_frames: u32,
    ) -> Option<u32> {
        for (index, &(poc, b)) in (0u32..).zip(pictures) {
            hold.picture(poc, displaced(&pictures[..index as usize], poc), b, false);
            if hold.ends(index, dpb_frames, index as usize) {
                return Some(index);
            }
        }
        None
    }

    /// How many of the last 16 order counts are above `poc`.
    fn displaced(before: &[(i32, bool)], poc: i32) -> u32 {
        let above = before.iter().rev().take(16).filter(|&&(q, _)| q > poc);
        u32::try_from(above.count()).unwrap()
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
        // As observed: index 6 at depth 0; 8 after a B picture one below (7
        // with P frames from order count 4); 9 after one two below; 10 after
        // one below and a later one two below; 20 at depth 3.
        assert_eq!(hold_end(&stream(2, &[], 30), 16), Some(6));
        assert_eq!(hold_end(&stream(2, &[(2, 1)], 30), 16), Some(8));
        assert_eq!(hold_end(&stream(4, &[(2, 1)], 30), 16), Some(7));
        assert_eq!(hold_end(&stream(2, &[(3, 2)], 30), 16), Some(9));
        assert_eq!(hold_end(&stream(2, &[(2, 1), (5, 2)], 30), 16), Some(10));
        assert_eq!(hold_end(&stream(2, &[(4, 3)], 30), 16), Some(20));
    }

    #[test]
    fn a_first_intra_picture_with_frame_num_0_does_not_count() {
        // From a non-IDR intra picture with frame_num 0 the hold at depth 0
        // ends at index 7, not 6.
        let hold = EarlyHold {
            uncounted_first: true,
            ..EarlyHold::default()
        };
        assert_eq!(hold_end_from(hold, &stream(2, &[], 30), 16), Some(7));
    }

    #[test]
    fn the_hold_ends_when_the_depth_reaches_max_dpb_frames() {
        // A B picture two below at MaxDpbFrames 2, or three below at 3,
        // releases the hold itself; three below at MaxDpbFrames 2 it holds to
        // index 20.
        assert_eq!(hold_end(&stream(2, &[(4, 2)], 30), 2), Some(4));
        assert_eq!(hold_end(&stream(2, &[(5, 3)], 30), 3), Some(5));
        assert_eq!(hold_end(&stream(2, &[(4, 3)], 30), 2), Some(20));
    }

    #[test]
    fn the_hold_ends_once_more_pictures_are_stored_than_it_can_hold() {
        // 0, 30000 (a step raising the depth to 1), 30010, then 50, 51, 52
        // and on (depth 2): every later picture leaves below the last output
        // and is dropped, so the seven outputs never come. The hold ends
        // once more than MaxDpbFrames (3) and the margin are stored.
        let pictures: Vec<_> = [0, 30_000, 30_010]
            .into_iter()
            .chain(50..30_000)
            .map(|poc| (poc, false))
            .collect();
        assert_eq!(
            hold_end(&pictures, 3),
            Some(3 + u32::try_from(HOLD_MARGIN).unwrap() + 1)
        );
        let mut hold = EarlyHold::default();
        for (index, &(poc, b)) in pictures.iter().enumerate().take(1000) {
            hold.picture(poc, displaced(&pictures[..index], poc), b, false);
        }
        assert_eq!((hold.depth, hold.outputs), (2, 2));
    }
}
