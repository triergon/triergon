//! The codec a PMT entry names: H.222.0 Table 2-34 stream types and, for PES
//! private data, the EN 300 468 descriptors that name a codec.
use super::{DescriptorNamespace, ElementaryStream};
use triergon_core::container::CodecId;

/// `stream_type` 0x06: PES packets containing private data.
pub(crate) const PES_PRIVATE_DATA: u8 = 0x06;

impl ElementaryStream {
    /// The codec this entry names, or `None` when it names none [`CodecId`] has.
    ///
    /// H.222.0 Table 2-34 assigns 0x01 and 0x02 to MPEG video, 0x03 and 0x04
    /// to MPEG audio, 0x0f to AAC with ADTS, 0x11 to AAC with LATM, 0x1b to
    /// AVC and 0x24 to HEVC. PES private data
    /// (0x06) names a codec only through a DVB descriptor, so only under a DVB
    /// [`DescriptorNamespace`] ([`Self::dvb_descriptor_codec`]).
    /// ATSC A/53 Part 3 §5.7 assigns 0x81 and 0x87 to AC-3 and E-AC-3
    /// in an explicitly selected ATSC namespace.
    pub fn codec(&self) -> Option<CodecId<'static>> {
        match self.stream_type {
            0x01 | 0x02 => Some(CodecId::Mpeg2Video),
            0x03 | 0x04 => Some(CodecId::MpegAudio),
            0x0f => Some(CodecId::Aac),
            0x11 => Some(CodecId::AacLatm),
            0x1b => Some(CodecId::H264),
            0x24 => Some(CodecId::Hevc),
            0x81 if self.namespace == DescriptorNamespace::Atsc => Some(CodecId::Ac3),
            0x87 if self.namespace == DescriptorNamespace::Atsc => Some(CodecId::Eac3),
            PES_PRIVATE_DATA if matches!(self.namespace, DescriptorNamespace::Dvb(_)) => {
                self.dvb_descriptor_codec()
            }
            _ => None,
        }
    }

    /// The codec named by the first EN 300 468 descriptor in this entry's loop
    /// that names one: teletext (0x56), subtitling (0x59), AC-3 (0x6a) or
    /// E-AC-3 (0x7a), whatever the namespace and stream type. Outside DVB these
    /// tags are user private; the loop is read up to its first malformed
    /// descriptor.
    pub fn dvb_descriptor_codec(&self) -> Option<CodecId<'static>> {
        self.descriptors()
            .map_while(Result::ok)
            .find_map(|descriptor| match descriptor.tag {
                0x56 => Some(CodecId::DvbTeletext),
                0x59 => Some(CodecId::DvbSubtitle),
                0x6a => Some(CodecId::Ac3),
                0x7a => Some(CodecId::Eac3),
                _ => None,
            })
    }
}
