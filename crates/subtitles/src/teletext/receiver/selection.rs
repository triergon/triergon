//! Which completed pages become subtitles.

use crate::teletext::packet::{Header, PageNumber};

/// Which pages become subtitles (`txt_page`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PageSelection {
    /// `*`: every page.
    #[default]
    All,
    /// `subtitle`: the pages whose latest header has C6 (subtitle) and C7
    /// (suppress header) but not C5 (newsflash).
    Subtitles,
    /// Any other text: the pages whose three-digit number occurs in it.
    Numbers(String),
}

impl PageSelection {
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text {
            "*" => Self::All,
            "subtitle" => Self::Subtitles,
            other => Self::Numbers(other.to_owned()),
        }
    }

    pub(super) fn selects(&self, page: PageNumber, subtitle: bool) -> bool {
        match self {
            Self::All => true,
            Self::Subtitles => subtitle,
            Self::Numbers(list) => {
                let number = page.hex();
                let digits = [number >> 8, number >> 4, number].map(|d| b'0' + (d & 0xF) as u8);
                list.as_bytes().windows(3).any(|w| w == digits)
            }
        }
    }
}

/// The pages whose latest header marks them as subtitles: C6 (subtitle) and
/// C7 (suppress header) without C5 (newsflash). Selection and the output
/// follow it as it stands after all headers of the packet that completed a page.
#[derive(Debug)]
pub(super) struct SubtitlePages([u64; 32]);

impl SubtitlePages {
    pub(super) const NONE: Self = Self([0; 32]);

    fn bit(page: PageNumber) -> (usize, u64) {
        let index = usize::from(page.hex() - 0x100);
        (index / 64, 1 << (index % 64))
    }

    pub(super) fn observe(&mut self, header: &Header) {
        let control = header.control;
        let (word, bit) = Self::bit(header.page);
        if control.subtitle && control.suppress_header && !control.newsflash {
            self.0[word] |= bit;
        } else {
            self.0[word] &= !bit;
        }
    }

    pub(super) fn contains(&self, page: PageNumber) -> bool {
        let (word, bit) = Self::bit(page);
        self.0[word] & bit != 0
    }
}
