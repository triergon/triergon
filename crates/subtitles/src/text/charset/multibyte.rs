//! Strict multibyte charset profiles, each bound to one mapping table, and a
//! decoder that reads them a byte at a time. No charset is detected.
//!
//! The mapping data and licences are described in `MULTIBYTE-CONTRACT.md`.
//! The profiles keep historical Unicode, Windows and restricted EUC
//! repertoires apart; a label that names one does not name the others.
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Error, Result, WorkBudget};

use super::super::unicode::cancelled;

/// A multibyte charset profile: its lead and trail bytes and its mapping
/// table. Standalone ASCII bytes decode as themselves except where noted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultibyteCharset {
    /// Microsoft CP936 table 2.01, including its single-byte euro mapping.
    GbkCp936,
    /// Unicode historical Big5 table 2.0, with its seven NO MAPPING entries refused.
    Big5Unicode,
    /// Unicode historical Shift-JIS table 2.0, which decodes 0x5C as yen and
    /// 0x7E as overline.
    ShiftJisUnicode,
    /// CP949 table 2.01 restricted to A1–FE byte pairs, plus ASCII; the UHC
    /// extension pairs are refused.
    EucKr,
    /// Microsoft CP932 table 2.01.
    Windows932,
    /// Microsoft CP949 table 2.01, including the UHC extension pairs.
    Windows949,
    /// Microsoft CP950 table 2.01.
    Windows950,
    /// The GNU Big5 charmap 0.92 as glibc 2.39 has it, the repertoire FFmpeg
    /// converts with glibc `iconv`.
    ReceiverBig5Glibc239,
    /// [`EucKr`](Self::EucKr) plus bytes 0x80–0x9F as the C1 controls.
    ReceiverEucKrC1,
    /// CP932 plus the 1,880 user-defined codes F040–F9FC as U+E000–U+E757.
    ReceiverWindows932Pua,
    /// [`ReceiverEucKrC1`](Self::ReceiverEucKrC1) plus A2 E8 as U+327E,
    /// which together equal the glibc 2.39 EUC-KR charmap.
    ReceiverEucKrGlibc239,
}

impl MultibyteCharset {
    /// The profile of `label`, matched ASCII case-insensitively with no
    /// trimming: gbk, cp936, windows-936, big5, big-5, shift_jis, shift-jis,
    /// sjis, euc-kr, euc_kr, cp932, windows-932, cp949, windows-949, cp950
    /// or windows-950. `Error::Limit` for a label over 256 bytes,
    /// `Error::Unsupported` for any other. The `Receiver` profiles have no
    /// label here.
    pub fn from_label(label: &[u8]) -> Result<Self> {
        if label.len() > 256 {
            return Err(Error::Limit {
                requested: label.len(),
                maximum: 256,
            });
        }
        for (name, profile) in [
            ("gbk", Self::GbkCp936),
            ("cp936", Self::GbkCp936),
            ("windows-936", Self::GbkCp936),
            ("big5", Self::Big5Unicode),
            ("big-5", Self::Big5Unicode),
            ("shift_jis", Self::ShiftJisUnicode),
            ("shift-jis", Self::ShiftJisUnicode),
            ("sjis", Self::ShiftJisUnicode),
            ("euc-kr", Self::EucKr),
            ("euc_kr", Self::EucKr),
            ("cp932", Self::Windows932),
            ("windows-932", Self::Windows932),
            ("cp949", Self::Windows949),
            ("windows-949", Self::Windows949),
            ("cp950", Self::Windows950),
            ("windows-950", Self::Windows950),
        ] {
            if label.eq_ignore_ascii_case(name.as_bytes()) {
                return Ok(profile);
            }
        }
        Err(Error::Unsupported("explicit multibyte charset label"))
    }

    // Sorted fixed-width DATA records: two-byte wire code, two-byte scalar.
    // At most 16 binary-search comparisons for a mapping; no allocation.
    fn lookup(self, code: u16) -> Option<char> {
        let data: &[u8] = match self {
            Self::GbkCp936 => include_bytes!("mapping-data/gbk.bin"),
            Self::Big5Unicode => include_bytes!("mapping-data/big5.bin"),
            Self::ShiftJisUnicode => include_bytes!("mapping-data/shift_jis.bin"),
            Self::EucKr | Self::ReceiverEucKrC1 | Self::ReceiverEucKrGlibc239 => {
                if let Ok(byte) = u8::try_from(code) {
                    // ASCII in every profile, plus the receiver profiles' C1 controls.
                    let last = if self == Self::EucKr { 0x7f } else { 0x9f };
                    return (byte <= last).then(|| char::from(byte));
                }
                if self == Self::ReceiverEucKrGlibc239 && code == 0xa2e8 {
                    return Some('\u{327e}');
                }
                include_bytes!("mapping-data/euc_kr.bin")
            }
            Self::Windows932 => include_bytes!("mapping-data/windows932.bin"),
            Self::Windows949 => include_bytes!("mapping-data/windows949.bin"),
            Self::Windows950 => include_bytes!("mapping-data/windows950.bin"),
            Self::ReceiverBig5Glibc239 => include_bytes!("mapping-data/receiver_big5_glibc239.bin"),
            Self::ReceiverWindows932Pua => include_bytes!("mapping-data/receiver_cp932_pua.bin"),
        };
        let mut low = 0;
        let mut high = data.len() / 4;
        while low < high {
            let mid = low + (high - low) / 2;
            let at = mid * 4;
            let key = u16::from_be_bytes([data[at], data[at + 1]]);
            match key.cmp(&code) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    return char::from_u32(u32::from(u16::from_be_bytes([
                        data[at + 2],
                        data[at + 3],
                    ])));
                }
            }
        }
        None
    }

    fn lead(self, byte: u8) -> bool {
        match self {
            Self::ShiftJisUnicode => matches!(byte, 0x81..=0x9f | 0xe0..=0xef),
            Self::Windows932 | Self::ReceiverWindows932Pua => {
                matches!(byte, 0x81..=0x9f | 0xe0..=0xfc)
            }
            Self::Big5Unicode | Self::ReceiverBig5Glibc239 => matches!(byte, 0xa1..=0xf9),
            Self::EucKr | Self::ReceiverEucKrC1 | Self::ReceiverEucKrGlibc239 => {
                matches!(byte, 0xa1..=0xfe)
            }
            Self::GbkCp936 | Self::Windows949 | Self::Windows950 => matches!(byte, 0x81..=0xfe),
        }
    }

    fn trail(self, byte: u8) -> bool {
        match self {
            Self::ShiftJisUnicode | Self::Windows932 | Self::ReceiverWindows932Pua => {
                matches!(byte, 0x40..=0x7e | 0x80..=0xfc)
            }
            Self::GbkCp936 => matches!(byte, 0x40..=0x7e | 0x80..=0xfe),
            Self::Big5Unicode | Self::Windows950 | Self::ReceiverBig5Glibc239 => {
                matches!(byte, 0x40..=0x7e | 0xa1..=0xfe)
            }
            Self::EucKr | Self::ReceiverEucKrC1 | Self::ReceiverEucKrGlibc239 => {
                matches!(byte, 0xa1..=0xfe)
            }
            Self::Windows949 => matches!(byte, 0x41..=0x5a | 0x61..=0x7a | 0x81..=0xfe),
        }
    }
}

/// A decoded character and the bytes it was decoded from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedScalar {
    /// The Unicode scalar the bytes map to.
    pub value: char,
    /// Its byte range in the caller's stream, counted from the decoder's
    /// origin.
    pub range: Range<usize>,
}

/// What [`MultibyteDecoder::push`] did with a byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeStep {
    /// The byte was taken as a lead byte; push its trail byte. This is not
    /// the end of input.
    NeedInput,
    /// The byte completed a character.
    Scalar(DecodedScalar),
    /// The work budget ran out: no byte was consumed and no state changed.
    Yield,
}

/// Decodes a multibyte profile a byte at a time, holding at most one lead
/// byte and allocating nothing. Each byte costs one work unit, its table
/// lookup included, and chunk boundaries do not change the result. Only
/// [`finish`](Self::finish) ends the input: a pending lead byte is not
/// truncation before it. Errors other than cancellation are sticky.
#[derive(Debug)]
pub struct MultibyteDecoder {
    profile: MultibyteCharset,
    position: usize,
    lead: Option<u8>,
    finished: bool,
    failed: Option<Error>,
}

// Fixed inline state on both 64-bit development and 32-bit source targets.
const _: () = assert!(size_of::<MultibyteDecoder>() <= 64);
const _: () = assert!(size_of::<DecodedScalar>() <= 32);
const _: () = assert!(size_of::<DecodeStep>() <= 48);

impl MultibyteDecoder {
    /// A decoder of `profile` whose byte positions start at 0.
    pub fn new(profile: MultibyteCharset) -> Self {
        Self::at_offset(profile, 0)
    }

    /// A decoder of `profile` whose byte positions start at `offset`.
    pub fn at_offset(profile: MultibyteCharset, offset: usize) -> Self {
        Self {
            profile,
            position: offset,
            lead: None,
            finished: false,
            failed: None,
        }
    }

    /// The position of the next byte to push.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Decodes `byte`. `Error::Invalid` for input after
    /// [`finish`](Self::finish), a trail byte the profile refuses, or a byte
    /// or pair its table does not map; `Error::Overflow` when the position
    /// does not fit `usize`.
    pub fn push(
        &mut self,
        byte: u8,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DecodeStep> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !work.try_charge(1) {
            return Ok(DecodeStep::Yield);
        }
        let result = self.consume(byte);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }

    fn consume(&mut self, byte: u8) -> Result<DecodeStep> {
        if self.finished {
            return Err(Error::Invalid("multibyte input after finish"));
        }
        let next = self.position.checked_add(1).ok_or(Error::Overflow)?;
        if let Some(lead) = self.lead {
            if !self.profile.trail(byte) {
                return Err(Error::Invalid("invalid multibyte trail"));
            }
            let code = u16::from_be_bytes([lead, byte]);
            let value = self
                .profile
                .lookup(code)
                .ok_or(Error::Invalid("unmapped multibyte pair"))?;
            let start = self.position.checked_sub(1).ok_or(Error::Overflow)?;
            self.lead = None;
            self.position = next;
            return Ok(DecodeStep::Scalar(DecodedScalar {
                value,
                range: start..next,
            }));
        }
        // Historical Shift-JIS maps 0x5C to yen and 0x7E to overline.
        let single = if byte.is_ascii() {
            Some(match (self.profile, byte) {
                (MultibyteCharset::ShiftJisUnicode, b'\\') => '\u{00a5}',
                (MultibyteCharset::ShiftJisUnicode, b'~') => '\u{203e}',
                _ => char::from(byte),
            })
        } else {
            self.profile.lookup(u16::from(byte))
        };
        if let Some(value) = single {
            let start = self.position;
            self.position = next;
            return Ok(DecodeStep::Scalar(DecodedScalar {
                value,
                range: start..next,
            }));
        }
        if self.profile.lead(byte) {
            self.lead = Some(byte);
            self.position = next;
            return Ok(DecodeStep::NeedInput);
        }
        Err(Error::Invalid("unmapped multibyte byte"))
    }

    /// Ends the input. A pending lead byte is a sticky `Error::Truncated`;
    /// cancellation is resumable, and finishing again succeeds.
    pub fn finish(&mut self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.lead.is_some() {
            self.failed = Some(Error::Truncated);
            return Err(Error::Truncated);
        }
        self.finished = true;
        Ok(())
    }
}
