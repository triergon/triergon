use triergon_core::{Error, Result};

/// Borrowed source view. Its owner is the charged section/descriptor Bytes.
#[derive(Clone, Copy, Debug)]
pub struct Descriptor<'a> {
    pub tag: u8,
    pub data: &'a [u8],
    pub source: &'a [u8],
}

impl Descriptor<'_> {
    /// Syntax only; private descriptor identity still depends on namespace/context.
    pub fn validate_selected_syntax(self) -> Result<()> {
        match self.tag {
            0x05 => self.registration().map(|_| ()),
            0x0a => self.languages().map(|_| ()),
            0x28 => self.avc_video().map(|_| ()),
            0x38 => self.hevc_video().map(|_| ()),
            _ => Ok(()),
        }
    }
    /// Only use after the format coordinator establishes a DVB namespace.
    pub fn validate_dvb_audio_syntax(self) -> Result<()> {
        match self.tag {
            0x6a => self.dvb_ac3().map(|_| ()),
            0x7a => self.dvb_eac3().map(|_| ()),
            _ => Ok(()),
        }
    }
}

#[derive(Debug)]
pub struct Descriptors<'a> {
    remaining: &'a [u8],
    failed: bool,
}
impl<'a> Descriptors<'a> {
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
            return Some(Err(Error::Invalid("descriptor length")));
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
