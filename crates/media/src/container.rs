//! The Matroska reader through the neutral container model.
//!
//! Packet times are nanoseconds (Matroska's TimestampScale is already applied),
//! DTS stays `None` because Matroska carries none, and random access follows
//! the Block keyframe rule. Nothing here copies a packet.
use crate::colour::Colour;
use crate::matroska::{SeekReceipt as MatroskaSeek, Track};
use crate::typed::{Event, Packet, Reader};
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
        self.packet.dts_ns
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
            1 => MediaKind::Video,
            2 => MediaKind::Audio,
            17 => MediaKind::Subtitle,
            3 | 16 | 18 | 32 => MediaKind::Data,
            _ => MediaKind::Unknown,
        }
    }
    fn codec_tag(&self) -> &[u8] {
        self.codec.as_str().as_bytes()
    }
    fn codec(&self) -> CodecId<'_> {
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
fn codec_id(codec: &str) -> CodecId<'_> {
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
        "A_PCM/INT/LIT" => CodecId::Pcm,
        "S_TEXT/UTF8" => CodecId::Utf8Text,
        "S_TEXT/ASS" => CodecId::Ass,
        "S_TEXT/SSA" => CodecId::Ssa,
        "S_TEXT/WEBVTT" => CodecId::WebVtt,
        "S_VOBSUB" => CodecId::VobSub,
        "S_DVBSUB" => CodecId::DvbSubtitle,
        "S_HDMV/PGS" => CodecId::Pgs,
        other => CodecId::Other(other.as_bytes()),
    }
}

/// Which Colour children (bit `id - 0x55B1`) the retained TrackEntry payload
/// carries. Read on demand from the source bytes the track already keeps, so
/// packet-path tracks carry no extra field. A malformed tail ends the walk.
fn colour_presence(entry: &[u8]) -> u16 {
    fn child(data: &[u8], id: u64) -> Option<&[u8]> {
        let mut rest = data;
        while !rest.is_empty() {
            let (element, id_width, _) = crate::ebml::vint(rest, true).ok()?;
            let (size, size_width, unknown) = crate::ebml::vint(&rest[id_width..], false).ok()?;
            let start = id_width + size_width;
            let end = start.checked_add(usize::try_from(size).ok()?)?;
            if unknown || end > rest.len() {
                return None;
            }
            if element == id {
                return Some(&rest[start..end]);
            }
            rest = &rest[end..];
        }
        None
    }
    let Some(colour) = child(entry, 0xe0).and_then(|video| child(video, 0x55b0)) else {
        return 0;
    };
    (0x55b1..=0x55bd).fold(0, |present, id| {
        present | (u16::from(child(colour, id).is_some()) << (id - 0x55b1))
    })
}

impl Colour {
    /// The neutral H.273 description. `present` has bit `id - 0x55B1` set for
    /// each Colour child the source carried; omitted fields carry the Matroska
    /// schema default with [`Origin::SpecificationDefault`].
    pub fn description(&self, present: u16) -> ColourDescription {
        let origin = |id: u16| {
            if present & (1 << (id - 0x55b1)) != 0 {
                Origin::Container
            } else {
                Origin::SpecificationDefault
            }
        };
        let range = match self.range {
            1 => ColourRange::Limited,
            2 => ColourRange::Full,
            _ => ColourRange::Unspecified,
        };
        let siting = present & (0b11 << (0x55b7 - 0x55b1)) != 0;
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
            primaries: Sourced::new(self.primaries, origin(0x55bb)),
            transfer: Sourced::new(self.transfer_characteristics, origin(0x55ba)),
            matrix: Sourced::new(self.matrix_coefficients, origin(0x55b1)),
            range: Sourced::new(range, origin(0x55b9)),
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

impl ContainerReader for Reader {
    type Packet = Packet;
    type Track = Track;
    /// Configuration, documents and envelope structure.
    type Other = Event;

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
            Step::Ready(other) => Step::Ready(Neutral::Other(other)),
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
            ("A_AC3", CodecId::Ac3),
            ("A_AC3/BSID9", CodecId::Ac3),
            ("A_AC3/BSID10", CodecId::Ac3),
            ("A_EAC3", CodecId::Eac3),
            ("A_DTS", CodecId::Dts),
            ("A_VORBIS", CodecId::Vorbis),
            ("A_OPUS", CodecId::Opus),
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
            "A_FLAC",
            "A_DTS/LOSSLESS",
            "A_AAC/MPEG4/LC",
            "A_PCM/INT/BIG",
            "a_aac",
            "V_AUTHORED/OPAQUE",
        ] {
            assert_eq!(codec_id(tag), CodecId::Other(tag.as_bytes()));
        }
    }
}
