//! Borrowed track metadata from the PMT. Payload admission stays separate.
use super::ElementaryStream;
use triergon_core::container::{CodecId, MediaKind, TrackId, TrackInfo};

impl TrackInfo for ElementaryStream {
    fn id(&self) -> TrackId {
        TrackId(u64::from(self.pid))
    }

    fn kind(&self) -> MediaKind {
        match ElementaryStream::codec(self) {
            Some(CodecId::Mpeg2Video | CodecId::H264 | CodecId::Hevc) => MediaKind::Video,
            Some(
                CodecId::MpegAudio | CodecId::Aac | CodecId::AacLatm | CodecId::Ac3 | CodecId::Eac3,
            ) => MediaKind::Audio,
            Some(CodecId::DvbSubtitle | CodecId::DvbTeletext) => MediaKind::Subtitle,
            _ => MediaKind::Unknown,
        }
    }

    fn codec_tag(&self) -> &[u8] {
        std::slice::from_ref(&self.stream_type)
    }

    fn codec(&self) -> CodecId<'_> {
        ElementaryStream::codec(self).unwrap_or_else(|| CodecId::Other(self.codec_tag()))
    }

    fn configuration(&self) -> &[u8] {
        // PMT descriptors remain source signalling, not decoder configuration.
        &[]
    }
}
