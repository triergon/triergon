//! Speaker positions and channel layouts.
//!
//! Planes are ordered by ascending [`Speaker`] discriminant. The discriminants are the
//! bit positions of the public FFmpeg `AV_CH_*` speaker masks, so this ordering is the
//! "native" order FFmpeg uses for its own decoder output, which keeps comparisons direct.

/// A loudspeaker position. Discriminant = FFmpeg `AV_CH_*` bit index.
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
    WideLeft = 31,
    WideRight = 32,
    SurroundDirectLeft = 33,
    SurroundDirectRight = 34,
    LowFrequency2 = 35,
    TopSideLeft = 36,
    TopSideRight = 37,
}

impl Speaker {
    /// Short FFmpeg-style name (`FL`, `FR`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Speaker::FrontLeft => "FL",
            Speaker::FrontRight => "FR",
            Speaker::FrontCenter => "FC",
            Speaker::LowFrequency => "LFE",
            Speaker::BackLeft => "BL",
            Speaker::BackRight => "BR",
            Speaker::FrontLeftOfCenter => "FLC",
            Speaker::FrontRightOfCenter => "FRC",
            Speaker::BackCenter => "BC",
            Speaker::SideLeft => "SL",
            Speaker::SideRight => "SR",
            Speaker::TopCenter => "TC",
            Speaker::TopFrontLeft => "TFL",
            Speaker::TopFrontCenter => "TFC",
            Speaker::TopFrontRight => "TFR",
            Speaker::WideLeft => "WL",
            Speaker::WideRight => "WR",
            Speaker::SurroundDirectLeft => "SDL",
            Speaker::SurroundDirectRight => "SDR",
            Speaker::LowFrequency2 => "LFE2",
            Speaker::TopSideLeft => "TSL",
            Speaker::TopSideRight => "TSR",
        }
    }

    /// FFmpeg `AV_CH_*` mask bit for this speaker.
    pub fn mask(self) -> u64 {
        1u64 << (self as u8)
    }
}

/// Maximum number of output channels (A/52 Annex E 3.8.2: at most 16 per program).
pub const MAX_CHANNELS: usize = 16;

/// Ordered set of speaker positions, one per output plane.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChannelLayout {
    speakers: [Speaker; MAX_CHANNELS],
    len: u8,
    /// True for AC-3 / E-AC-3 1+1 (dual mono) programs: plane 0 is Ch1, plane 1 is Ch2.
    pub dual_mono: bool,
}

impl Default for ChannelLayout {
    fn default() -> Self {
        ChannelLayout {
            speakers: [Speaker::FrontCenter; MAX_CHANNELS],
            len: 0,
            dual_mono: false,
        }
    }
}

impl core::fmt::Debug for ChannelLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list()
            .entries(self.speakers().iter().map(|s| s.name()))
            .finish()
    }
}

impl ChannelLayout {
    /// Speakers in plane order.
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers[..self.len as usize]
    }

    /// Number of planes.
    pub fn channels(&self) -> usize {
        self.len as usize
    }

    /// FFmpeg-compatible channel mask.
    pub fn mask(&self) -> u64 {
        self.speakers().iter().fold(0, |m, s| m | s.mask())
    }

    /// Plane index of `speaker`, if present.
    pub fn position(&self, speaker: Speaker) -> Option<usize> {
        self.speakers().iter().position(|&s| s == speaker)
    }

    /// Conventional name of a well-known layout (e.g. `5.1(side)`, as ffprobe prints it), else the speaker list.
    pub fn layout_name(&self) -> String {
        use Speaker::*;
        let known: &[(&[Speaker], &str)] = &[
            (&[FrontCenter], "mono"),
            (&[FrontLeft, FrontRight], "stereo"),
            (&[FrontLeft, FrontRight, LowFrequency], "2.1"),
            (&[FrontLeft, FrontRight, FrontCenter], "3.0"),
            (&[FrontLeft, FrontRight, BackCenter], "3.0(back)"),
            (&[FrontLeft, FrontRight, FrontCenter, BackCenter], "4.0"),
            (&[FrontLeft, FrontRight, SideLeft, SideRight], "quad(side)"),
            (&[FrontLeft, FrontRight, FrontCenter, LowFrequency], "3.1"),
            (
                &[FrontLeft, FrontRight, LowFrequency, BackCenter],
                "3.1(back)?",
            ),
            (
                &[FrontLeft, FrontRight, FrontCenter, LowFrequency, BackCenter],
                "4.1",
            ),
            (
                &[FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight],
                "5.0(side)",
            ),
            (
                &[FrontLeft, FrontRight, LowFrequency, SideLeft, SideRight],
                "4.1(side)?",
            ),
            (
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    SideLeft,
                    SideRight,
                ],
                "5.1(side)",
            ),
            (
                &[
                    FrontLeft,
                    FrontRight,
                    FrontCenter,
                    LowFrequency,
                    BackLeft,
                    BackRight,
                    SideLeft,
                    SideRight,
                ],
                "7.1",
            ),
        ];
        for (sp, name) in known {
            if self.speakers() == *sp {
                return (*name).to_string();
            }
        }
        self.speakers()
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
            .join("+")
    }

    /// Build a layout from an unordered speaker set; planes are sorted into native order.
    /// Returns `None` on duplicates or more than [`MAX_CHANNELS`] speakers.
    pub fn from_speakers(list: &[Speaker]) -> Option<ChannelLayout> {
        if list.len() > MAX_CHANNELS {
            return None;
        }
        let mut l = ChannelLayout::default();
        l.speakers[..list.len()].copy_from_slice(list);
        l.len = list.len() as u8;
        l.speakers[..list.len()].sort_unstable();
        if l.speakers[..list.len()].windows(2).any(|w| w[0] == w[1]) {
            return None;
        }
        Some(l)
    }
}
