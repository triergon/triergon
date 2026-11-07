//! Borrowed packet side data from Matroska/WebM block additions, as FFmpeg's
//! Matroska demuxer exports it. RFC 9559 defines the wire fields and the mapping
//! registry; the policy is documented on
//! [`compat::block_additions`](crate::compat::block_additions).
use super::{
    hdr10_plus::{Hdr10Plus, MAX_BODY, T35_HEADER},
    mapping::TrackExtensions,
    packet::TypedPacket,
    side_data::BlockGroupSource,
    syntax::check,
};
use crate::{Error, Result, incremental::ParseStep};
use std::{mem::discriminant, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockAdditionContainer {
    Matroska,
    WebM,
}

/// One packet side-data entry projected from a block addition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketSideData<'a> {
    /// A mapped or default addition, exported with its 64-bit type as a prefix.
    Opaque { id: u64, payload: &'a [u8] },
    /// LCEVC enhancement data from an ITU-T T.35 addition, without its header.
    Lcevc(&'a [u8]),
}

impl<'a> PacketSideData<'a> {
    /// The optional inline prefix and the original, borrowed packet-side bytes.
    pub fn wire_parts(self) -> (Option<[u8; 8]>, &'a [u8]) {
        match self {
            Self::Opaque { id, payload } => (Some(id.to_be_bytes()), payload),
            Self::Lcevc(payload) => (None, payload),
        }
    }
}

fn t35(data: &[u8]) -> Result<Option<PacketSideData<'_>>> {
    match data {
        // Records shorter than six bytes are ignored.
        [0xb4, _, 0x50, 0x00, payload @ ..] if payload.len() >= 2 => {
            Ok(Some(PacketSideData::Lcevc(payload)))
        }
        [0xb5, 0, 0x3c, 0, 1, 4, ..] | [0xb5, 0, 0x90, 0, 1, _, ..] => Err(Error::Unsupported(
            "Matroska ITU-T T.35 dynamic HDR needs a metadata decoder",
        )),
        _ => Ok(None),
    }
}

fn mapped_id(
    raw_id: u64,
    extensions: &TrackExtensions,
    container: BlockAdditionContainer,
) -> Option<u64> {
    let mapping = extensions
        .mappings()
        .find(|entry| entry.value() == Some(raw_id));
    if raw_id != 1 && container == BlockAdditionContainer::Matroska && mapping.is_none() {
        return None;
    }
    Some(mapping.map_or(raw_id, |entry| match entry.kind() {
        0 => raw_id,
        kind => kind,
    }))
}

fn mapping_work(group: &BlockGroupSource, extensions: &TrackExtensions) -> Result<u64> {
    let per_addition = (extensions.mappings().len() as u64)
        .checked_add(8)
        .ok_or(Error::Overflow)?;
    (group.additions().len() as u64)
        .checked_mul(per_addition)
        .ok_or(Error::Overflow)
}

impl TypedPacket {
    fn addition_group(&self, extensions: &TrackExtensions) -> Result<Option<&BlockGroupSource>> {
        let Some(group) = self
            .group
            .as_ref()
            .filter(|group| group.max_addition_id() != 0)
        else {
            return Ok(None);
        };
        let track = self
            .configuration
            .tracks()
            .get(self.packet.track)
            .ok_or(Error::Invalid("missing packet addition track"))?;
        if !std::ptr::eq(track.source_metadata.as_ref(), extensions.raw_track_entry()) {
            return Err(Error::Invalid("packet additions TrackEntry mismatch"));
        }
        Ok(Some(group))
    }
}

/// Decode the HDR10+ (ITU-T T.35 Application 4) event from the packet's block
/// additions: an addition mapped to type 4, or on WebM an implicit addition 4.
/// The same event applies to every lace. Each recognized record validates, and
/// the last recognized event is returned.
pub fn hdr10_plus<'a>(
    packet: &'a TypedPacket,
    extensions: &TrackExtensions,
    container: BlockAdditionContainer,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<ParseStep<Option<Hdr10Plus<'a>>>> {
    check(cancel)?;
    let Some(group) = packet.addition_group(extensions)? else {
        return Ok(ParseStep::Event(None));
    };
    let mut required = mapping_work(group, extensions)?;
    for addition in group.additions() {
        check(cancel)?;
        let raw = addition.data();
        if raw.starts_with(&T35_HEADER) {
            let cost = raw.len().min(T35_HEADER.len() + MAX_BODY) as u64 * 8;
            required = required.checked_add(cost).ok_or(Error::Overflow)?;
        }
    }
    if !work.try_charge(required) {
        return Ok(ParseStep::Yield);
    }
    let mut output = None;
    for addition in group.additions() {
        check(cancel)?;
        let raw = addition.data();
        if raw.starts_with(&T35_HEADER)
            && mapped_id(addition.id(), extensions, container) == Some(4)
        {
            Hdr10Plus::cost(raw)?;
            output = Some(Hdr10Plus::decode(raw, cancel)?);
        }
    }
    Ok(ParseStep::Event(output))
}

/// Project opaque and LCEVC packet side data using the extensions parsed from
/// this packet's TrackEntry. Parse those once with the configuration's
/// track_extensions_job and reuse the result across packets and laces.
///
/// The caller selects the container from the source DocType. Every lace gets
/// the same projection. A later nonempty value of a projected side-data
/// kind replaces that kind in place. The original group remains available.
///
/// The returned array is in encounter order, with unused slots at the end.
/// A work yield leaves the operation retryable. A dynamic HDR T.35 addition is
/// refused with `Unsupported`; decode it with [`hdr10_plus`].
pub fn packet_side_data<'a>(
    packet: &'a TypedPacket,
    extensions: &TrackExtensions,
    container: BlockAdditionContainer,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<ParseStep<[Option<PacketSideData<'a>>; 2]>> {
    check(cancel)?;
    let mut result = [None, None];
    let Some(group) = packet.addition_group(extensions)? else {
        return Ok(ParseStep::Event(result));
    };
    if !work.try_charge(mapping_work(group, extensions)?) {
        return Ok(ParseStep::Yield);
    }
    for addition in group.additions() {
        check(cancel)?;
        let payload = addition.data();
        if payload.is_empty() {
            continue;
        }
        let Some(id) = mapped_id(addition.id(), extensions, container) else {
            continue;
        };
        let value = if id == 4 {
            let Some(value) = t35(payload)? else {
                continue;
            };
            value
        } else {
            PacketSideData::Opaque { id, payload }
        };
        let slot = result
            .iter_mut()
            .find(|slot| slot.is_none_or(|entry| discriminant(&entry) == discriminant(&value)))
            .ok_or(Error::Invalid("packet addition projection capacity"))?;
        *slot = Some(value);
    }
    Ok(ParseStep::Event(result))
}
