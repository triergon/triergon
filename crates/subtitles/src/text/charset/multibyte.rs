//! Explicit, strict, version-pinned mapping profiles. No charset detection.
//!
//! Mapping DATA and licences are described in `MULTIBYTE-CONTRACT.md`.
//! These profiles intentionally distinguish historical Unicode, Windows and
//! restricted EUC repertoires. They do not claim universal label equivalence.
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Error, Result, WorkBudget};

use super::super::unicode::cancelled;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultibyteCharset {
    /// Microsoft CP936 table 2.01, including its single-byte euro mapping.
    GbkCp936,
    /// Unicode historical Big5 table 2.0, with its seven NO MAPPING entries refused.
    Big5Unicode,
    /// Unicode historical Shift-JIS table 2.0, including yen and overline.
    ShiftJisUnicode,
    /// CP949 table 2.01 restricted to A1..FE byte pairs, plus ASCII.
    EucKr,
    Windows932,
    Windows949,
    Windows950,
    /// Opt-in freely distributable GNU Big5 charmap version 0.92, glibc 2.39 snapshot.
    ReceiverBig5Glibc239,
    /// Historical EUC-KR repertoire plus explicit single-byte C1 controls.
    ReceiverEucKrC1,
    /// Original CP932 repertoire plus Microsoft-documented 1880 user-defined scalars.
    ReceiverWindows932Pua,
    /// Explicit receiver v2: C1 repertoire plus the independently observed GNU A2E8/U327E mapping.
    ReceiverEucKrGlibc239,
}

impl MultibyteCharset {
    /// Opt-in resolver. The original `Charset::from_label` stays unchanged.
    /// Matching is ASCII case-insensitive, with no trimming or guessed aliases.
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

    fn data(self) -> &'static [u8] {
        match self {
            Self::GbkCp936 => include_bytes!("mapping-data/gbk.bin"),
            Self::Big5Unicode => include_bytes!("mapping-data/big5.bin"),
            Self::ShiftJisUnicode => include_bytes!("mapping-data/shift_jis.bin"),
            Self::EucKr => include_bytes!("mapping-data/euc_kr.bin"),
            Self::Windows932 => include_bytes!("mapping-data/windows932.bin"),
            Self::Windows949 => include_bytes!("mapping-data/windows949.bin"),
            Self::Windows950 => include_bytes!("mapping-data/windows950.bin"),
            Self::ReceiverBig5Glibc239 => include_bytes!("mapping-data/receiver_big5_glibc239.bin"),
            Self::ReceiverEucKrC1 => include_bytes!("mapping-data/receiver_euc_kr_c1.bin"),
            Self::ReceiverEucKrGlibc239 => {
                include_bytes!("mapping-data/receiver_euc_kr_glibc239.bin")
            }
            Self::ReceiverWindows932Pua => include_bytes!("mapping-data/receiver_cp932_pua.bin"),
        }
    }

    // Sorted fixed-width DATA records: two-byte wire code, two-byte scalar.
    // At most 16 comparisons for the entire 16-bit domain; no allocation.
    fn lookup(self, code: u16) -> Option<char> {
        let data = self.data();
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedScalar {
    pub value: char,
    /// Exact byte range in the caller's stream, without joining or rewriting bytes.
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeStep {
    /// The supplied byte was consumed as a lead. This is not physical EOF.
    NeedInput,
    Scalar(DecodedScalar),
    /// No byte was consumed and no state changed.
    Yield,
}

/// Allocation-free state machine, retaining at most one lead byte.
/// Each consumed byte costs one work unit, including a bounded table lookup.
/// A caller may supply bytes from any physical chunk partition without joining
/// chunks. It retains source owners, labels, epochs and transport outcomes.
/// Only explicit `finish` asserts final source EOF; starvation is not truncation.
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
    pub fn new(profile: MultibyteCharset) -> Self {
        Self::at_offset(profile, 0)
    }

    /// Use an explicit caller-provided physical origin; no source ID is inferred.
    pub fn at_offset(profile: MultibyteCharset, offset: usize) -> Self {
        Self {
            profile,
            position: offset,
            lead: None,
            finished: false,
            failed: None,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

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
        if let Some(value) = self.profile.lookup(u16::from(byte)) {
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

    /// Constant-size explicit EOF control. A partial lead is a sticky Truncated
    /// error; cancellation is resumable. Repeated successful finish is idempotent.
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
