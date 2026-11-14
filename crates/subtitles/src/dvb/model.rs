#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    Two,
    Four,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}
