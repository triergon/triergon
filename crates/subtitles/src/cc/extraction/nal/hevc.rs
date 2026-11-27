//! HEVC picture output: which pictures are decoded, and when the pictures
//! waiting leave or are discarded.
//!
//! Order counts follow 8.3.1 and the reference picture sets 8.3.2. A picture is
//! decoded from the first IRAP picture on; RASL pictures of an IRAP picture
//! with NoRaslOutputFlag equal to 1 are not. NoRaslOutputFlag is 1 for an IDR
//! or BLA picture, the first picture, and the first after an end of sequence or
//! end of bitstream NAL unit (8.1.3), and, observed, for an IRAP picture that
//! activates another SPS or one whose content changed, the pictures waiting
//! being output first (`hevc-sps-change-ts`). An SPS has changed when it comes
//! with other content than the last SPS of its id: sets of two ids repeated in
//! turn change nothing, and a changed set of an id the IRAP picture does not
//! activate starts no sequence (`hevc-sps-ids-ts`). A picture that is not
//! decoded, or an access unit without a picture, passes its caption bytes on to
//! the next picture decoded. Output follows C.5.2.2 and C.5.2.3 with
//! `sps_max_num_reorder_pics`, `sps_max_latency_increase_plus1` and
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
//! (`hevc-eos-rasl-ts`). Pictures already output or not to be output are
//! removed, oldest first, once more are stored than the SPS's buffer size and
//! 2: a conforming stream stores no more than its buffer size (C.4), but
//! reference picture sets naming pictures by their order count LSBs can keep
//! any number.

use super::{Held, NalDecode, collect};
use crate::cc::extraction::queue::FrameQueue;
use crate::cc::video::hevc::{self, Reference};
use triergon_framing::hevc as kinds;
use triergon_framing::picture_output::order::{HevcOrder, HevcPocFields};
use triergon_framing::picture_output::{HevcLimits, HevcPicture};

/// HEVC: the pictures stored beyond the SPS's buffer size and this many are
/// removed, oldest first, once they have been output or are not to be. A
/// conforming stream stores no more than the buffer size (C.4); reference
/// picture sets naming pictures by order count LSBs can keep more, without
/// bound.
const HEVC_SLACK: usize = 2;

/// HEVC decoding state between pictures.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HevcState {
    order: HevcOrder,
    /// An IRAP picture has been decoded.
    started: bool,
    /// An end of sequence or end of bitstream NAL unit came after the last
    /// picture decoded.
    pub(super) after_end: bool,
    /// NoRaslOutputFlag of the last IRAP picture.
    no_rasl_output: bool,
    /// The SPS of the last picture decoded, and the ids of the SPSs whose
    /// content changed since.
    active_sps: Option<u8>,
    pub(super) changed_sps: u16,
}

impl NalDecode {
    pub(super) fn hevc_picture(
        &mut self,
        slice: hevc::Slice,
        sps: hevc::SpsFields,
        after_end: bool,
        held: Held,
        out: &mut FrameQueue,
    ) {
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
            return;
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
        // Observed: room is made before the reference picture set applies.
        self.pictures
            .bump_while_holding(holding, collect(&mut self.leaving));
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
            while self.pictures.bump(collect(&mut self.leaving)) {}
        }
        self.pictures.hevc_remove(
            HevcPicture {
                irap_no_rasl_output: no_rasl_output,
                cra: kind == kinds::CRA,
                first,
                no_output_of_prior_pics: slice.no_output_of_prior_pics,
            },
            limits,
            collect(&mut self.leaving),
        );
        self.pictures.remove_oldest_beyond(
            sps.max_dec_pic_buffering as usize + HEVC_SLACK,
            collect(&mut self.leaving),
        );
        // Within the room reserved, the store neither allocates nor fails.
        let _ = self.pictures.hevc_store(
            poc,
            slice.pic_output,
            held,
            limits,
            collect(&mut self.leaving),
        );
        self.emit(out);
    }
}
