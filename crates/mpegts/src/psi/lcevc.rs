//! Source descriptor views from H.222.0 (08/2023) §§2.6.90–91, 2.6.137–140.
use super::Descriptor;
use triergon_core::{Error, Result};

/// Encoded LCEVC video parameters (Table 2-142), without codec conformance checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LcevcVideo {
    pub lcevc_stream_tag: u8,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub sublevel_idc: u8,
    pub processed_planes_type_flag: bool,
    pub picture_type_bit_flag: bool,
    pub field_type_bit_flag: bool,
    pub hdr_wcg_idc: u8,
    pub video_properties_tag: u8,
}

/// Ordered association tags (Table 2-143), borrowed from the source descriptor.
/// Matching tags do not by themselves establish a usable base/enhancement pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LcevcLinkage<'a> {
    pub lcevc_stream_tags: &'a [u8],
}

impl<'a> Descriptor<'a> {
    fn lcevc_body(self, extension: u8) -> Result<&'a [u8]> {
        if self.tag != 0x3f || self.data.first() != Some(&extension) {
            return Err(Error::Invalid("LCEVC extension descriptor tag"));
        }
        if self.data.len() > 255 {
            return Err(Error::Invalid("LCEVC extension descriptor length"));
        }
        Ok(&self.data[1..])
    }

    /// Read extension 0x17 as four bytes of encoded fields with checked reserved bits. Profile, HDR and picture semantics remain codec facts.
    /// This opt-in view does not change PMT admission or selected syntax validation.
    pub fn lcevc_video(self) -> Result<LcevcVideo> {
        let data = self.lcevc_body(0x17)?;
        if data.len() != 4 {
            return Err(Error::Invalid("LCEVC video descriptor length"));
        }
        if data[2] & 7 != 7 || data[3] & 0x30 != 0 {
            return Err(Error::Invalid("LCEVC video descriptor reserved bits"));
        }
        Ok(LcevcVideo {
            lcevc_stream_tag: data[0],
            profile_idc: data[1] >> 4,
            level_idc: data[1] & 15,
            sublevel_idc: data[2] >> 6,
            processed_planes_type_flag: data[2] & 32 != 0,
            picture_type_bit_flag: data[2] & 16 != 0,
            field_type_bit_flag: data[2] & 8 != 0,
            hdr_wcg_idc: data[3] >> 6,
            video_properties_tag: data[3] & 15,
        })
    }

    /// Read extension 0x18, checking the eight-bit count against the exact body.
    /// Zero tags and repeated tags are retained; no association is inferred.
    pub fn lcevc_linkage(self) -> Result<LcevcLinkage<'a>> {
        let data = self.lcevc_body(0x18)?;
        let count = *data.first().ok_or(Error::Invalid("LCEVC linkage count"))?;
        if data.len() != 1 + usize::from(count) {
            return Err(Error::Invalid("LCEVC linkage descriptor length"));
        }
        Ok(LcevcLinkage {
            lcevc_stream_tags: &data[1..],
        })
    }
}
