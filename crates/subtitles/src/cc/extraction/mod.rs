//! FFmpeg's caption data of decoded video: which frames carry which bytes, in
//! which order and at which time.
//!
//! [`CaptionExtractor`] takes the stream in decode order, one
//! [`VideoPacket`] at a time, and hands out [`CaptionFrame`]s in FFmpeg's
//! output order as soon as they are settled; [`CaptionExtractor::finish`]
//! releases the rest at the end of the stream.
//!
//! FFmpeg's video decoders attach to each output frame the caption constructs
//! of the pictures behind it, and the frames leave in presentation order:
//!
//! * **Bytes.** ATSC A/53 `cc_data` constructs are kept as sent, every
//!   `GA94` user data or T.35 SEI message of the picture in stream order. An
//!   SCTE 20 pair becomes a construct `04` (field 1 data) or `05` (field 2
//!   data) and its two bytes: field numbers 1 and 3 are field 1 data when the
//!   picture's top field comes first and field 2 data otherwise, field number
//!   2 the other way round; a pair of field number 0 becomes `00 00 00`. DVD
//!   caption blocks (two words each) become two constructs, `FC` for a word
//!   marked odd that stands first when `caption_odd_field_first` says so (or
//!   second when it does not), `FD` otherwise; Dish Network pairs become `FC`
//!   constructs. An MPEG-2 stream keeps the format of its first caption user
//!   data and ignores user data of the others.
//! * **MPEG-2.** A B picture decoded before two reference pictures (the
//!   leading B pictures of an open GOP at the start) is not output; its bytes
//!   go to the next picture that is decoded. So do the bytes of a field
//!   picture's second field. Reference pictures leave when the next reference
//!   picture is decoded, B pictures at once, the last reference picture at the
//!   end. A frame lasts its display fields (`repeat_first_field`), each half a
//!   frame period, truncated to the clock. In MPEG-TS a frame keeps its PES
//!   PTS. A raw elementary stream is timed in 1/1200000 s: the first frame
//!   takes the decode time of the picture whose decoding outputs it (each
//!   picture decoding when the ones before it have lasted), each later frame
//!   the time the frame before it ends, and the frame output at the end has
//!   no time.
//! * **H.264 pictures** before the first intra picture are discarded with
//!   their bytes, and so are the ones presented before it (its leading
//!   pictures). When that picture is not an IDR picture and no recovery point
//!   SEI comes with it, the first P picture after its leading pictures is
//!   discarded too.
//! * **HEVC pictures** the decoding process skips (before the first IRAP
//!   picture, and the RASL pictures of an IRAP picture that starts decoding)
//!   pass their bytes to the next decoded picture. Captions come from prefix
//!   SEI NAL units only.
//! * **H.264 and HEVC frames** leave in PTS order (equal times in decode order;
//!   a frame is settled once 16 later captioned frames are decoded, deeper than
//!   decoders reorder) with their packet's duration or, when the container has
//!   none, one frame period of the sequence parameter set's VUI timing (two
//!   clock ticks for H.264, one for HEVC), truncated to the clock; without VUI
//!   timing, the least step between successive decode times so far.
//!
//! Frames without caption bytes are left out.

mod captions;
mod mpeg2;
mod nal;
mod queue;

use super::charge;
use super::video::nal::{Codec, Framing};
use mpeg2::Mpeg2Decode;
use nal::NalDecode;
use queue::FrameQueue;
use triergon_core::{Budget, Result, WorkBudget};

/// The time base of a raw MPEG video elementary stream, as FFmpeg's demuxer sets it.
pub const RAW_MPEG2_TIME_BASE: (u32, u32) = (1, 1_200_000);

/// One output frame's caption bytes and times, in the extractor's time base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptionFrame<'a> {
    pub pts: Option<i64>,
    pub duration: Option<i64>,
    pub data: &'a [u8],
}

/// The coded video an extractor reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCoding {
    /// MPEG-1/2 video timed by PES packets (MPEG-TS, MPEG-PS).
    Mpeg2Pes,
    /// A raw MPEG-1/2 video elementary stream, timed as FFmpeg's demuxer
    /// times it, in [`RAW_MPEG2_TIME_BASE`].
    Mpeg2Raw,
    /// H.264 or HEVC access units.
    Nal(Codec, Framing),
}

/// What the stream hands the extractor next, in decode order.
///
/// MPEG-2: any run of elementary stream bytes, such as one PES payload with
/// its PTS (which belongs to the first picture starting in it; `dts` and
/// `duration` are not used). H.264 and HEVC: one access unit with its
/// container times.
#[derive(Clone, Copy, Debug)]
pub struct VideoPacket<'a> {
    pub data: &'a [u8],
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
}

#[derive(Debug)]
enum Decode {
    Mpeg2(Mpeg2Decode),
    Nal(NalDecode),
}

/// FFmpeg's caption frames of one video stream, extracted incrementally.
///
/// Push packets in decode order, pop frames as they are settled, and call
/// [`finish`](Self::finish) at the end of the stream. Memory is charged to the
/// pool: the MPEG-2 bytes of the picture being received, a few captioned
/// frames waiting for their order, and the frames not yet popped.
#[derive(Debug)]
pub struct CaptionExtractor {
    time_base: (u32, u32),
    decode: Decode,
    output: FrameQueue,
}

impl CaptionExtractor {
    /// An extractor for `coding` whose packet times are in `time_base` (a raw
    /// MPEG-2 stream ignores it and uses [`RAW_MPEG2_TIME_BASE`]).
    #[must_use]
    pub fn new(pool: &Budget, coding: VideoCoding, time_base: (u32, u32)) -> Self {
        let (time_base, decode) = match coding {
            VideoCoding::Mpeg2Pes => (
                time_base,
                Decode::Mpeg2(Mpeg2Decode::new(pool, Some(time_base))),
            ),
            VideoCoding::Mpeg2Raw => (
                RAW_MPEG2_TIME_BASE,
                Decode::Mpeg2(Mpeg2Decode::new(pool, None)),
            ),
            VideoCoding::Nal(codec, framing) => (
                time_base,
                Decode::Nal(NalDecode::new(pool, codec, framing, time_base)),
            ),
        };
        Self {
            time_base,
            decode,
            output: FrameQueue::new(pool),
        }
    }

    /// The time base of the frames' times.
    #[must_use]
    pub fn time_base(&self) -> (u32, u32) {
        self.time_base
    }

    /// Takes the next packet in decode order. Charges one work unit per byte.
    ///
    /// # Errors
    ///
    /// When `work` cannot cover the packet or the pool cannot hold the frames.
    pub fn push(&mut self, packet: VideoPacket<'_>, work: &mut WorkBudget) -> Result<()> {
        charge(work, packet.data.len())?;
        match &mut self.decode {
            Decode::Mpeg2(decode) => decode.push(packet.data, packet.pts, &mut self.output),
            Decode::Nal(decode) => decode.push(
                packet.data,
                packet.pts,
                packet.dts,
                packet.duration,
                &mut self.output,
            ),
        }
    }

    /// The end of the stream: settles every frame still waiting.
    ///
    /// # Errors
    ///
    /// When the pool cannot hold the frames.
    pub fn finish(&mut self) -> Result<()> {
        match &mut self.decode {
            Decode::Mpeg2(decode) => decode.finish(&mut self.output),
            Decode::Nal(decode) => decode.finish(&mut self.output),
        }
    }

    /// The next settled frame, in output order; it stays readable until the
    /// extractor is next used. Frames without caption bytes are not output.
    ///
    /// # Errors
    ///
    /// When the pool cannot hold the remaining frames while it compacts them.
    pub fn pop(&mut self) -> Result<Option<CaptionFrame<'_>>> {
        self.output.pop_front()
    }
}
