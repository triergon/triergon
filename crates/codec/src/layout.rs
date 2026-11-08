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
        use Speaker::{
            BackCenter, BackLeft, BackRight, BottomFrontCenter, BottomFrontLeft, BottomFrontRight,
            FrontCenter, FrontLeft, FrontLeftOfCenter, FrontRight, FrontRightOfCenter,
            LowFrequency, LowFrequency2, SideLeft, SideRight, SurroundDirectLeft,
            SurroundDirectRight, TopBackCenter, TopBackLeft, TopBackRight, TopCenter,
            TopFrontCenter, TopFrontLeft, TopFrontRight, TopSideLeft, TopSideRight, WideLeft,
            WideRight,
        };
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

    /// FFmpeg's name for the layout, as `ffprobe` reports `channel_layout`: a standard
    /// layout's name (`stereo`, `5.1(side)`, ...), else the speaker names joined by `+`,
    /// or `N channels` when a speaker repeats.
    pub fn name(&self) -> String {
        let mask = self.mask();
        if mask == 0 {
            return format!("{} channels", self.channels());
        }
        if let Some((_, name)) = STANDARD.iter().find(|(m, _)| *m == mask) {
            return (*name).to_owned();
        }
        let names: Vec<&str> = self.speakers().iter().map(|s| s.name()).collect();
        names.join("+")
    }

    /// Plane index of `speaker`, if present.
    pub fn position(&self, speaker: Speaker) -> Option<usize> {
        self.speakers().iter().position(|&s| s == speaker)
    }
}

/// FFmpeg's standard layouts by mask (`ffmpeg -layouts`), in its order: the first match
/// names a mask.
const STANDARD: &[(u64, &str)] = &[
    (0x4, "mono"),
    (0x3, "stereo"),
    (0xb, "2.1"),
    (0x7, "3.0"),
    (0x103, "3.0(back)"),
    (0x107, "4.0"),
    (0x33, "quad"),
    (0x603, "quad(side)"),
    (0xf, "3.1"),
    (0x37, "5.0"),
    (0x607, "5.0(side)"),
    (0x10f, "4.1"),
    (0x3f, "5.1"),
    (0x60f, "5.1(side)"),
    (0x707, "6.0"),
    (0x6c3, "6.0(front)"),
    (0x500f, "3.1.2"),
    (0x137, "hexagonal"),
    (0x70f, "6.1"),
    (0x13f, "6.1(back)"),
    (0x6cb, "6.1(front)"),
    (0x637, "7.0"),
    (0x6c7, "7.0(front)"),
    (0x63f, "7.1"),
    (0xff, "7.1(wide)"),
    (0x6cf, "7.1(wide-side)"),
    (0x560f, "5.1.2"),
    (0x503f, "5.1.2(back)"),
    (0x737, "octagonal"),
    (0x2d033, "cube"),
    (0x2d60f, "5.1.4"),
    (0x563f, "7.1.2"),
    (0x2d63f, "7.1.4"),
    (0x80001563f, "7.2.3"),
    (0x2d6ff, "9.1.4"),
    (0x300002d6ff, "9.1.6"),
    (0x18003f737, "hexadecagonal"),
    (0x1f80003ffff, "22.2"),
];

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

/// Speaker of each coded channel in the Vorbis I channel order (section 4.3.9), which
/// Opus channel mapping family 1 also uses (RFC 7845 section 5.1.1.2), for 1 to 8
/// channels. `None` beyond 8, where the order is application defined.
#[must_use]
pub fn vorbis_order(channels: usize) -> Option<&'static [Speaker]> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency,
        SideLeft, SideRight,
    };
    Some(match channels {
        1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontCenter, FrontRight],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontCenter, FrontRight, BackLeft, BackRight],
        6 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            BackLeft,
            BackRight,
            LowFrequency,
        ],
        7 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            SideLeft,
            SideRight,
            BackCenter,
            LowFrequency,
        ],
        8 => &[
            FrontLeft,
            FrontCenter,
            FrontRight,
            SideLeft,
            SideRight,
            BackLeft,
            BackRight,
            LowFrequency,
        ],
        _ => return None,
    })
}

/// Speaker of each coded channel in the FLAC default order (RFC 9639 section 9.1.3), for
/// 1 to 8 channels.
#[must_use]
pub fn flac_order(channels: usize) -> Option<&'static [Speaker]> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency,
        SideLeft, SideRight,
    };
    Some(match channels {
        1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontRight, FrontCenter],
        4 => &[FrontLeft, FrontRight, BackLeft, BackRight],
        5 => &[FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
        6 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
        ],
        7 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackCenter,
            SideLeft,
            SideRight,
        ],
        8 => &[
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ],
        _ => return None,
    })
}

/// The output layout for channels coded in `coded` order, and the plane each coded
/// channel goes to: planes follow ascending [`Speaker`] order, as everywhere in this crate.
/// `None` if a speaker repeats or there are more than [`ChannelLayout::CAPACITY`].
#[must_use]
pub fn native_order(coded: &[Speaker]) -> Option<(ChannelLayout, [u8; ChannelLayout::CAPACITY])> {
    let mut sorted = ChannelLayout::new(coded.iter().copied())?;
    sorted.speakers[..coded.len()].sort_unstable();
    if sorted.speakers().windows(2).any(|w| w[0] == w[1]) {
        return None;
    }
    let mut plane = [0u8; ChannelLayout::CAPACITY];
    for (i, speaker) in coded.iter().enumerate() {
        plane[i] = sorted.position(*speaker)? as u8;
    }
    Some((sorted, plane))
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

    #[test]
    fn vorbis_and_flac_orders_map_to_ffmpeg_layouts() {
        use Speaker::*;
        // 5.1: Vorbis FL FC FR BL BR LFE -> FFmpeg 5.1(back) FL FR FC LFE BL BR.
        let (layout, plane) = native_order(vorbis_order(6).unwrap()).unwrap();
        assert_eq!(layout.mask(), 0x3f);
        assert_eq!(&plane[..6], &[0, 2, 1, 4, 5, 3]);
        // 7.1: FL FC FR SL SR BL BR LFE -> FL FR FC LFE BL BR SL SR.
        let (layout, plane) = native_order(vorbis_order(8).unwrap()).unwrap();
        assert_eq!(layout.mask(), 0x63f);
        assert_eq!(&plane[..8], &[0, 2, 1, 6, 7, 4, 5, 3]);
        assert_eq!(
            native_order(vorbis_order(7).unwrap()).unwrap().0.speakers(),
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackCenter,
                SideLeft,
                SideRight
            ]
        );
        for channels in 1..=8 {
            let (_, plane) = native_order(flac_order(channels).unwrap()).unwrap();
            assert!(
                plane[..channels]
                    .iter()
                    .enumerate()
                    .all(|(i, &p)| usize::from(p) == i)
            );
        }
        assert!(vorbis_order(9).is_none());
        let name = |coded: &[Speaker]| native_order(coded).unwrap().0.name();
        assert_eq!(name(vorbis_order(2).unwrap()), "stereo");
        assert_eq!(name(vorbis_order(6).unwrap()), "5.1");
        assert_eq!(name(vorbis_order(7).unwrap()), "6.1");
        assert_eq!(name(vorbis_order(8).unwrap()), "7.1");
        assert_eq!(name(&[FrontLeft, LowFrequency]), "FL+LFE");
        assert_eq!(
            ChannelLayout::new([FrontCenter; 3]).unwrap().name(),
            "3 channels"
        );
        assert!(native_order(&[FrontLeft, FrontLeft]).is_none());
    }
}
