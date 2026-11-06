//! Speaker positions and output channel layouts.
//!
//! # Channel order
//!
//! A [`Speaker`]'s discriminant is FFmpeg's channel id (`AV_CHAN_*`), which is also
//! the bit index of its `AV_CH_*` mask. Every decoder emits one plane per speaker in
//! ascending discriminant order, which is the "native" order FFmpeg's own decoders
//! use, so plane `i` corresponds to channel `i` of the reference output. The one
//! exception is an AAC program configuration FFmpeg cannot map to a known layout:
//! FFmpeg then keeps the PCE's element order and so does the AAC decoder.

/// A loudspeaker position. Discriminant = FFmpeg `AV_CHAN_*` id / `AV_CH_*` bit index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Speaker {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    LowFrequency = 3,
    BackLeft = 4,
    BackRight = 5,
    FrontLeftOfCenter = 6,
    FrontRightOfCenter = 7,
    BackCenter = 8,
    SideLeft = 9,
    SideRight = 10,
    TopCenter = 11,
    TopFrontLeft = 12,
    TopFrontCenter = 13,
    TopFrontRight = 14,
    TopBackLeft = 15,
    TopBackCenter = 16,
    TopBackRight = 17,
    WideLeft = 31,
    WideRight = 32,
    SurroundDirectLeft = 33,
    SurroundDirectRight = 34,
    LowFrequency2 = 35,
    TopSideLeft = 36,
    TopSideRight = 37,
    BottomFrontCenter = 38,
    BottomFrontLeft = 39,
    BottomFrontRight = 40,
}

impl Speaker {
    /// FFmpeg's short channel name (`FL`, `FR`, ...).
    pub fn name(self) -> &'static str {
        use Speaker::*;
        match self {
            FrontLeft => "FL",
            FrontRight => "FR",
            FrontCenter => "FC",
            LowFrequency => "LFE",
            BackLeft => "BL",
            BackRight => "BR",
            FrontLeftOfCenter => "FLC",
            FrontRightOfCenter => "FRC",
            BackCenter => "BC",
            SideLeft => "SL",
            SideRight => "SR",
            TopCenter => "TC",
            TopFrontLeft => "TFL",
            TopFrontCenter => "TFC",
            TopFrontRight => "TFR",
            TopBackLeft => "TBL",
            TopBackCenter => "TBC",
            TopBackRight => "TBR",
            WideLeft => "WL",
            WideRight => "WR",
            SurroundDirectLeft => "SDL",
            SurroundDirectRight => "SDR",
            LowFrequency2 => "LFE2",
            TopSideLeft => "TSL",
            TopSideRight => "TSR",
            BottomFrontCenter => "BFC",
            BottomFrontLeft => "BFL",
            BottomFrontRight => "BFR",
        }
    }

    /// FFmpeg `AV_CH_*` mask bit.
    pub fn mask(self) -> u64 {
        1u64 << (self as u8)
    }
}

/// Speaker position of each output plane, in plane order.
///
/// Inline storage, so a layout is `Copy` and comparing or caching one never allocates.
/// Equality compares the speakers in plane order.
#[derive(Clone, Copy)]
pub struct ChannelLayout {
    speakers: [Speaker; ChannelLayout::CAPACITY],
    len: u8,
}

impl ChannelLayout {
    /// The most planes any decoder emits: an AAC program configuration element lists at
    /// most 15 front, 15 side and 15 back elements (two channels each as a CPE) and 3
    /// LFEs, 93 in all; AC-3 and DTS stay at 16 and 8.
    pub const CAPACITY: usize = 93;

    /// The empty layout (no planes).
    pub const EMPTY: ChannelLayout = ChannelLayout {
        speakers: [Speaker::FrontCenter; ChannelLayout::CAPACITY],
        len: 0,
    };

    /// A layout with `speakers` as its planes, in the order given. `None` beyond
    /// [`Self::CAPACITY`] planes.
    pub fn new(speakers: impl IntoIterator<Item = Speaker>) -> Option<ChannelLayout> {
        let mut layout = ChannelLayout::EMPTY;
        for speaker in speakers {
            *layout.speakers.get_mut(usize::from(layout.len))? = speaker;
            layout.len += 1;
        }
        Some(layout)
    }

    /// Speakers in plane order.
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers[..usize::from(self.len)]
    }

    /// Number of planes.
    pub fn channels(&self) -> usize {
        usize::from(self.len)
    }

    /// OR of the FFmpeg masks; 0 when a position repeats (no FFmpeg mask describes it).
    pub fn mask(&self) -> u64 {
        let mut mask = 0u64;
        for speaker in self.speakers() {
            if mask & speaker.mask() != 0 {
                return 0;
            }
            mask |= speaker.mask();
        }
        mask
    }

    /// Plane index of `speaker`, if present.
    pub fn position(&self, speaker: Speaker) -> Option<usize> {
        self.speakers().iter().position(|&s| s == speaker)
    }
}

impl Default for ChannelLayout {
    fn default() -> Self {
        ChannelLayout::EMPTY
    }
}

impl PartialEq for ChannelLayout {
    fn eq(&self, other: &Self) -> bool {
        self.speakers() == other.speakers()
    }
}

impl Eq for ChannelLayout {}

impl core::hash::Hash for ChannelLayout {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.speakers().hash(state);
    }
}

impl core::fmt::Debug for ChannelLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list()
            .entries(self.speakers().iter().map(|s| s.name()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_compare_by_planes_and_report_ffmpeg_masks() {
        use Speaker::*;
        let stereo = ChannelLayout::new([FrontLeft, FrontRight]).unwrap();
        let mut reused = ChannelLayout::new([FrontLeft, FrontRight, FrontCenter]).unwrap();
        reused.len = 2;
        assert_eq!(stereo, reused, "stale storage past len is ignored");
        assert_eq!(stereo.mask(), 0x3);
        assert_eq!(stereo.position(FrontRight), Some(1));
        assert_eq!(
            ChannelLayout::new([FrontLeft, FrontLeft]).unwrap().mask(),
            0
        );
        assert_eq!(BottomFrontRight.mask(), 1 << 40);
        assert_eq!(format!("{stereo:?}"), r#"["FL", "FR"]"#);
        assert!(ChannelLayout::new([FrontCenter; ChannelLayout::CAPACITY]).is_some());
        assert!(ChannelLayout::new([FrontCenter; ChannelLayout::CAPACITY + 1]).is_none());
    }
}
