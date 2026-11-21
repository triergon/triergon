/// The bits per pixel of a region (EN 300 743 region depth).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// 2 bits: four colours.
    Two,
    /// 4 bits: 16 colours.
    Four,
    /// 8 bits: 256 colours.
    Eight,
}

impl Depth {
    pub(crate) fn bits(self) -> u8 {
        match self {
            Self::Two => 2,
            Self::Four => 4,
            Self::Eight => 8,
        }
    }

    pub(crate) fn maximum(self) -> u8 {
        match self {
            Self::Two => 3,
            Self::Four => 15,
            Self::Eight => 255,
        }
    }
}

/// A display size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}
