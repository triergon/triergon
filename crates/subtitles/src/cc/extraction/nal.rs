//! FFmpeg's H.264 and HEVC decoders as far as captions go: which pictures
//! they output, with which caption bytes, released in presentation order.

use super::captions::Gathered;
use super::queue::FrameQueue;
use crate::cc::video::nal::{
    self, Codec, Framing, SEI_RECOVERY_POINT, SEI_USER_DATA_REGISTERED, SliceType, Timing, h264,
    hevc,
};
use triergon_core::{Budget, Result};

/// Frames wait for release until more than this many are decoded after them:
/// deeper than any picture reordering the decoders allow.
const REORDER_DEPTH: usize = 16;

/// Access units in decode order.
#[derive(Debug)]
pub(super) struct NalDecode {
    codec: Codec,
    framing: Framing,
    time_base: (u32, u32),
    timing: Option<Timing>,
    h264: H264Output,
    hevc: HevcDecoding,
    gathered: Gathered,
    /// The least step between successive decode times so far.
    last_decode: Option<i64>,
    step: Option<i64>,
    /// Decoded frames not yet released.
    reorder: FrameQueue,
}

impl NalDecode {
    pub(super) fn new(
        pool: &Budget,
        codec: Codec,
        framing: Framing,
        time_base: (u32, u32),
    ) -> Self {
        Self {
            codec,
            framing,
            time_base,
            timing: None,
            h264: H264Output::default(),
            hevc: HevcDecoding::default(),
            gathered: Gathered::new(pool),
            last_decode: None,
            step: None,
            reorder: FrameQueue::new(pool),
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
        if let Some(at) = dts.or(pts) {
            if let Some(delta) = self.last_decode.and_then(|last| at.checked_sub(last))
                && delta > 0
            {
                self.step = Some(self.step.map_or(delta, |s| s.min(delta)));
            }
            self.last_decode = Some(at);
        }
        let codec = self.codec;
        let mut picture = Picture::default();
        for nal in nal::nal_units(data, self.framing) {
            let Some(nal_type) = codec.nal_type(nal) else {
                continue;
            };
            match (codec, nal_type) {
                (_, kind) if codec.is_slice(kind) => {
                    if picture.nal_type.is_none() {
                        picture.nal_type = Some(kind);
                        if codec == Codec::H264 {
                            picture.slice_type = nal::h264_slice_type(nal);
                        }
                    }
                }
                (Codec::H264, h264::SPS) | (Codec::Hevc, hevc::SPS) => {
                    if let Some(found) = nal::sps_timing(codec, nal) {
                        self.timing = Some(found);
                    }
                }
                (Codec::Hevc, hevc::EOS) => picture.end_of_sequence = true,
                (Codec::H264, h264::SEI) | (Codec::Hevc, hevc::PREFIX_SEI) => {
                    let gathered = &mut self.gathered;
                    let mut result = Ok(());
                    let mut buffer = [0; 256];
                    nal::sei_messages(codec.payload(nal), &mut buffer, |message| {
                        match message.payload_type {
                            SEI_USER_DATA_REGISTERED if result.is_ok() => {
                                result = gathered.sei(message.payload);
                            }
                            SEI_RECOVERY_POINT => picture.recovery_point = true,
                            _ => {}
                        }
                    });
                    result?;
                }
                _ => {}
            }
        }
        let fate = match codec {
            Codec::H264 => self.h264.fate(&picture, pts),
            Codec::Hevc => self.hevc.fate(&picture),
        };
        match fate {
            Fate::Output => {
                self.reorder
                    .push(pts, duration, self.gathered.bytes.as_bytes())?;
            }
            Fate::Discard => {}
            Fate::Skip => return Ok(()),
        }
        self.gathered.bytes.clear();
        while self.reorder.len() > REORDER_DEPTH {
            self.release(out)?;
        }
        Ok(())
    }

    /// The end of the stream: every frame still waiting.
    pub(super) fn finish(&mut self, out: &mut FrameQueue) -> Result<()> {
        while self.reorder.len() > 0 {
            self.release(out)?;
        }
        Ok(())
    }

    /// Releases the waiting frame presented first (equal times in decode
    /// order, unknown times last), lasting its packet's duration or one frame
    /// period.
    fn release(&mut self, out: &mut FrameQueue) -> Result<()> {
        let Some(index) = self
            .reorder
            .entries()
            .iter()
            .enumerate()
            .min_by_key(|(i, e)| (e.pts.is_none(), e.pts, *i))
            .map(|(i, _)| i)
        else {
            return Ok(());
        };
        let period = self
            .timing
            .and_then(|t| stream_period(self.codec, t, self.time_base))
            .or(self.step);
        self.reorder.remove(index, |entry, bytes| {
            out.push(entry.pts, entry.duration.or(period), bytes)
        })
    }
}

/// What FFmpeg's decoder does with a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// Decoded and output.
    Output,
    /// Decoded, then discarded with its caption bytes.
    Discard,
    /// Not decoded: its caption bytes go to the next decoded picture.
    Skip,
}

/// The facts of one access unit that decide its fate.
#[derive(Clone, Copy, Debug, Default)]
struct Picture {
    /// The first slice's `nal_unit_type`, `None` without a slice.
    nal_type: Option<u8>,
    /// H.264: the first slice's `slice_type`.
    slice_type: Option<SliceType>,
    /// H.264: a recovery point SEI message.
    recovery_point: bool,
    /// HEVC: an end of sequence NAL unit.
    end_of_sequence: bool,
}

/// FFmpeg's H.264 output: pictures before the first intra picture are
/// discarded, and so are pictures presented before that one (its leading
/// pictures). Started at a non-IDR intra picture without a recovery point SEI,
/// it also discards the first P picture after leading pictures.
#[derive(Debug, Default)]
struct H264Output {
    start: Option<Start>,
}

#[derive(Clone, Copy, Debug)]
struct Start {
    pts: Option<i64>,
    heuristic: bool,
    discard_next_p: bool,
}

impl H264Output {
    fn fate(&mut self, picture: &Picture, pts: Option<i64>) -> Fate {
        let Some(nal_type) = picture.nal_type else {
            return Fate::Discard;
        };
        let intra = picture.slice_type.is_some_and(SliceType::is_intra);
        let Some(start) = self.start.as_mut() else {
            if nal_type == h264::IDR || intra {
                self.start = Some(Start {
                    pts,
                    heuristic: nal_type != h264::IDR && !picture.recovery_point,
                    discard_next_p: false,
                });
                return Fate::Output;
            }
            return Fate::Discard;
        };
        if let (Some(pts), Some(first)) = (pts, start.pts)
            && pts < first
        {
            start.discard_next_p |= start.heuristic;
            return Fate::Discard;
        }
        if start.discard_next_p && matches!(picture.slice_type, Some(SliceType::P | SliceType::Sp))
        {
            start.discard_next_p = false;
            return Fate::Discard;
        }
        Fate::Output
    }
}

/// HEVC decoding (8.1.3): nothing before the first IRAP picture, and no RASL
/// picture of an IRAP picture that starts decoding (the first, any after an
/// end of sequence, a BLA picture).
#[derive(Debug, Default)]
struct HevcDecoding {
    started: bool,
    after_end_of_sequence: bool,
    skip_rasl: bool,
}

impl HevcDecoding {
    fn fate(&mut self, picture: &Picture) -> Fate {
        let Some(nal_type) = picture.nal_type else {
            return Fate::Skip;
        };
        let fate = if (hevc::BLA_W_LP..=hevc::IRAP_LAST).contains(&nal_type) {
            self.skip_rasl =
                !self.started || self.after_end_of_sequence || nal_type <= hevc::BLA_N_LP;
            self.started = true;
            self.after_end_of_sequence = false;
            Fate::Output
        } else if !self.started
            || (self.skip_rasl && matches!(nal_type, hevc::RASL_N | hevc::RASL_R))
        {
            Fate::Skip
        } else {
            Fate::Output
        };
        self.after_end_of_sequence |= picture.end_of_sequence;
        fate
    }
}

/// The frame period of VUI timing in `time_base` units, truncated: two clock
/// ticks for H.264, one for HEVC.
fn stream_period(codec: Codec, timing: Timing, (num, den): (u32, u32)) -> Option<i64> {
    let ticks = match codec {
        Codec::H264 => 2,
        Codec::Hevc => 1,
    };
    let numerator = i128::from(den) * i128::from(timing.num_units_in_tick) * ticks;
    let denominator = i128::from(num) * i128::from(timing.time_scale);
    (denominator > 0)
        .then(|| numerator / denominator)
        .and_then(|period| i64::try_from(period).ok())
}
