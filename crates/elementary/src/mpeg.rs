//! MPEG-1, MPEG-2 and MPEG-2.5 audio, Layer I, II and III (ISO/IEC 11172-3, 13818-3).
//!
//! Frames follow the header length; a free-format stream (bit rate index 0) runs each
//! frame to the next header. A first Layer III frame carrying a Xing, Info or VBRI
//! description is not audio and is not delivered. Bytes between frames go into the
//! packet of the frame after them, and bytes after the last frame form a last,
//! incomplete packet, as FFmpeg's demuxer delivers them.
//!
//! # Gapless trim (`compat::mpeg`)
//!
//! A LAME extension written by LAME or by FFmpeg (encoder string starting `LAME`,
//! `Lavf` or `Lavc`) trims the stream as FFmpeg 9.0 does: the first packet drops
//! `encoder delay + 529` samples (the decoder delay of 528 plus one), and the packet
//! with frame index `frames - 1` drops `encoder padding - 529` samples when that is
//! positive. The end trim is withheld, as FFmpeg withholds it, when the source holds
//! more than a sixteenth more bytes after the information frame's header than the
//! frame states.
use crate::stream::Limits;
use crate::syntax::{Gap, Length, Plan, Probe, SeekBasis, SeekContext, Syntax, TrackFacts};
use triergon_core::container::{CodecId, Trim};
use triergon_core::{Budget, BudgetVec, Result};
use triergon_framing::{
    LameTag, MpegAudioHeader, MpegInfoFrame, MpegInfoKind, MpegLayer, mpeg_info_frame,
};

/// Samples the reference decoder outputs before the first encoded sample, plus the one
/// FFmpeg adds.
const DECODER_DELAY: u32 = 528 + 1;

/// The information frame a stream starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InfoFrame {
    pub frame: MpegInfoFrame,
    /// Source offset of the frame.
    pub offset: u64,
    /// Bytes the frame occupies.
    pub len: u64,
    /// The source length agrees with the byte count the frame states (or either is
    /// unknown).
    pub consistent: bool,
}

/// Samples the stream's LAME tag removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gapless {
    /// Samples dropped from the first packet.
    pub start: u32,
    /// The packet (frame index from the first audio frame) and samples dropped from its
    /// end.
    pub end: Option<(u64, u32)>,
}

/// MPEG audio elementary stream facts.
#[derive(Debug)]
pub struct MpegAudio {
    first: Option<MpegAudioHeader>,
    info: Option<InfoFrame>,
    gapless: Option<Gapless>,
}

impl MpegAudio {
    /// The first frame's header.
    pub fn header(&self) -> Option<&MpegAudioHeader> {
        self.first.as_ref()
    }
    /// The Xing, Info or VBRI frame the stream starts with.
    pub fn info_frame(&self) -> Option<&InfoFrame> {
        self.info.as_ref()
    }
    /// The LAME tag of the information frame.
    pub fn lame(&self) -> Option<LameTag> {
        self.info.and_then(|info| info.frame.lame)
    }
    /// The trim the stream's LAME tag implies.
    pub fn gapless(&self) -> Option<Gapless> {
        self.gapless
    }

    fn samples_per_frame(&self) -> u32 {
        self.first.map_or(1152, |h| h.samples_per_frame())
    }

    fn audio_frames(&self) -> Option<u64> {
        self.info.and_then(|info| info.frame.frames).map(u64::from)
    }

    /// Average bytes per audio frame: the information frame's counts, else the first
    /// header's bit rate.
    fn frame_bytes(&self, context: SeekContext) -> Option<f64> {
        if let (Some(frames), Some(info)) = (self.audio_frames(), self.info) {
            let bytes = info
                .frame
                .bytes
                .map(|b| u64::from(b).saturating_sub(info.len))
                .or_else(|| {
                    context
                        .audio_end
                        .map(|end| end.saturating_sub(context.audio_start))
                })?;
            return (frames > 0).then(|| bytes as f64 / frames as f64);
        }
        let header = self.first?;
        (header.bitrate_kbps > 0).then(|| {
            f64::from(header.samples_per_frame()) / 8.0 * f64::from(header.bitrate_kbps) * 1000.0
                / f64::from(header.sample_rate)
        })
    }
}

impl Syntax for MpegAudio {
    type Header = MpegAudioHeader;
    const PROBE: usize = 4;
    const SYNC: u8 = 0xff;
    const GAP: Gap = Gap::PrependToNext;
    const CONFIRM: u8 = 2;
    /// Layer III frames may take main data from the frames before them.
    const PREROLL: u32 = 2;

    fn new(_budget: &Budget, _limits: &Limits) -> MpegAudio {
        MpegAudio {
            first: None,
            info: None,
            gapless: None,
        }
    }

    fn probe(&self, bytes: &[u8]) -> Probe<MpegAudioHeader> {
        let Some(word) = bytes.get(..4) else {
            return if bytes.first().is_none_or(|&b| b == 0xff) {
                Probe::Short
            } else {
                Probe::Invalid
            };
        };
        let word = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        // FFmpeg accepts the reserved emphasis value 2 (conformance stream hecommon).
        let emphasis = (word & 3) as u8;
        match MpegAudioHeader::parse(word & !3) {
            Some(header) => Probe::Header(MpegAudioHeader { emphasis, ..header }),
            None => Probe::Invalid,
        }
    }

    fn length(&self, header: &MpegAudioHeader) -> Length {
        match header.frame_bytes() {
            Some(len) => Length::Known(len),
            None => Length::Search {
                minimum: 4 + usize::from(header.crc) * 2,
            },
        }
    }

    /// Version, layer and sample rate stay fixed, and a free-format stream stays free.
    fn continues(&self, reference: &MpegAudioHeader, next: &MpegAudioHeader) -> bool {
        reference.same_stream(next) && (reference.bitrate_kbps == 0) == (next.bitrate_kbps == 0)
    }

    fn samples(&self, header: &MpegAudioHeader) -> u32 {
        header.samples_per_frame()
    }

    fn adopt(&mut self, header: &MpegAudioHeader, budget: &Budget) -> Result<TrackFacts> {
        self.first = Some(*header);
        Ok(TrackFacts {
            codec_tag: match header.layer {
                MpegLayer::I => b"mp1",
                MpegLayer::II => b"mp2",
                MpegLayer::III => b"mp3",
            },
            codec: CodecId::MpegAudio,
            sample_rate: header.sample_rate,
            channels: Some(header.channels()),
            configuration: BudgetVec::new(budget),
            duration: None,
        })
    }

    fn inspect_first(&self, header: &MpegAudioHeader) -> Option<usize> {
        (header.layer == MpegLayer::III)
            .then(|| header.frame_bytes())
            .flatten()
    }

    fn describes(
        &mut self,
        frame: &[u8],
        header: &MpegAudioHeader,
        offset: u64,
        source_len: Option<u64>,
    ) -> bool {
        let Some(description) = mpeg_info_frame(frame, header) else {
            return false;
        };
        let consistent = match (description.bytes, source_len) {
            (Some(stated), Some(len)) => {
                let stated = u64::from(stated);
                len.saturating_sub(offset + 4) <= stated + stated / 16
            }
            _ => true,
        };
        self.info = Some(InfoFrame {
            frame: description,
            offset,
            len: frame.len() as u64,
            consistent,
        });
        let encoder = description.lame.filter(|tag| {
            [b"LAME", b"Lavf", b"Lavc"]
                .iter()
                .any(|n| tag.encoder.starts_with(*n))
        });
        if description.kind != MpegInfoKind::Vbri
            && let Some(tag) = encoder
        {
            let padding = u32::from(tag.encoder_padding).saturating_sub(DECODER_DELAY);
            let end = description
                .frames
                .filter(|&frames| consistent && padding > 0 && frames > 0)
                .map(|frames| (u64::from(frames) - 1, padding));
            self.gapless = Some(Gapless {
                start: u32::from(tag.encoder_delay) + DECODER_DELAY,
                end,
            });
        }
        true
    }

    fn duration(&self) -> Option<u64> {
        let info = self.info.filter(|info| info.consistent)?;
        let total = u64::from(info.frame.frames?) * u64::from(self.samples_per_frame());
        let trim = info.frame.lame.map_or(0, |tag| {
            u64::from(tag.encoder_delay) + u64::from(tag.encoder_padding)
        });
        Some(total.saturating_sub(if self.gapless.is_some() { trim } else { 0 }))
    }

    fn trim(&self, pts: i64, _header: &MpegAudioHeader) -> Trim {
        let Some(gapless) = self.gapless else {
            return Trim::default();
        };
        let index = pts.max(0) as u64 / u64::from(self.samples_per_frame());
        Trim {
            start: (pts == 0).then_some(i64::from(gapless.start)),
            end: gapless
                .end
                .filter(|&(last, _)| last == index)
                .map(|(_, padding)| i64::from(padding)),
        }
    }

    fn plan_seek(&self, target: u64, context: SeekContext) -> Plan {
        let spf = u64::from(self.samples_per_frame());
        if let (Some(info), Some(frames)) = (self.info, self.audio_frames())
            && let (Some(toc), Some(bytes)) = (info.frame.toc, info.frame.bytes)
            && frames > 0
        {
            let total = frames * spf;
            let percent = (target.min(total) * 100 / total).min(99) as usize;
            let offset = info.offset + u64::from(toc[percent]) * u64::from(bytes) / 256;
            return Plan::Estimate {
                offset,
                basis: SeekBasis::Toc,
            };
        }
        match self.frame_bytes(context) {
            Some(frame_bytes) => Plan::Estimate {
                offset: context.audio_start + ((target / spf) as f64 * frame_bytes) as u64,
                basis: SeekBasis::Bitrate,
            },
            None => Plan::Scan,
        }
    }

    fn estimate(&self, offset: u64, context: SeekContext, basis: SeekBasis) -> u64 {
        let spf = u64::from(self.samples_per_frame());
        let into = offset.saturating_sub(context.audio_start);
        let frame = match (basis, self.info, self.audio_frames()) {
            (SeekBasis::Toc, Some(info), Some(frames)) => {
                let (Some(toc), Some(bytes)) = (info.frame.toc, info.frame.bytes) else {
                    return 0;
                };
                // Invert the table by linear interpolation between its points.
                let position = (offset.saturating_sub(info.offset) as f64 * 256.0
                    / f64::from(bytes.max(1)))
                .min(256.0);
                let mut percent = 0.0;
                for i in 0..100 {
                    let low = f64::from(toc[i]);
                    let high = if i == 99 {
                        256.0
                    } else {
                        f64::from(toc[i + 1])
                    };
                    if position < high || i == 99 {
                        let span = (high - low).max(1.0);
                        percent = i as f64 + ((position - low) / span).clamp(0.0, 1.0);
                        break;
                    }
                }
                (percent / 100.0 * frames as f64) as u64
            }
            _ => self
                .frame_bytes(context)
                .map_or(0, |bytes| (into as f64 / bytes).round() as u64),
        };
        frame * spf
    }

    fn reset(&mut self) {
        *self = MpegAudio {
            first: None,
            info: None,
            gapless: None,
        };
    }
}

impl crate::Format for MpegAudio {}
