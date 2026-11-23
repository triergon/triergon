use super::descriptor_tag;
use triergon_core::{Error, Result};

/// A descriptor that overruns its loop.
pub(crate) const DESCRIPTOR_LENGTH: &str = "descriptor length";

/// One descriptor of a descriptor loop, borrowed from the section's bytes.
#[derive(Clone, Copy, Debug)]
pub struct Descriptor<'a> {
    /// The 8-bit descriptor tag.
    pub tag: u8,
    /// The descriptor body after the tag and length bytes.
    pub data: &'a [u8],
    /// The whole descriptor: tag, length and body.
    pub source: &'a [u8],
}

impl Descriptor<'_> {
    /// Validate the body of a registration, language, AVC or HEVC descriptor.
    /// Invalid lengths and fields return `Invalid`. Other tags remain opaque.
    pub fn validate_selected_syntax(self) -> Result<()> {
        match self.tag {
            descriptor_tag::REGISTRATION => self.registration().map(|_| ()),
            descriptor_tag::ISO_639_LANGUAGE => self.languages().map(|_| ()),
            descriptor_tag::AVC_VIDEO => self.avc_video().map(|_| ()),
            descriptor_tag::HEVC_VIDEO => self.hevc_video().map(|_| ()),
            _ => Ok(()),
        }
    }
    /// Check DVB tags 0x6A and 0x7A for the fields their flags require. Missing fields
    /// return `Invalid`. This check is for DVB loops. All other tags succeed.
    pub fn validate_dvb_audio_syntax(self) -> Result<()> {
        match self.tag {
            descriptor_tag::DVB_AC3 => self.dvb_ac3().map(|_| ()),
            descriptor_tag::DVB_EAC3 => self.dvb_eac3().map(|_| ()),
            _ => Ok(()),
        }
    }
}

/// Iterator over the descriptors of a descriptor loop, in source order.
///
/// Yields one `Invalid` error and stops when a descriptor header or body overruns the loop.
#[derive(Debug)]
pub struct Descriptors<'a> {
    remaining: &'a [u8],
    failed: bool,
}
impl<'a> Descriptors<'a> {
    /// Iterates the descriptors in the loop bytes `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            remaining: data,
            failed: false,
        }
    }
}
impl<'a> Iterator for Descriptors<'a> {
    type Item = Result<Descriptor<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining.is_empty() {
            return None;
        }
        if self.remaining.len() < 2 {
            self.failed = true;
            return Some(Err(Error::Invalid("descriptor header")));
        }
        let end = 2 + usize::from(self.remaining[1]);
        if end > self.remaining.len() {
            self.failed = true;
            return Some(Err(Error::Invalid(DESCRIPTOR_LENGTH)));
        }
        let source = &self.remaining[..end];
        self.remaining = &self.remaining[end..];
        Some(Ok(Descriptor {
            tag: source[0],
            data: &source[2..],
            source,
        }))
    }
}
