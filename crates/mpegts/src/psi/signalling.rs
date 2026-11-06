//! Original bounded views from H.222.0 2.6 and EN 300 468 D.3/D.5.
//! Each parser reads at most one 255-byte descriptor; dynamic tails stay borrowed
//! from its charged source. Codec payload/profile conformance belongs downstream.
use super::Descriptor;
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registration<'a> {
    pub format_identifier: [u8; 4],
    pub additional_identification_info: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageEntry {
    /// Original ISO 639/ISO 8859-1 bytes, without UTF-8 conversion or lookup.
    pub language_code: [u8; 3],
    /// Includes user-private and reserved values; no role is silently inferred.
    pub audio_type: u8,
}

#[derive(Clone, Debug)]
pub struct LanguageEntries<'a> {
    remaining: &'a [u8],
}
impl Iterator for LanguageEntries<'_> {
    type Item = LanguageEntry;
    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.remaining.get(..4)?;
        self.remaining = &self.remaining[4..];
        Some(LanguageEntry {
            language_code: [entry[0], entry[1], entry[2]],
            audio_type: entry[3],
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.remaining.len() / 4;
        (count, Some(count))
    }
}
impl ExactSizeIterator for LanguageEntries<'_> {}
impl std::iter::FusedIterator for LanguageEntries<'_> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcVideo {
    pub profile_idc: u8,
    /// Constraint set flags 0 through 5, in source order.
    pub constraint_set_flags: [bool; 6],
    pub compatible_flags: u8,
    pub level_idc: u8,
    pub still_present: bool,
    pub picture_24h: bool,
    pub frame_packing_sei_not_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcVideo {
    pub profile_space: u8,
    pub high_tier: bool,
    pub profile_idc: u8,
    pub profile_compatibility_indication: u32,
    pub progressive_source: bool,
    pub interlaced_source: bool,
    pub non_packed_constraint: bool,
    pub frame_only_constraint: bool,
    /// Copied profile-tier-level source bits; these are not reserved-zero bits.
    pub copied_44bits: u64,
    pub level_idc: u8,
    pub still_present: bool,
    pub picture_24h: bool,
    pub sub_pic_hrd_params_not_present: bool,
    /// Raw two-bit value. Descriptor signalling alone does not validate HDR video.
    pub hdr_wcg_idc: u8,
    pub temporal_ids: Option<(u8, u8)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbAc3<'a> {
    pub component_type: Option<u8>,
    pub bsid: Option<u8>,
    pub mainid: Option<u8>,
    pub asvc: Option<u8>,
    /// DVB recommends zero; retain the source bits rather than repair them.
    pub reserved_flags: u8,
    pub additional_info: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbEac3<'a> {
    pub component_type: Option<u8>,
    pub bsid: Option<u8>,
    pub mainid: Option<u8>,
    pub asvc: Option<u8>,
    pub mix_info_exists: bool,
    pub substreams: [Option<u8>; 3],
    pub additional_info: &'a [u8],
}

fn audio_field(data: &[u8], position: &mut usize, present: bool) -> Result<Option<u8>> {
    if !present {
        return Ok(None);
    }
    let value = *data
        .get(*position)
        .ok_or(Error::Invalid("audio descriptor optional fields"))?;
    *position += 1;
    Ok(Some(value))
}

impl<'a> Descriptor<'a> {
    pub fn registration(self) -> Result<Registration<'a>> {
        if self.tag != 5 {
            return Err(Error::Invalid("registration descriptor tag"));
        }
        if self.data.len() < 4 {
            return Err(Error::Invalid("registration descriptor length"));
        }
        Ok(Registration {
            format_identifier: [self.data[0], self.data[1], self.data[2], self.data[3]],
            additional_identification_info: &self.data[4..],
        })
    }
    pub fn languages(self) -> Result<LanguageEntries<'a>> {
        if self.tag != 10 {
            return Err(Error::Invalid("language descriptor tag"));
        }
        if !self.data.len().is_multiple_of(4) {
            return Err(Error::Invalid("language descriptor length"));
        }
        Ok(LanguageEntries {
            remaining: self.data,
        })
    }
    /// H.222.0 Table 2-92 syntax; matching the signalled SPS is M02 work.
    pub fn avc_video(self) -> Result<AvcVideo> {
        if self.tag != 0x28 {
            return Err(Error::Invalid("AVC descriptor tag"));
        }
        let data = self.data;
        if data.len() != 4 {
            return Err(Error::Invalid("AVC descriptor length"));
        }
        if data[3] & 0x1f != 0x1f {
            return Err(Error::Invalid("AVC descriptor reserved bits"));
        }
        Ok(AvcVideo {
            profile_idc: data[0],
            constraint_set_flags: [
                data[1] & 128 != 0,
                data[1] & 64 != 0,
                data[1] & 32 != 0,
                data[1] & 16 != 0,
                data[1] & 8 != 0,
                data[1] & 4 != 0,
            ],
            compatible_flags: data[1] & 3,
            level_idc: data[2],
            still_present: data[3] & 128 != 0,
            picture_24h: data[3] & 64 != 0,
            frame_packing_sei_not_present: data[3] & 32 != 0,
        })
    }
    /// H.222.0 Table 2-113. Association to a permitted stream type is checked
    /// by PmtReader; profile/HDR and actual temporal-layer conformance need M02.
    pub fn hevc_video(self) -> Result<HevcVideo> {
        if self.tag != 0x38 {
            return Err(Error::Invalid("HEVC descriptor tag"));
        }
        let data = self.data;
        let flags = *data
            .get(12)
            .ok_or(Error::Invalid("HEVC descriptor length"))?;
        let subset = flags & 128 != 0;
        if data.len() != if subset { 15 } else { 13 } {
            return Err(Error::Invalid("HEVC descriptor length"));
        }
        if flags & 0x0c != 0x0c {
            return Err(Error::Invalid("HEVC descriptor reserved bits"));
        }
        let temporal_ids = if subset {
            if data[13] & 31 != 31 || data[14] & 31 != 31 {
                return Err(Error::Invalid("HEVC temporal reserved bits"));
            }
            let bounds = (data[13] >> 5, data[14] >> 5);
            if bounds.0 > bounds.1 {
                return Err(Error::Invalid("HEVC temporal bounds"));
            }
            Some(bounds)
        } else {
            None
        };
        Ok(HevcVideo {
            profile_space: data[0] >> 6,
            high_tier: data[0] & 32 != 0,
            profile_idc: data[0] & 31,
            profile_compatibility_indication: u32::from_be_bytes([
                data[1], data[2], data[3], data[4],
            ]),
            progressive_source: data[5] & 128 != 0,
            interlaced_source: data[5] & 64 != 0,
            non_packed_constraint: data[5] & 32 != 0,
            frame_only_constraint: data[5] & 16 != 0,
            copied_44bits: u64::from_be_bytes([
                0,
                0,
                data[5] & 15,
                data[6],
                data[7],
                data[8],
                data[9],
                data[10],
            ]),
            level_idc: data[11],
            still_present: flags & 64 != 0,
            picture_24h: flags & 32 != 0,
            sub_pic_hrd_params_not_present: flags & 16 != 0,
            hdr_wcg_idc: flags & 3,
            temporal_ids,
        })
    }
    /// Invoke only after the caller establishes DVB descriptor context. This
    /// does not identify an arbitrary private tag as AC-3 or strip PES bytes.
    pub fn dvb_ac3(self) -> Result<DvbAc3<'a>> {
        if self.tag != 0x6a {
            return Err(Error::Invalid("DVB AC-3 descriptor tag"));
        }
        let flags = *self
            .data
            .first()
            .ok_or(Error::Invalid("audio descriptor length"))?;
        let mut position = 1;
        let component_type = audio_field(self.data, &mut position, flags & 128 != 0)?;
        let bsid = audio_field(self.data, &mut position, flags & 64 != 0)?;
        let mainid = audio_field(self.data, &mut position, flags & 32 != 0)?;
        let asvc = audio_field(self.data, &mut position, flags & 16 != 0)?;
        Ok(DvbAc3 {
            component_type,
            bsid,
            mainid,
            asvc,
            reserved_flags: flags & 15,
            additional_info: &self.data[position..],
        })
    }
    /// DVB EN 300 468 D.5; mix_info_exists is a flag, not an extra body byte.
    pub fn dvb_eac3(self) -> Result<DvbEac3<'a>> {
        if self.tag != 0x7a {
            return Err(Error::Invalid("DVB E-AC-3 descriptor tag"));
        }
        let flags = *self
            .data
            .first()
            .ok_or(Error::Invalid("audio descriptor length"))?;
        let mut position = 1;
        let component_type = audio_field(self.data, &mut position, flags & 128 != 0)?;
        let bsid = audio_field(self.data, &mut position, flags & 64 != 0)?;
        let mainid = audio_field(self.data, &mut position, flags & 32 != 0)?;
        let asvc = audio_field(self.data, &mut position, flags & 16 != 0)?;
        let substreams = [
            audio_field(self.data, &mut position, flags & 4 != 0)?,
            audio_field(self.data, &mut position, flags & 2 != 0)?,
            audio_field(self.data, &mut position, flags & 1 != 0)?,
        ];
        Ok(DvbEac3 {
            component_type,
            bsid,
            mainid,
            asvc,
            mix_info_exists: flags & 8 != 0,
            substreams,
            additional_info: &self.data[position..],
        })
    }
}
