//! The codec a PMT entry names: H.222.0 Table 2-34 stream types and, for PES
//! private data, the EN 300 468 descriptors that name a codec.
use super::{DescriptorNamespace, ElementaryStream, descriptor_tag, stream_type};
use triergon_core::container::CodecId;

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
    pub fn codec(&self) -> Option<CodecId> {
        match self.stream_type {
            stream_type::MPEG1_VIDEO | stream_type::MPEG2_VIDEO => Some(CodecId::Mpeg2Video),
            stream_type::MPEG1_AUDIO | stream_type::MPEG2_AUDIO => Some(CodecId::MpegAudio),
            stream_type::AAC_ADTS => Some(CodecId::Aac),
            stream_type::AAC_LATM => Some(CodecId::AacLatm),
            stream_type::H264 => Some(CodecId::H264),
            stream_type::HEVC => Some(CodecId::Hevc),
            stream_type::ATSC_AC3 if self.namespace == DescriptorNamespace::Atsc => {
                Some(CodecId::Ac3)
            }
            stream_type::ATSC_EAC3 if self.namespace == DescriptorNamespace::Atsc => {
                Some(CodecId::Eac3)
            }
            stream_type::PES_PRIVATE_DATA
                if matches!(self.namespace, DescriptorNamespace::Dvb(_)) =>
            {
                self.dvb_descriptor_codec().or_else(|| {
                    let hd = self.dvb_dts_hd().ok().flatten()?;
                    hd.extensions
                        .iter()
                        .any(Option::is_some)
                        .then_some(CodecId::Dts)
                })
            }
            _ => None,
        }
    }

    /// The codec named by the first EN 300 468 descriptor in this entry's loop
    /// that names one: teletext (0x56), subtitling (0x59), AC-3 (0x6a) or
    /// E-AC-3 (0x7a) or DTS core (0x7b), whatever the namespace and stream type. Outside DVB these
    /// tags are user private; the loop is read up to its first malformed
    /// descriptor.
    pub fn dvb_descriptor_codec(&self) -> Option<CodecId> {
        self.descriptors()
            .map_while(Result::ok)
            .find_map(|descriptor| match descriptor.tag {
                descriptor_tag::DVB_TELETEXT => Some(CodecId::DvbTeletext),
                descriptor_tag::DVB_SUBTITLING => Some(CodecId::DvbSubtitle),
                descriptor_tag::DVB_AC3 => Some(CodecId::Ac3),
                descriptor_tag::DVB_EAC3 => Some(CodecId::Eac3),
                descriptor_tag::DVB_DTS if descriptor.data.len() >= 5 => Some(CodecId::Dts),
                _ => None,
            })
    }
}
