//! Source descriptor views from H.222.0 (08/2023) §§2.6.90–91, 2.6.137–140.
use super::{Descriptor, ElementaryStream, ProgramMap};
use crate::TraceEvent;
use triergon_core::{Error, Result, WorkBudget};

/// A source-signalled base/enhancement relationship in one active PMT epoch.
/// The borrowed map retains the namespace, ordered descriptors and physical ranges.
/// This is not codec admission or a relationship between access units.
#[derive(Debug)]
pub struct LcevcAssociation<'a> {
    pub program_map: &'a ProgramMap,
    pub configuration_epoch: u64,
    pub base: &'a ElementaryStream,
    pub enhancement: &'a ElementaryStream,
    pub video: LcevcVideo,
}

impl TraceEvent {
    /// Require one explicit same-program association for an enhancement PID.
    ///
    /// H.222.0 §§2.6.138/140 define the matching tag. This bounded view supports
    /// MPEG-1/2, AVC and HEVC bases, independently of the private namespace.
    /// Repeated linkage occurrences on one base are equivalent; distinct bases
    /// sharing the tag are ambiguous. Enhancement PIDs may share a base and tag.
    /// Multiple video descriptors on the selected enhancement are unsupported.
    ///
    /// Charges the stream-length scan first, then reserves work for both
    /// descriptor scans. If either charge fails, retry with a larger grant.
    /// The result borrows the active map and configuration epoch of this event.
    pub fn require_lcevc_association(
        &self,
        enhancement_pid: u16,
        work: &mut WorkBudget,
    ) -> Result<LcevcAssociation<'_>> {
        let Self::ProgramMap {
            map,
            active: true,
            configuration_epoch,
        } = self
        else {
            return Err(Error::Invalid("LCEVC association requires active PMT"));
        };
        if !work.try_charge(map.streams.len() as u64) {
            return Err(Error::Invalid("LCEVC association work grant"));
        }
        let cost = map
            .streams
            .iter()
            .try_fold(0_u64, |sum, stream| {
                sum.checked_add(stream.descriptor_bytes.len() as u64 + 1)
                    .ok_or(Error::Overflow)
            })?
            .checked_mul(2)
            .ok_or(Error::Overflow)?;
        if !work.try_charge(cost) {
            return Err(Error::Invalid("LCEVC association work grant"));
        }
        let mut selected = None;
        for stream in map.streams.iter() {
            if stream.pid == enhancement_pid && selected.replace(stream).is_some() {
                return Err(Error::Invalid("ambiguous LCEVC enhancement PID"));
            }
        }
        let enhancement = selected.ok_or(Error::Invalid("missing LCEVC enhancement PID"))?;
        if enhancement.stream_type != 0x36 {
            return Err(Error::Unsupported("LCEVC enhancement stream type"));
        }
        let mut video = None;
        for descriptor in enhancement.descriptors() {
            let descriptor = descriptor?;
            if descriptor.tag == 0x3f && descriptor.data.first() == Some(&0x17) {
                let value = descriptor.lcevc_video()?;
                if video.replace(value).is_some() {
                    return Err(Error::Unsupported("multiple LCEVC video descriptors"));
                }
            }
        }
        let video = video.ok_or(Error::Invalid("missing LCEVC video descriptor"))?;
        let mut base = None;
        for stream in map.streams.iter() {
            for descriptor in stream.descriptors() {
                let descriptor = descriptor?;
                if descriptor.tag != 0x3f {
                    continue;
                }
                if descriptor.data.first() == Some(&0x18) {
                    if !descriptor
                        .lcevc_linkage()?
                        .lcevc_stream_tags
                        .contains(&video.lcevc_stream_tag)
                    {
                        continue;
                    }
                    if stream.pid == enhancement_pid {
                        return Err(Error::Invalid("LCEVC self association"));
                    }
                    if base.is_some_and(|old: &ElementaryStream| !std::ptr::eq(old, stream)) {
                        return Err(Error::Invalid("ambiguous LCEVC base tag"));
                    }
                    base = Some(stream);
                }
            }
        }
        let base = base.ok_or(Error::Invalid("missing LCEVC base linkage"))?;
        if !matches!(base.stream_type, 0x01 | 0x02 | 0x1b | 0x24) {
            return Err(Error::Unsupported("LCEVC base stream type"));
        }
        Ok(LcevcAssociation {
            program_map: map,
            configuration_epoch: *configuration_epoch,
            base,
            enhancement,
            video,
        })
    }
}

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
