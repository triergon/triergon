//! Picture order counts: H.264 8.2.1 (all three types) and H.265 8.3.1.

use super::Parity;

/// How an H.264 picture codes its order count (the SPS's
/// `pic_order_cnt_type` and the slice header's fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvcPocCoding<'a> {
    /// Type 0: `pic_order_cnt_lsb` of `log2_max_lsb` bits and
    /// `delta_pic_order_cnt_bottom`.
    Lsb {
        lsb: u32,
        log2_max_lsb: u32,
        delta_bottom: i32,
    },
    /// Type 1: the SPS's cycle of reference frame offsets and the slice's
    /// `delta_pic_order_cnt[ 0..2 ]`.
    Cycle {
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        offsets_for_ref_frame: &'a [i32],
        delta: [i32; 2],
    },
    /// Type 2: output order is decode order.
    FrameNum,
}

/// The fields of an H.264 picture that its order count depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcPocFields<'a> {
    pub idr: bool,
    /// `nal_ref_idc` is not 0.
    pub reference: bool,
    pub frame_num: u32,
    /// `log2_max_frame_num_minus4 + 4`.
    pub log2_max_frame_num: u32,
    /// The field of a field picture (`field_pic_flag` and
    /// `bottom_field_flag`).
    pub field: Option<Parity>,
    /// The picture has `memory_management_control_operation` equal to 5.
    pub mmco5: bool,
    pub coding: AvcPocCoding<'a>,
}

/// What a type 0 order count takes from a reference picture with
/// `memory_management_control_operation` 5 for the pictures after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AfterMmco5 {
    /// 8.2.1.1: its TopFieldOrderCnt after the reset of 8.2.1 (0 for a
    /// frame whose bottom field does not come first).
    #[default]
    Specified,
    /// Its `pic_order_cnt_lsb` as coded (observed in FFmpeg's H.264 output,
    /// case `h264-mmco5-ts`).
    CodedLsb,
}

/// How a gap in `frame_num` (a `frame_num` other than PrevRefFrameNum and its
/// successor, 7.4.3) reaches the order counts of types 1 and 2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AfterGap {
    /// 8.2.5.2: the caller passes each inferred frame of the gap to
    /// [`AvcOrder::gap_frame`] before the picture, each the previous picture
    /// of the next.
    #[default]
    Specified,
    /// No frame is inferred: the previous `frame_num` is the one before the
    /// picture's and FrameNumOffset that of the last picture decoded, so a
    /// gap across `frame_num` 0 does not advance FrameNumOffset (observed in
    /// FFmpeg's H.264 output, case `h264-gap-ts`).
    OffsetKept,
}

/// The order count state of an H.264 stream between pictures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvcOrder {
    after_mmco5: AfterMmco5,
    after_gap: AfterGap,
    /// PrevRefFrameNum, once a reference picture has been decoded.
    prev_ref_frame_num: Option<u32>,
    /// prevPicOrderCntMsb and prevPicOrderCntLsb for the next picture.
    prev_msb: i32,
    prev_lsb: i32,
    /// FrameNumOffset and frame_num of the previous picture, and whether it
    /// had `memory_management_control_operation` 5.
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
    prev_mmco5: bool,
}

/// A picture's order counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcPoc {
    /// `PicOrderCnt( )` (8-1) as derived for the picture.
    pub poc: i32,
    /// `PicOrderCnt( )` after decoding: 0 after
    /// `memory_management_control_operation` 5 (8.2.1), which orders it
    /// among the pictures of the buffer.
    pub stored_poc: i32,
}

impl AvcOrder {
    #[must_use]
    pub fn new(after_mmco5: AfterMmco5, after_gap: AfterGap) -> Self {
        Self {
            after_mmco5,
            after_gap,
            ..Self::default()
        }
    }

    /// PrevRefFrameNum (7.4.3), once a reference picture has been decoded.
    #[must_use]
    pub fn prev_ref_frame_num(&self) -> Option<u32> {
        self.prev_ref_frame_num
    }

    /// PrevRefFrameNum modulo MaxFrameNum `max`: an SPS may only change
    /// `log2_max_frame_num_minus4` at an IDR picture (7.4.1.2.1), and one
    /// that does so elsewhere counts on from the value as the new width takes
    /// it.
    fn prev_ref_within(&self, max: u32) -> Option<u32> {
        self.prev_ref_frame_num.map(|prev| prev & (max - 1))
    }

    /// The `frame_num` values of the frames a gap before a picture with
    /// `frame_num` infers (8.2.5.2): those after PrevRefFrameNum up to the
    /// picture's, modulo MaxFrameNum. None after an IDR picture's.
    pub fn gap(&self, frame_num: u32, log2_max_frame_num: u32) -> impl Iterator<Item = u32> {
        let max = 1u32 << log2_max_frame_num.min(16);
        let frame_num = frame_num & (max - 1);
        let (first, count) = match self.prev_ref_within(max) {
            Some(prev) if frame_num != prev && frame_num != (prev + 1) % max => {
                ((prev + 1) % max, (frame_num + max - prev - 1) % max)
            }
            _ => (0, 0),
        };
        (0..count).map(move |k| (first + k) % max)
    }

    /// The order counts of an inferred "non-existing" frame of a gap
    /// (8.2.5.2), a reference frame without memory management operations.
    /// Type 0 counts leave the state untouched; their value is not used.
    pub fn gap_frame(
        &mut self,
        frame_num: u32,
        log2_max_frame_num: u32,
        coding: AvcPocCoding<'_>,
    ) -> AvcPoc {
        let saved = (self.prev_msb, self.prev_lsb);
        let poc = self.next(&AvcPocFields {
            idr: false,
            reference: true,
            frame_num,
            log2_max_frame_num,
            field: None,
            mmco5: false,
            coding,
        });
        if matches!(coding, AvcPocCoding::Lsb { .. }) {
            (self.prev_msb, self.prev_lsb) = saved;
        }
        poc
    }

    /// The order counts of the next picture in decode order. `frame_num` is
    /// taken modulo MaxFrameNum.
    pub fn next(&mut self, f: &AvcPocFields<'_>) -> AvcPoc {
        let max_frame_num = 1i64 << f.log2_max_frame_num.min(16);
        let max = max_frame_num as u32;
        let frame_num = f.frame_num & (max - 1);
        let prev = if self.prev_mmco5 {
            0
        } else {
            self.prev_frame_num_offset
        };
        if self.after_gap == AfterGap::OffsetKept
            && let Some(prev_ref) = self.prev_ref_within(max)
            && !f.idr
            && frame_num != prev_ref
            && frame_num != (prev_ref + 1) % max
        {
            self.prev_frame_num = (frame_num + max - 1) % max;
        }
        let frame_num_offset = if f.idr {
            0
        } else if self.prev_frame_num > frame_num {
            (i64::from(prev) + max_frame_num) as i32
        } else {
            prev
        };
        let (top, bottom) = match f.coding {
            AvcPocCoding::Lsb {
                lsb,
                log2_max_lsb,
                delta_bottom,
            } => {
                let max_lsb = 1i64 << log2_max_lsb.min(16);
                let (prev_msb, prev_lsb) = if f.idr {
                    (0, 0)
                } else {
                    (i64::from(self.prev_msb), i64::from(self.prev_lsb))
                };
                let lsb = i64::from(lsb);
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = (msb + lsb) as i32;
                let bottom = match f.field {
                    None => top.wrapping_add(delta_bottom),
                    Some(_) => top,
                };
                if f.reference {
                    (self.prev_msb, self.prev_lsb) = if !f.mmco5 {
                        (msb as i32, lsb as i32)
                    } else if f.field == Some(Parity::Bottom) {
                        (0, 0)
                    } else {
                        match self.after_mmco5 {
                            AfterMmco5::Specified => (
                                0,
                                top.wrapping_sub(Self::pic_order_cnt(top, bottom, f.field)),
                            ),
                            AfterMmco5::CodedLsb => (0, lsb as i32),
                        }
                    };
                }
                (top, bottom)
            }
            AvcPocCoding::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                offsets_for_ref_frame,
                delta,
            } => {
                let cycle = offsets_for_ref_frame.len() as i64;
                let mut abs = if cycle != 0 {
                    i64::from(frame_num_offset) + i64::from(frame_num)
                } else {
                    0
                };
                if !f.reference && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i64;
                if abs > 0 {
                    let per_cycle: i64 = offsets_for_ref_frame.iter().map(|&o| i64::from(o)).sum();
                    expected = (abs - 1) / cycle * per_cycle;
                    let within = ((abs - 1) % cycle) as usize;
                    expected += offsets_for_ref_frame[..=within]
                        .iter()
                        .map(|&o| i64::from(o))
                        .sum::<i64>();
                }
                if !f.reference {
                    expected += i64::from(offset_for_non_ref_pic);
                }
                let expected = expected as i32;
                match f.field {
                    None => {
                        let top = expected.wrapping_add(delta[0]);
                        let bottom = top
                            .wrapping_add(offset_for_top_to_bottom_field)
                            .wrapping_add(delta[1]);
                        (top, bottom)
                    }
                    Some(Parity::Top) => {
                        let top = expected.wrapping_add(delta[0]);
                        (top, top)
                    }
                    Some(Parity::Bottom) => {
                        let bottom = expected
                            .wrapping_add(offset_for_top_to_bottom_field)
                            .wrapping_add(delta[0]);
                        (bottom, bottom)
                    }
                }
            }
            AvcPocCoding::FrameNum => {
                let temp = if f.idr {
                    0
                } else {
                    let doubled = 2 * (i64::from(frame_num_offset) + i64::from(frame_num));
                    (if f.reference { doubled } else { doubled - 1 }) as i32
                };
                (temp, temp)
            }
        };
        self.prev_frame_num_offset = frame_num_offset;
        // A picture with operation 5 counts as frame_num 0 afterwards (7.4.3).
        self.prev_frame_num = if f.mmco5 { 0 } else { frame_num };
        self.prev_mmco5 = f.mmco5;
        if f.reference {
            self.prev_ref_frame_num = Some(self.prev_frame_num);
        }
        let poc = Self::pic_order_cnt(top, bottom, f.field);
        AvcPoc {
            poc,
            stored_poc: if f.mmco5 { 0 } else { poc },
        }
    }

    /// PicOrderCnt( ) (8-1).
    fn pic_order_cnt(top: i32, bottom: i32, field: Option<Parity>) -> i32 {
        match field {
            None => top.min(bottom),
            Some(Parity::Top) => top,
            Some(Parity::Bottom) => bottom,
        }
    }
}

/// The order count state of an H.265 stream between pictures: the previous
/// picture of TemporalId 0 that is not a RASL, RADL or sub-layer
/// non-reference picture (prevTid0Pic, 8.3.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HevcOrder {
    prev_tid0_poc: i32,
}

/// The fields of an H.265 picture that its order count depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcPocFields {
    /// An IRAP picture with NoRaslOutputFlag equal to 1.
    pub irap_no_rasl_output: bool,
    /// `slice_pic_order_cnt_lsb` (0 for an IDR picture).
    pub lsb: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4 + 4`.
    pub log2_max_lsb: u32,
    /// TemporalId is 0 and the picture is not a RASL, RADL or sub-layer
    /// non-reference picture: it becomes prevTid0Pic.
    pub tid0_anchor: bool,
}

impl HevcOrder {
    /// PicOrderCntVal of the next picture in decode order (8-1 to 8-3).
    pub fn next(&mut self, f: HevcPocFields) -> i32 {
        let max_lsb = 1i64 << f.log2_max_lsb.min(16);
        let lsb = i64::from(f.lsb);
        let msb = if f.irap_no_rasl_output {
            0
        } else {
            let prev = i64::from(self.prev_tid0_poc);
            let prev_lsb = prev & (max_lsb - 1);
            let prev_msb = prev - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = (msb + lsb) as i32;
        if f.tid0_anchor {
            self.prev_tid0_poc = poc;
        }
        poc
    }
}
