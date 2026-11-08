//! Closed captions carried in video (ATSC A/53 `cc_data()`, SCTE 20 and the
//! other caption user data FFmpeg reads), decoded to timed text.
//!
//! [`CaptionBridge`] takes the selected video track's packets in decode order,
//! wherever the caller holds them: H.264 or HEVC access units (an MP4 or
//! Matroska sample, a TS PES payload, a framing bridge's Annex B output) or
//! MPEG-2 PES payloads. FFmpeg's caption extraction ([`CaptionExtractor`])
//! hands out the captioned frames in FFmpeg's output order and FFmpeg's
//! `ccaption` receiver ([`Decoder`]) decodes them. Every event line becomes
//! an opaque ASS [`TextSource`] for the
//! [`TextBridge`](crate::text_bridge::TextBridge), so captions arrive as the
//! cues of a Matroska ASS track do.
use crate::text_bridge::{AssIdentity, TextSource, receiver_timing};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, Rounding, SourceId, TimeBase, WorkBudget};
use triergon_subtitles::compat::cc::{
    CaptionExtractor, Clock, Decoder, Options, Subtitle, VideoCoding, VideoPacket,
};

/// The video the captions come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptionVideo {
    pub coding: VideoCoding,
    /// The clock of the packets' times.
    pub time_base: TimeBase,
    /// The largest packet accepted.
    pub max_packet_bytes: usize,
}

/// One video packet in decode order, its times in the video's time base: an
/// H.264 or HEVC access unit, or a run of MPEG-2 video such as a PES payload.
#[derive(Clone, Debug)]
pub struct CaptionUnit {
    pub data: Bytes,
    pub pts: Option<i64>,
    /// The decode time, where the container states one. A TS PES without a
    /// DTS decodes at its PTS (H.222.0 §2.4.3.7); Matroska states none.
    pub dts: Option<i64>,
    pub duration: Option<i64>,
}

#[derive(Debug)]
pub struct CaptionRejection {
    pub unit: CaptionUnit,
    pub error: Error,
}

/// What a poll did.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the cue moves its charged owners inline, as the text bridge's steps do"
)]
pub enum CaptionStep {
    /// Provide the next packet.
    NeedUnit,
    /// The work budget ran out; poll again.
    Yield,
    /// One event line for the text bridge.
    Cue(TextSource),
    /// Input finished and every caption was delivered.
    Finished,
}

/// The packet being read, and the work paid towards it (one unit per byte).
#[derive(Debug)]
struct Pending {
    unit: CaptionUnit,
    paid: u64,
}

/// Where the stream's end stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Open,
    Finishing,
    /// The extractor released its last frames.
    Extracted,
    /// The receiver drained.
    Drained,
}

/// The selected video track's captions for one source and generation. A seek,
/// track switch or configuration change needs a new bridge.
pub struct CaptionBridge {
    pool: Budget,
    source: SourceId,
    generation: u64,
    max_packet_bytes: usize,
    extractor: CaptionExtractor,
    /// The clock of the extracted frames.
    frame_clock: TimeBase,
    decoder: Decoder,
    header: Bytes,
    /// The last decoded event and how many of its lines were delivered.
    subtitle: Subtitle,
    delivered: Option<usize>,
    pending: Option<Pending>,
    sequence: u64,
    end: End,
}

impl CaptionBridge {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        generation: u64,
        video: CaptionVideo,
        options: Options,
    ) -> Result<Self> {
        let part = |value: u64| u32::try_from(value).map_err(|_| Error::Overflow);
        let time_base = (
            part(video.time_base.numerator())?,
            part(video.time_base.denominator())?,
        );
        let extractor = CaptionExtractor::new(pool, video.coding, time_base);
        let (numerator, denominator) = extractor.time_base();
        let decoder = Decoder::new(pool, options);
        Ok(Self {
            pool: pool.clone(),
            source,
            generation,
            max_packet_bytes: video.max_packet_bytes,
            frame_clock: TimeBase::new(numerator.into(), denominator.into())?,
            extractor,
            header: Bytes::copy_from_slice(pool, decoder.header().as_bytes())?,
            decoder,
            subtitle: Subtitle::new(pool),
            delivered: None,
            pending: None,
            sequence: 0,
            end: End::Open,
        })
    }

    /// The ASS script header the cues refer to.
    pub fn header(&self) -> &Bytes {
        &self.header
    }

    pub fn provide(&mut self, unit: CaptionUnit) -> std::result::Result<(), CaptionRejection> {
        let error = if self.pending.is_some() || self.end != End::Open {
            Error::Invalid("caption unit pending or finished")
        } else if !unit.data.belongs_to(&self.pool) {
            Error::ForeignPool
        } else if unit.data.len() > self.max_packet_bytes {
            Error::Limit {
                requested: unit.data.len(),
                maximum: self.max_packet_bytes,
            }
        } else {
            self.pending = Some(Pending { unit, paid: 0 });
            return Ok(());
        };
        Err(CaptionRejection { unit, error })
    }

    /// No more packets: the waiting frames leave and the receiver drains.
    pub fn finish(&mut self) {
        if self.end == End::Open {
            self.end = End::Finishing;
        }
    }

    /// Read the provided packet and deliver what it settles. Reading charges
    /// one unit per byte, paid over as many polls as `work` needs.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<CaptionStep> {
        if cancel.load(Ordering::Relaxed) {
            self.pending = None;
            self.end = End::Drained;
            return Err(Error::Cancelled);
        }
        loop {
            if !work.try_charge(1) {
                return Ok(CaptionStep::Yield);
            }
            if let Some(cue) = self.next_line()? {
                return Ok(CaptionStep::Cue(cue));
            }
            if let Some(pending) = &mut self.pending {
                let size = pending.unit.data.len() as u64;
                pending.paid += work.take(size - pending.paid);
                if pending.paid < size {
                    return Ok(CaptionStep::Yield);
                }
                let Some(Pending { unit, .. }) = self.pending.take() else {
                    unreachable!("pending packet")
                };
                let packet = VideoPacket {
                    data: &unit.data,
                    pts: unit.pts,
                    dts: unit.dts,
                    duration: unit.duration,
                };
                self.extractor.push(packet, &mut WorkBudget::new(size))?;
                continue;
            }
            if self.decode_next()? {
                continue;
            }
            match self.end {
                End::Open => return Ok(CaptionStep::NeedUnit),
                End::Finishing => {
                    self.extractor.finish()?;
                    self.end = End::Extracted;
                }
                End::Extracted if self.decoder.drain(&mut self.subtitle)? => {
                    self.delivered = Some(0);
                }
                End::Extracted | End::Drained => {
                    self.end = End::Drained;
                    return Ok(CaptionStep::Finished);
                }
            }
        }
    }

    /// Decode the next settled frame; `false` when none is.
    fn decode_next(&mut self) -> Result<bool> {
        let Some(frame) = self.extractor.pop()? else {
            return Ok(false);
        };
        let rescale = |ticks: i64, to: u64| {
            self.frame_clock
                .rescale(ticks, TimeBase::new(1, to)?, Rounding::NearestAway)
        };
        let clock = Clock {
            pts: frame.pts.map(|pts| rescale(pts, 1_000_000)).transpose()?,
            // Kept to 32 bits as FFmpeg's display time is; 0 when unknown.
            duration_ms: frame
                .duration
                .map(|d| rescale(d, 1_000))
                .transpose()?
                .and_then(|ms| u32::try_from(ms).ok())
                .unwrap_or(0),
        };
        if self.decoder.decode(frame.data, clock, &mut self.subtitle)? {
            self.delivered = Some(0);
        }
        Ok(true)
    }

    /// The next undelivered line of the last decoded event.
    fn next_line(&mut self) -> Result<Option<TextSource>> {
        let Some(delivered) = self.delivered else {
            return Ok(None);
        };
        let Some(line) = self.subtitle.lines().nth(delivered) else {
            self.delivered = None;
            return Ok(None);
        };
        // The receiver marks an open-ended (real-time) event with `u32::MAX`.
        let display = Some(self.subtitle.end_ms).filter(|&ms| ms != u32::MAX);
        let cue = TextSource::ass_event(
            &self.pool,
            &self.header,
            line,
            receiver_timing(self.subtitle.pts, display)?,
            AssIdentity {
                source: self.source,
                generation: self.generation,
                sequence: self.sequence,
            },
        )?;
        self.sequence += 1;
        self.delivered = Some(delivered + 1);
        Ok(Some(cue))
    }
}
