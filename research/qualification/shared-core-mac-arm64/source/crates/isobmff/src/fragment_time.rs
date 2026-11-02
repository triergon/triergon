//! ETSI TS 126 244 V18.0.0 (2024-05), §13.5. Source media decode time,
//! before edits. This supplies the tfdt field basis without claiming full ISO.
use crate::{BoxView, fields};
use safe_media_core::{Error, Result};

#[derive(Clone, Debug)]
pub struct FragmentDecodeTime {
    raw: BoxView,
    version: u8,
    ticks: u64,
}
impl FragmentDecodeTime {
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
    pub fn version(&self) -> u8 {
        self.version
    }
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
}
