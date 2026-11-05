//! Speaker positions and output channel layouts.
//!
//! Planes are ordered by ascending [`Speaker`] discriminant, which equals the bit index of
//! FFmpeg's public `AV_CH_*` masks; this is the "native" order FFmpeg's AAC decoder uses for
//! its output, so plane `i` of this decoder corresponds to channel `i` of FFmpeg's output.

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
    LowFrequency2 = 35,
    TopSideLeft = 36,
    TopSideRight = 37,
    BottomFrontCenter = 38,
    BottomFrontLeft = 39,
    BottomFrontRight = 40,
}

impl Speaker {
    /// FFmpeg-style short name.
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

/// Maximum number of output channels (bounded by the 4-bit element tags of a PCE).
pub const MAX_CHANNELS: usize = 48;

/// Ordered set of speaker positions, one per output plane.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChannelLayout {
    speakers: Vec<Speaker>,
}

impl ChannelLayout {
    pub fn from_speakers(s: &[Speaker]) -> ChannelLayout {
        ChannelLayout {
            speakers: s.to_vec(),
        }
    }

    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers
    }

    pub fn channels(&self) -> usize {
        self.speakers.len()
    }

    /// OR of the FFmpeg masks (0 when positions repeat).
    pub fn mask(&self) -> u64 {
        let mut m = 0u64;
        for s in &self.speakers {
            if m & s.mask() != 0 {
                return 0;
            }
            m |= s.mask();
        }
        m
    }

    /// FFmpeg layout name where one exists, else `"N channels (A+B+...)"` like ffprobe.
    pub fn describe(&self) -> String {
        let named: &[(u64, &str)] = &[
            (0x4, "mono"),
            (0x3, "stereo"),
            (0x7, "3.0"),
            (0x107, "4.0"),
            (0x37, "5.0(back)"),
            (0x607, "5.0"),
            (0x3f, "5.1(back)"),
            (0x60f, "5.1"),
            (0xff, "7.1(wide)"),
            (0x63f, "7.1"),
            (0x503f, "5.1.2(back)"),
            (0x13, "3.0(back)"),
            (0x33, "quad"),
            (0x603, "quad(side)"),
            (0xb, "2.1"),
            (0x70f, "6.1"),
            (0x13f, "6.1(back)"),
        ];
        let m = self.mask();
        for &(nm, s) in named {
            if nm == m {
                return s.to_string();
            }
        }
        let names: Vec<&str> = self.speakers.iter().map(|s| s.name()).collect();
        format!("{} channels ({})", self.speakers.len(), names.join("+"))
    }
}
