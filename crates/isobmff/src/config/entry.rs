//! Identification of a track's first sample entry for the neutral track view.
use super::{ConfigurationLimits, ObjectTypeScan, dts_entry_children};
use crate::{
    BuildStatus, Limits, Next,
    boxes::BoxChildren,
    movie::{EntryLayout, SampleDescription},
};
use triergon_core::container::CodecId;
use triergon_core::{Bytes, Result, WorkBudget};

/// The codec of a sample entry: the one mapping from a track's `hdlr` handler and the
/// entry code, as [`Track`](crate::Track) reports it. `raw ` needs the
/// handler, and `mp4a` and `mp4s` need their stated object type (ISO/IEC 14496-1
/// objectTypeIndication; 0xDD is the private value FFmpeg reads as Vorbis, 0xE0
/// the one it reads as VobSub): without one they stay [`CodecId::Other`].
pub fn codec_id(handler: &[u8; 4], format: &[u8; 4], object_type: Option<u8>) -> CodecId {
    match (handler, format) {
        (b"vide", b"avc1" | b"avc3") => CodecId::H264,
        (b"vide", b"hvc1" | b"hev1" | b"dvh1" | b"dvhe") => CodecId::Hevc,
        (b"vide", b"av01" | b"dav1") => CodecId::Av1,
        (b"vide", b"vp09") => CodecId::Vp9,
        (b"vide", b"raw ") => CodecId::RawVideo,
        (b"soun", b"mp4a") => match object_type {
            Some(0x40) => CodecId::Aac,
            // MPEG-2 audio (ISO/IEC 13818-3) and MPEG-1 audio (ISO/IEC 11172-3).
            Some(0x69 | 0x6b) => CodecId::MpegAudio,
            Some(0xa9 | 0xaa) => CodecId::Dts,
            Some(0xdd) => CodecId::Vorbis,
            _ => CodecId::Other,
        },
        (b"soun", b"ac-3") => CodecId::Ac3,
        (b"soun", b"ec-3") => CodecId::Eac3,
        (b"soun", b"dtsc" | b"dtsh" | b"dtsl") => CodecId::Dts,
        (b"soun", b"Opus") => CodecId::Opus,
        (b"soun", b"fLaC") => CodecId::Flac,
        (b"soun", b".mp3") => CodecId::MpegAudio,
        (b"soun", b"raw " | b"twos") => CodecId::Pcm,
        (b"sbtl" | b"text" | b"subt", b"tx3g") => CodecId::Tx3g,
        (b"subp", b"mp4s") => match object_type {
            Some(0xe0) => CodecId::VobSub,
            _ => CodecId::Other,
        },
        _ => CodecId::Other,
    }
}

/// The sample entry child holding a codec's configuration record.
fn configuration_box(codec: CodecId) -> Option<[u8; 4]> {
    Some(match codec {
        CodecId::H264 => *b"avcC",
        CodecId::Hevc => *b"hvcC",
        CodecId::Av1 => *b"av1C",
        CodecId::Vp9 => *b"vpcC",
        CodecId::Ac3 => *b"dac3",
        CodecId::Eac3 => *b"dec3",
        CodecId::Dts => *b"ddts",
        CodecId::Opus => *b"dOps",
        CodecId::Flac => *b"dfLa",
        _ => return None,
    })
}

#[derive(Debug)]
enum Configuration {
    Record(Bytes),
    /// The codec's record is absent or could not be read.
    Absent,
    /// The entry's layout is not known, so the whole entry payload stands in.
    Entry,
}

// One scan runs per track during opening, so its size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum EntryScan {
    ObjectType(ObjectTypeScan),
    Record(BoxChildren, [u8; 4]),
}

/// Identifies a track's first sample entry as [`Track`](crate::Track) reports it through
/// `TrackInfo`: the object type an `mp4a` or `mp4s` entry's `esds` states, the codec
/// [`codec_id`] gives with it, and the decoder configuration bytes. Poll until
/// [`BuildStatus::Ready`]. An entry that cannot be read keeps the identity and
/// configuration its code alone gives.
#[derive(Debug)]
pub struct EntryConfiguration {
    handler: [u8; 4],
    format: [u8; 4],
    scan: Option<EntryScan>,
    object_type: Option<u8>,
    configuration: Configuration,
}
impl EntryConfiguration {
    /// Starts identifying `entry`, a sample entry of a track whose `hdlr` handler type
    /// is `handler`. `limits` bounds the `esds` read; `format` bounds the walk over the
    /// entry's children, with at most `limits.max_child_boxes` of them.
    pub fn new(
        handler: [u8; 4],
        entry: &SampleDescription,
        limits: ConfigurationLimits,
        format: Limits,
    ) -> Self {
        let (scan, configuration) = entry_scan(handler, entry, limits, format);
        Self {
            handler,
            format: entry.format,
            scan,
            object_type: None,
            configuration,
        }
    }
    /// Reads the entry; `Yield` when the work is spent. Fails only where the record's
    /// bytes cannot be shared.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<BuildStatus> {
        while self.scan.is_some() {
            if self.step(work)? {
                return Ok(BuildStatus::Yield);
            }
        }
        Ok(BuildStatus::Ready)
    }
    /// The object type the entry's `esds` states, for `mp4a` and `mp4s`.
    pub fn object_type(&self) -> Option<u8> {
        self.object_type
    }
    /// The entry's codec: [`codec_id`] with the object type found.
    pub fn codec(&self) -> CodecId {
        codec_id(&self.handler, &self.format, self.object_type)
    }
    /// The configuration record's payload (AudioSpecificConfig for AAC), empty when it is
    /// absent, or `entry`'s whole payload when the entry has no known record.
    pub fn bytes<'a>(&'a self, entry: &'a SampleDescription) -> &'a [u8] {
        match &self.configuration {
            Configuration::Record(bytes) => bytes.as_ref(),
            Configuration::Absent => &[],
            Configuration::Entry => entry.raw().payload(),
        }
    }
}

/// What identifying a track's first entry still has to read, and what its
/// configuration is if that reading finds nothing.
fn entry_scan(
    handler: [u8; 4],
    entry: &SampleDescription,
    limits: ConfigurationLimits,
    format: Limits,
) -> (Option<EntryScan>, Configuration) {
    if matches!(
        (&handler, &entry.format),
        (b"soun", b"mp4a") | (b"subp", b"mp4s")
    ) {
        let scan = ObjectTypeScan::new(entry, limits).ok();
        return (scan.map(EntryScan::ObjectType), Configuration::Entry);
    }
    let codec = codec_id(&handler, &entry.format, None);
    let Some(record) = configuration_box(codec) else {
        return (None, Configuration::Entry);
    };
    let child_limits = Limits {
        max_boxes: limits.max_child_boxes,
        ..format
    };
    let children = match codec {
        CodecId::Dts => dts_entry_children(entry, child_limits),
        CodecId::Ac3 | CodecId::Eac3 | CodecId::Opus | CodecId::Flac => {
            entry.extensions(EntryLayout::AudioV0, child_limits)
        }
        _ => entry.extensions(EntryLayout::Visual, child_limits),
    };
    (
        children
            .ok()
            .map(|children| EntryScan::Record(children, record)),
        Configuration::Absent,
    )
}

/// Advance the scan. `true` means the work is spent. A sample entry that cannot
/// be read keeps the identity and configuration its code alone gives.
impl EntryConfiguration {
    fn step(&mut self, work: &mut WorkBudget) -> Result<bool> {
        let active = &mut self.scan;
        match active.as_mut() {
            Some(EntryScan::ObjectType(scan)) => {
                let found = match scan.poll(work) {
                    Ok(None) => return Ok(true),
                    Ok(Some(found)) => Some(found),
                    Err(_) => None,
                };
                if let Some((object_type, specific)) = found {
                    self.object_type = Some(object_type);
                    if codec_id(&self.handler, &self.format, Some(object_type)) != CodecId::Other {
                        self.configuration =
                            specific.map_or(Configuration::Absent, Configuration::Record);
                    }
                }
                *active = None;
            }
            Some(EntryScan::Record(children, record)) => match children.poll(work) {
                Ok(Next::Yield) => return Ok(true),
                Ok(Next::Item(raw)) if raw.header().kind == *record => {
                    let start = usize::from(raw.header().header_size);
                    let bytes = raw.bytes().slice(start..raw.bytes().len())?;
                    self.configuration = Configuration::Record(bytes);
                    *active = None;
                }
                Ok(Next::Item(_)) => {}
                Ok(Next::End) | Err(_) => *active = None,
            },
            None => {}
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recognised_entry_has_one_name() {
        for (handler, format, object_type, id) in [
            (b"vide", b"avc1", None, CodecId::H264),
            (b"vide", b"avc3", None, CodecId::H264),
            (b"vide", b"hvc1", None, CodecId::Hevc),
            (b"vide", b"hev1", None, CodecId::Hevc),
            (b"vide", b"dvh1", None, CodecId::Hevc),
            (b"vide", b"dvhe", None, CodecId::Hevc),
            (b"vide", b"av01", None, CodecId::Av1),
            (b"vide", b"dav1", None, CodecId::Av1),
            (b"vide", b"vp09", None, CodecId::Vp9),
            (b"vide", b"raw ", None, CodecId::RawVideo),
            (b"soun", b"mp4a", Some(0x40), CodecId::Aac),
            (b"soun", b"mp4a", Some(0x69), CodecId::MpegAudio),
            (b"soun", b"mp4a", Some(0x6b), CodecId::MpegAudio),
            (b"soun", b"mp4a", Some(0xdd), CodecId::Vorbis),
            (b"soun", b".mp3", None, CodecId::MpegAudio),
            (b"soun", b"fLaC", None, CodecId::Flac),
            (b"soun", b"mp4a", Some(0xa9), CodecId::Dts),
            (b"soun", b"mp4a", Some(0xaa), CodecId::Dts),
            (b"soun", b"ac-3", None, CodecId::Ac3),
            (b"soun", b"ec-3", None, CodecId::Eac3),
            (b"soun", b"dtsc", None, CodecId::Dts),
            (b"soun", b"dtsh", None, CodecId::Dts),
            (b"soun", b"dtsl", None, CodecId::Dts),
            (b"soun", b"Opus", None, CodecId::Opus),
            (b"soun", b"raw ", None, CodecId::Pcm),
            (b"soun", b"twos", None, CodecId::Pcm),
            (b"sbtl", b"tx3g", None, CodecId::Tx3g),
            (b"text", b"tx3g", None, CodecId::Tx3g),
            (b"subt", b"tx3g", None, CodecId::Tx3g),
            (b"subp", b"mp4s", Some(0xe0), CodecId::VobSub),
        ] {
            assert_eq!(codec_id(handler, format, object_type), id);
        }
    }

    #[test]
    fn an_unrecognised_entry_is_reported_with_its_own_code() {
        for (handler, format, object_type) in [
            (b"vide", b"xyz1", None),
            (b"soun", b"mp4a", None),
            (b"soun", b"mp4a", Some(0x6a)),
            (b"soun", b"mp4a", Some(0xac)),
            (b"soun", b"flac", None),
            (b"vide", b"fLaC", None),
            (b"soun", b"dtse", None),
            (b"soun", b"dtsx", None),
            (b"vide", b"Opus", None),
            (b"vide", b"mp4a", Some(0x40)),
            (b"soun", b"avc1", None),
            (b"meta", b"tx3g", None),
            (b"subp", b"mp4s", None),
            (b"subp", b"mp4s", Some(0x40)),
        ] {
            assert_eq!(codec_id(handler, format, object_type), CodecId::Other);
        }
    }
}
