//! The Matroska reader through the neutral container model.
//!
//! Packet times are nanoseconds (Matroska's TimestampScale is already applied),
//! DTS stays `None` because Matroska carries none, and random access follows
//! the Block keyframe rule. Nothing here copies a packet.
use crate::colour::{
    COLOUR, Colour,
    parse::{
        CHROMA_SITING_HORZ, CHROMA_SITING_VERT, MATRIX_COEFFICIENTS, MAX_FALL, PRIMARIES, RANGE,
        TRANSFER_CHARACTERISTICS,
    },
};
use crate::ebml::ChildCursor;
use crate::packet::batch::TypedPacket as Packet;
use crate::reader::{Event, Reader};
use crate::seek::SeekReceipt as MatroskaSeek;
use crate::step::ParseStep;
use crate::track::{LegacyAacProfile, Track, parse::VIDEO};
use crate::{Configuration, ElementEvent, metadata::SharedDocument};
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    CodecId, ColourDescription, ColourRange, ContainerReader, ContentLight, Event as Neutral,
    MasteringDisplay, MediaKind, Origin, PacketInfo, RandomAccess, ReaderStep, SeekMethod,
    SeekPolicy, SeekReceipt, SeekRequest, Sourced, TrackId, TrackInfo, Trim,
};
use triergon_core::{
    Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities, SourceFailure,
    SourceId, Step, Time, TimeBase, WorkBudget,
};

const NANOSECONDS: TimeBase = TimeBase::NANOSECONDS;

impl PacketInfo for Packet {
    #[inline]
    fn track(&self) -> TrackId {
        TrackId(self.configuration.tracks()[self.packet.track].number)
    }
    #[inline]
    fn payload(&self) -> &[u8] {
        self.packet.data.as_ref()
    }
    #[inline]
    fn time_base(&self) -> TimeBase {
        NANOSECONDS
    }
    #[inline]
    fn pts(&self) -> Option<i64> {
        self.packet.pts_ns
    }
    #[inline]
    fn dts(&self) -> Option<i64> {
        None
    }
    #[inline]
    fn duration(&self) -> Option<i64> {
        self.packet
            .duration_ns
            .and_then(|ns| i64::try_from(ns).ok())
    }
    #[inline]
    fn random_access(&self) -> RandomAccess {
        if self.packet.keyframe {
            RandomAccess::Sync
        } else {
            RandomAccess::NotSync
        }
    }
    #[inline]
    fn trim(&self) -> Trim {
        Trim {
            start: None,
            end: (self.packet.discard_padding_ns != 0).then_some(self.packet.discard_padding_ns),
        }
    }
}

impl TrackInfo for Track {
    fn id(&self) -> TrackId {
        TrackId(self.number)
    }
    fn kind(&self) -> MediaKind {
        match self.kind {
            Self::VIDEO => MediaKind::Video,
            Self::AUDIO => MediaKind::Audio,
            Self::SUBTITLE => MediaKind::Subtitle,
            Self::COMPLEX | Self::LOGO | Self::BUTTONS | Self::CONTROL => MediaKind::Data,
            _ => MediaKind::Unknown,
        }
    }
    fn codec_tag(&self) -> &[u8] {
        self.codec.as_str().as_bytes()
    }
    fn codec(&self) -> CodecId {
        codec_id(self.codec.as_str())
    }
    fn configuration(&self) -> &[u8] {
        &self.codec_private
    }
    fn language(&self) -> Option<&str> {
        Some(self.effective_language())
    }
    fn default_track(&self) -> Option<bool> {
        Some(self.default)
    }
    fn forced(&self) -> Option<bool> {
        Some(self.forced)
    }
    fn colour(&self) -> Option<ColourDescription> {
        let colour = self.colour.as_ref()?;
        Some(colour.description(colour_presence(&self.source_metadata)))
    }
}

/// The one mapping from a Matroska CodecID. The comparison is exact, so an
/// unlisted CodecID, including a longer form of a listed one, stays
/// [`CodecId::Other`].
fn codec_id(codec: &str) -> CodecId {
    match codec {
        "V_MPEG4/ISO/AVC" => CodecId::H264,
        "V_MPEGH/ISO/HEVC" => CodecId::Hevc,
        "V_AV1" => CodecId::Av1,
        "V_VP9" => CodecId::Vp9,
        "A_AAC" => CodecId::Aac,
        "A_AC3" | "A_AC3/BSID9" | "A_AC3/BSID10" => CodecId::Ac3,
        "A_EAC3" => CodecId::Eac3,
        "A_DTS" => CodecId::Dts,
        "A_VORBIS" => CodecId::Vorbis,
        "A_OPUS" => CodecId::Opus,
        "A_FLAC" => CodecId::Flac,
        "A_MPEG/L1" | "A_MPEG/L2" | "A_MPEG/L3" => CodecId::MpegAudio,
        "A_PCM/INT/LIT" => CodecId::Pcm,
        "S_TEXT/UTF8" => CodecId::Utf8Text,
        "S_TEXT/ASS" => CodecId::Ass,
        "S_TEXT/SSA" => CodecId::Ssa,
        "S_TEXT/WEBVTT" => CodecId::WebVtt,
        "S_VOBSUB" => CodecId::VobSub,
        "S_DVBSUB" => CodecId::DvbSubtitle,
        "S_HDMV/PGS" => CodecId::Pgs,
        other if LegacyAacProfile::from_codec_id(other).is_some() => CodecId::Aac,
        _ => CodecId::Other,
    }
}

#[derive(Clone, Copy, Default)]
struct ColourPresence(u16);

impl ColourPresence {
    fn includes(self, id: u64) -> bool {
        self.0 & (1 << (id - MATRIX_COEFFICIENTS)) != 0
    }

    fn origin(self, id: u64) -> Origin {
        if self.includes(id) {
            Origin::Container
        } else {
            Origin::SpecificationDefault
        }
    }
}

/// Read Colour presence from the retained TrackEntry. Each child consumes at
/// least one byte, so the retained length bounds the walk without another slot
/// on every track.
fn colour_presence(entry: &[u8]) -> ColourPresence {
    fn children(bytes: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
        let mut cursor = ChildCursor::new(0..bytes.len());
        let mut work = WorkBudget::new(bytes.len() as u64);
        let cancel = AtomicBool::new(false);
        std::iter::from_fn(move || {
            let Ok(ParseStep::Event(Some(child))) = cursor.step(bytes, &mut work, &cancel) else {
                return None;
            };
            Some((child.id, &bytes[child.payload]))
        })
    }

    let colour = children(entry)
        .find(|&(id, _)| id == VIDEO)
        .and_then(|(_, video)| children(video).find(|&(id, _)| id == COLOUR));
    let mut present = ColourPresence::default();
    if let Some((_, colour)) = colour {
        for (id, _) in children(colour) {
            if (MATRIX_COEFFICIENTS..=MAX_FALL).contains(&id) {
                present.0 |= 1 << (id - MATRIX_COEFFICIENTS);
            }
        }
    }
    present
}

impl Colour {
    fn description(&self, present: ColourPresence) -> ColourDescription {
        let range = match self.range {
            1 => ColourRange::Limited,
            2 => ColourRange::Full,
            _ => ColourRange::Unspecified,
        };
        let siting = present.includes(CHROMA_SITING_HORZ) || present.includes(CHROMA_SITING_VERT);
        let mastering = self.mastering.and_then(|m| {
            let [rx, ry, gx, gy, bx, by, wx, wy] = m.chromaticity;
            Some(MasteringDisplay {
                primaries: [[rx?, ry?], [gx?, gy?], [bx?, by?]],
                white_point: [wx?, wy?],
                luminance_max: m.luminance_max?,
                luminance_min: m.luminance_min?,
            })
        });
        ColourDescription {
            primaries: Sourced::new(self.primaries, present.origin(PRIMARIES)),
            transfer: Sourced::new(
                self.transfer_characteristics,
                present.origin(TRANSFER_CHARACTERISTICS),
            ),
            matrix: Sourced::new(
                self.matrix_coefficients,
                present.origin(MATRIX_COEFFICIENTS),
            ),
            range: Sourced::new(range, present.origin(RANGE)),
            chroma_siting: siting.then_some(self.chroma_siting),
            mastering,
            content_light: self
                .max_cll
                .zip(self.max_fall)
                .map(|(max_cll, max_fall)| ContentLight { max_cll, max_fall }),
        }
    }
}

impl MatroskaSeek {
    /// The neutral receipt; Cue and preroll detail stay on this type.
    pub fn neutral(&self) -> Result<SeekReceipt> {
        let ns = |value: i64| Time::from_ticks(i128::from(value), NANOSECONDS);
        Ok(SeekReceipt {
            requested: ns(self.requested_ns)?,
            landing: ns(self.actual_ns)?,
            discard_until: ns(self.discard_until_ns)?,
            method: if self.used_index {
                SeekMethod::Index
            } else {
                SeekMethod::BoundedScan
            },
            scanned_bytes: self.scanned_bytes,
            replays_from_start: self.replays_from_start,
        })
    }
}

/// Non-packet events in the neutral Matroska reader.
#[derive(Clone, Debug)]
pub enum ReaderEvent {
    /// Source structure or syntax.
    Envelope(ElementEvent),
    /// The track configuration published before any packet that uses it.
    Configuration(Configuration),
    /// A validated Info, Chapters, Tags or Attachments document.
    Document(SharedDocument),
}

impl ContainerReader for Reader {
    type Packet = Packet;
    type Track = Track;
    /// Configuration, documents and envelope structure.
    type Other = ReaderEvent;

    fn provide(&mut self, window: &InputWindow) -> Result<()> {
        Reader::provide(self, window)
    }
    fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        Reader::finish(self, source, token)
    }
    fn fail(&mut self, source: SourceId, token: InputToken, failure: SourceFailure) -> Result<()> {
        Reader::fail(self, source, token, failure)
    }
    fn reset(&mut self) -> Result<InputRequest> {
        Reader::reset(self)
    }
    #[inline]
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ReaderStep<Self>> {
        Ok(match Reader::step(self, work, cancel)? {
            Step::Ready(Event::Packet(packet)) => Step::Ready(Neutral::Packet(packet)),
            Step::Ready(Event::Seek(receipt)) => Step::Ready(Neutral::Seek(receipt.neutral()?)),
            Step::Ready(Event::Envelope(event)) => {
                Step::Ready(Neutral::Other(ReaderEvent::Envelope(event)))
            }
            Step::Ready(Event::Configuration(configuration)) => {
                Step::Ready(Neutral::Other(ReaderEvent::Configuration(configuration)))
            }
            Step::Ready(Event::Document(document)) => {
                Step::Ready(Neutral::Other(ReaderEvent::Document(document)))
            }
            Step::NeedInput(request) => Step::NeedInput(request),
            Step::Yield => Step::Yield,
            Step::Backpressure(pressure) => Step::Backpressure(pressure),
            Step::End(location) => Step::End(location),
        })
    }
    fn request_seek(&mut self, request: SeekRequest) -> Result<InputRequest> {
        let track = self
            .tracks()
            .iter()
            .position(|track| track.number == request.track.0)
            .ok_or(Error::Invalid("seek track is not in the Segment"))?;
        let requested_ns = request
            .target
            .to_nanoseconds(triergon_core::Rounding::Floor)?;
        match request.policy {
            SeekPolicy::PreviousRandomAccess => self.seek_previous(track, requested_ns),
            SeekPolicy::PlaybackPreroll => self.seek_for_playback(track, requested_ns),
        }
    }
    fn track_count(&self) -> usize {
        Reader::tracks(self).len()
    }
    fn track(&self, index: usize) -> Option<&Track> {
        Reader::tracks(self).get(index)
    }
    fn capabilities(&self) -> SourceCapabilities {
        Reader::capabilities(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recognised_codec_id_has_one_name() {
        for (tag, id) in [
            ("V_MPEG4/ISO/AVC", CodecId::H264),
            ("V_MPEGH/ISO/HEVC", CodecId::Hevc),
            ("V_AV1", CodecId::Av1),
            ("V_VP9", CodecId::Vp9),
            ("A_AAC", CodecId::Aac),
            ("A_AAC/MPEG2/MAIN", CodecId::Aac),
            ("A_AAC/MPEG2/LC", CodecId::Aac),
            ("A_AAC/MPEG2/SSR", CodecId::Aac),
            ("A_AAC/MPEG2/LC/SBR", CodecId::Aac),
            ("A_AAC/MPEG4/LC", CodecId::Aac),
            ("A_AAC/MPEG4/MAIN", CodecId::Aac),
            ("A_AAC/MPEG4/SSR", CodecId::Aac),
            ("A_AAC/MPEG4/LTP", CodecId::Aac),
            ("A_AAC/MPEG4/LC/SBR", CodecId::Aac),
            ("A_AC3", CodecId::Ac3),
            ("A_AC3/BSID9", CodecId::Ac3),
            ("A_AC3/BSID10", CodecId::Ac3),
            ("A_EAC3", CodecId::Eac3),
            ("A_DTS", CodecId::Dts),
            ("A_VORBIS", CodecId::Vorbis),
            ("A_OPUS", CodecId::Opus),
            ("A_FLAC", CodecId::Flac),
            ("A_MPEG/L1", CodecId::MpegAudio),
            ("A_MPEG/L2", CodecId::MpegAudio),
            ("A_MPEG/L3", CodecId::MpegAudio),
            ("A_PCM/INT/LIT", CodecId::Pcm),
            ("S_TEXT/UTF8", CodecId::Utf8Text),
            ("S_TEXT/ASS", CodecId::Ass),
            ("S_TEXT/SSA", CodecId::Ssa),
            ("S_TEXT/WEBVTT", CodecId::WebVtt),
            ("S_VOBSUB", CodecId::VobSub),
            ("S_DVBSUB", CodecId::DvbSubtitle),
            ("S_HDMV/PGS", CodecId::Pgs),
        ] {
            assert_eq!(codec_id(tag), id, "{tag}");
        }
    }

    #[test]
    fn an_unrecognised_codec_id_is_reported_with_its_own_bytes() {
        for tag in [
            "",
            "V_VP8",
            "A_FLAC/",
            "A_MPEG/L4",
            "A_MPEG",
            "A_DTS/LOSSLESS",
            "A_AAC/MPEG4/LC/UNKNOWN",
            "A_PCM/INT/BIG",
            "a_aac",
            "V_AUTHORED/OPAQUE",
        ] {
            assert_eq!(codec_id(tag), CodecId::Other);
        }
    }
}
