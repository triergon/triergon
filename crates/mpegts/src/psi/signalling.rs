//! Bounded descriptor views for H.222.0 §2.6 and EN 300 468 Annex D.
//! Each parser reads at most one 255-byte descriptor body. Variable tails borrow
//! the charged source, and the fields describe signalling without inspecting coded payloads.
use super::{Descriptor, descriptor_tag};
use triergon_core::{Error, Result};

/// A registration descriptor (tag 5), borrowed from its descriptor bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registration<'a> {
    /// The 32-bit format identifier, as four source bytes.
    pub format_identifier: [u8; 4],
    /// The additional identification bytes after the format identifier; may be empty.
    pub additional_identification_info: &'a [u8],
}

/// One entry of an ISO 639 language descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageEntry {
    /// The ISO 639-2 language code as its three source bytes, unchecked.
    pub language_code: [u8; 3],
    /// The 8-bit audio type (H.222.0 Table 2-61), user-private and reserved values included.
    pub audio_type: u8,
}

/// Iterator over the 4-byte entries of an ISO 639 language descriptor.
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

/// An AVC video descriptor (tag 0x28).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcVideo {
    /// Coded AVC profile identifier from the stream descriptor, 0–255.
    pub profile_idc: u8,
    /// Constraint set flags 0 through 5, in source order.
    pub constraint_set_flags: [bool; 6],
    /// The 2-bit `AVC_compatible_flags`.
    pub compatible_flags: u8,
    /// Coded AVC level identifier from the stream descriptor, 0–255.
    pub level_idc: u8,
    /// `AVC_still_present`: the stream may contain AVC still pictures.
    pub still_present: bool,
    /// `AVC_24_hour_picture_flag`: the stream may contain pictures presented more than 24 hours
    /// after decoding.
    pub picture_24h: bool,
    /// Whether the AVC stream signals absence of frame-packing arrangement SEI messages.
    pub frame_packing_sei_not_present: bool,
}

/// An HEVC video descriptor (tag 0x38).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcVideo {
    /// Selects the namespace, 0–3, for the HEVC profile identifier.
    pub profile_space: u8,
    /// Selects the HEVC tier. `true` is High, `false` is Main.
    pub high_tier: bool,
    /// Coded HEVC profile identifier, 0–31, within the signalled profile space.
    pub profile_idc: u8,
    /// The 32 profile compatibility flags, first flag in the most significant bit.
    pub profile_compatibility_indication: u32,
    /// Whether the descriptor signals progressive source content.
    pub progressive_source: bool,
    /// Whether the descriptor signals interlaced source content.
    pub interlaced_source: bool,
    /// Whether the profile-tier-level constraints exclude frame packing.
    pub non_packed_constraint: bool,
    /// Whether the profile-tier-level constraints restrict pictures to frames.
    pub frame_only_constraint: bool,
    /// Forty-four unmodified profile-tier-level constraint bits, right-aligned.
    pub copied_44bits: u64,
    /// Coded HEVC level identifier from the stream descriptor, 0–255.
    pub level_idc: u8,
    /// Whether this stream may carry HEVC still pictures.
    pub still_present: bool,
    /// Whether pictures may be presented more than 24 hours after decoding.
    pub picture_24h: bool,
    /// Whether the stream signals absence of sub-picture hypothetical reference decoder parameters.
    pub sub_pic_hrd_params_not_present: bool,
    /// Raw two-bit value. Descriptor signalling alone does not validate HDR video.
    pub hdr_wcg_idc: u8,
    /// The minimum and maximum temporal ID, when the temporal layer subset flag is set; minimum
    /// never exceeds maximum.
    pub temporal_ids: Option<(u8, u8)>,
}

/// A DVB AC-3 descriptor (tag 0x6A), borrowed from its descriptor bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbAc3<'a> {
    /// Optional 8-bit audio component classification, as signalled by the descriptor.
    pub component_type: Option<u8>,
    /// Optional audio bitstream identification byte (`bsid`).
    pub bsid: Option<u8>,
    /// Optional identifier byte for the associated main audio service.
    pub mainid: Option<u8>,
    /// Optional associated-service information byte.
    pub asvc: Option<u8>,
    /// The reserved low bits of the flags byte as encoded (EN 300 468 sets them to zero).
    pub reserved_flags: u8,
    /// The additional info bytes after the optional fields; may be empty.
    pub additional_info: &'a [u8],
}

/// A DVB enhanced AC-3 descriptor (tag 0x7A), borrowed from its descriptor bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbEac3<'a> {
    /// Optional 8-bit audio component classification, as signalled by the descriptor.
    pub component_type: Option<u8>,
    /// Optional audio bitstream identification byte (`bsid`).
    pub bsid: Option<u8>,
    /// Optional identifier byte for the associated main audio service.
    pub mainid: Option<u8>,
    /// Optional associated-service information byte.
    pub asvc: Option<u8>,
    /// Whether mixing information is present in the signalled E-AC-3 stream.
    pub mix_info_exists: bool,
    /// The `substream1` to `substream3` bytes, each present when its flag is set.
    pub substreams: [Option<u8>; 3],
    /// The additional info bytes after the optional fields; may be empty.
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
    /// Parses this descriptor as a registration descriptor.
    ///
    /// Fails with `Invalid` when the tag is not 5 or the body is shorter than 4 bytes.
    pub fn registration(self) -> Result<Registration<'a>> {
        if self.tag != descriptor_tag::REGISTRATION {
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
    /// Parses this descriptor as an ISO 639 language descriptor and iterates its entries.
    ///
    /// Fails with `Invalid` when the tag is not 10 or the body length is not a multiple of 4.
    pub fn languages(self) -> Result<LanguageEntries<'a>> {
        if self.tag != descriptor_tag::ISO_639_LANGUAGE {
            return Err(Error::Invalid("language descriptor tag"));
        }
        if !self.data.len().is_multiple_of(4) {
            return Err(Error::Invalid("language descriptor length"));
        }
        Ok(LanguageEntries {
            remaining: self.data,
        })
    }
    /// Borrow the AVC profile, level and source flags from a four-byte descriptor body.
    /// A wrong tag or length returns `Invalid`; the reserved bits are ignored.
    pub fn avc_video(self) -> Result<AvcVideo> {
        if self.tag != descriptor_tag::AVC_VIDEO {
            return Err(Error::Invalid("AVC descriptor tag"));
        }
        let data = self.data;
        if data.len() != 4 {
            return Err(Error::Invalid("AVC descriptor length"));
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
    /// Decode HEVC profile, tier and level, picture flags and optional temporal IDs.
    /// A wrong tag, illegal length or an inverted temporal-ID range returns `Invalid`; the
    /// reserved bits are ignored. The PMT reader checks the enclosing stream type.
    // Inlined into the PMT and private-audio descriptor loops it measured +0.065% there.
    #[inline(never)]
    pub fn hevc_video(self) -> Result<HevcVideo> {
        if self.tag != descriptor_tag::HEVC_VIDEO {
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
        let temporal_ids = if subset {
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
    /// In a DVB descriptor loop, decode the AC-3 flags byte and its indicated fields.
    /// A wrong tag, missing flags or missing signalled field returns `Invalid`.
    pub fn dvb_ac3(self) -> Result<DvbAc3<'a>> {
        if self.tag != descriptor_tag::DVB_AC3 {
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
    /// In a DVB descriptor loop, decode the E-AC-3 fields and up to three substream bytes.
    /// The mixing information bit consumes no extra byte. A wrong tag or missing
    /// flags or fields returns `Invalid`.
    pub fn dvb_eac3(self) -> Result<DvbEac3<'a>> {
        if self.tag != descriptor_tag::DVB_EAC3 {
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
