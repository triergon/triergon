//! The track fragment decode time box `tfdt`.
//!
//! Specification: ETSI TS 126 244 V18.0.0 (2024-05) §13.5.
use crate::{boxes::BoxView, fields};
use triergon_core::{Error, Result};

/// A `tfdt` box: the decode time of a fragment's first sample in ticks of the media
/// timescale, before any edit.
#[derive(Clone, Debug)]
pub struct FragmentDecodeTime {
    raw: BoxView,
    version: u8,
    ticks: u64,
}
impl FragmentDecodeTime {
    /// Reads a version 0 (32-bit) or 1 (64-bit) `tfdt`. `Error::Invalid` for another box
    /// type or trailing bytes, `Error::Unsupported` for a later version or nonzero flags.
    pub fn parse(raw: BoxView) -> Result<Self> {
        if raw.header().kind != *b"tfdt" {
            return Err(Error::Invalid("not tfdt"));
        }
        let b = raw.payload();
        let (version, flags) = fields::full(b)?;
        if flags != 0 || version > 1 {
            return Err(Error::Unsupported("tfdt version/flags"));
        }
        fields::exact_rows(b, if version == 0 { 8 } else { 12 }, 0, 0)?;
        let ticks = if version == 0 {
            u64::from(fields::u32_at(b, 4)?)
        } else {
            fields::u64_at(b, 4)?
        };
        Ok(Self {
            raw,
            version,
            ticks,
        })
    }
    /// The box version: 0 or 1.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// The decode time in ticks of the media timescale.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
    /// The `tfdt` box as read.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
}
