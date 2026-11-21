//! Packets: one frame of a Block with its source timing, and the same frame
//! with the track configuration and BlockGroup side data it was read under.
use crate::storage::SharedBytes;
use crate::timing::{BlockTiming, ExactTime};
use crate::{Error, Result};

pub(crate) mod batch;
pub(crate) mod block_group;
pub(crate) mod lace;

pub use batch::TypedPacket;

/// One frame's payload with its content encodings removed, in charged storage that
/// can be shared with the rest of its Block or input window.
#[derive(Clone, Debug)]
pub struct PacketData {
    pub(crate) storage: SharedBytes,
    pub(crate) range: std::ops::Range<usize>,
}
impl AsRef<[u8]> for PacketData {
    fn as_ref(&self) -> &[u8] {
        &self.storage[self.range.clone()]
    }
}
impl std::ops::Deref for PacketData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}
impl PacketData {
    /// Share the restored frame in its original charged allocation.
    pub fn shared_bytes(&self) -> Result<Option<triergon_core::Bytes>> {
        self.storage
            .as_core()
            .map(|storage| storage.slice(self.range.clone()))
            .transpose()
    }

    /// Capacity in bytes of the allocation this frame keeps charged, which exceeds
    /// its length when it shares a larger Block or input allocation.
    pub fn retained_capacity(&self) -> usize {
        self.storage.retained_capacity()
    }
    /// Whether both frames lie in one backing allocation.
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        self.storage.shares_storage_with(&other.storage)
    }
}

/// One frame of a SimpleBlock or BlockGroup with its source timing.
#[derive(Clone, Debug)]
pub struct Packet {
    /// The frame's source timing facts; [`BlockTiming::source`] gives its exact time.
    pub timing: BlockTiming,
    /// Index of the track in [`Configuration::tracks`](crate::Configuration::tracks),
    /// not its TrackNumber.
    pub track: usize,
    /// Later laced frames are unknown unless the container supplies their duration.
    pub pts_ns: Option<i64>,
    /// Matroska supplies presentation time, not a codec-derived decode timestamp.
    pub dts_ns: Option<i64>,
    /// Frame duration in nanoseconds, rounded once from the exact source duration;
    /// `None` when neither BlockDuration nor DefaultDuration is known.
    pub duration_ns: Option<u64>,
    /// The SimpleBlock keyframe flag; for a BlockGroup, whether it has no ReferenceBlock.
    pub keyframe: bool,
    /// The Block's DiscardPadding in nanoseconds; 0 when absent.
    pub discard_padding_ns: i64,
    /// The frame payload.
    pub data: PacketData,
}

/// All fallible packet-clock projection precedes transferring a ready frame.
/// This is the same source clock used by the blocking and incremental adapters.
struct PacketFields {
    timing: BlockTiming,
    track: usize,
    pts_ns: Option<i64>,
    duration_ns: Option<u64>,
    keyframe: bool,
    discard_padding_ns: i64,
}
impl PacketFields {
    #[inline(always)]
    fn new(
        track: usize,
        timing: BlockTiming,
        keyframe: bool,
        discard_padding_ns: i64,
    ) -> Result<Self> {
        let (pts_ns, duration_ns) = if timing.duration_ticks.is_none() {
            // The block time and track default are already integral nanoseconds.
            let base = timing.source_base_ns()?;
            let pts = match (timing.frame, timing.default_duration_ns) {
                (0, _) => Some(base),
                (frame, Some(duration)) => Some(
                    base.checked_add(i128::from(duration) * i128::from(frame))
                        .ok_or(Error::Overflow)?,
                ),
                (_, None) => None,
            };
            (
                pts.map(i64::try_from)
                    .transpose()
                    .map_err(|_| Error::Overflow)?,
                timing.default_duration_ns,
            )
        } else {
            let source = timing.source()?;
            (
                source.pts.map(ExactTime::rounded_ns).transpose()?,
                source
                    .duration
                    .map(ExactTime::rounded_duration_ns)
                    .transpose()?,
            )
        };
        Ok(Self {
            timing,
            track,
            pts_ns,
            duration_ns,
            keyframe,
            discard_padding_ns,
        })
    }
    #[inline(always)]
    fn finish(self, data: PacketData) -> Packet {
        Packet {
            timing: self.timing,
            track: self.track,
            pts_ns: self.pts_ns,
            dts_ns: None,
            duration_ns: self.duration_ns,
            keyframe: self.keyframe,
            discard_padding_ns: self.discard_padding_ns,
            data,
        }
    }
}
