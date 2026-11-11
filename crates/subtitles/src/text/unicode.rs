//! Original byte-resumable scalar decoding from RFC 3629 / RFC 2781.
use super::{
    Encoding,
    charset::multibyte::{DecodeStep, MultibyteDecoder},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Debug)]
pub(crate) enum Poll<T> {
    Ready(T),
    End,
    Yield,
}

pub(crate) fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Scalar {
    pub value: char,
    pub start: usize,
    pub end: usize,
    pub decoded_end: usize,
}

#[derive(Debug)]
pub(crate) struct Cursor {
    at: usize,
    end: usize,
    encoding: Encoding,
    scalar_start: usize,
    value: u32,
    minimum: u32,
    continuation: u8,
    unit_first: Option<u8>,
    high: Option<u16>,
    decoded: usize,
    multibyte: Option<MultibyteDecoder>,
}

impl Cursor {
    pub(super) fn new(range: Range<usize>, encoding: Encoding) -> Self {
        Self {
            at: range.start,
            end: range.end,
            encoding,
            scalar_start: range.start,
            value: 0,
            minimum: 0,
            continuation: 0,
            unit_first: None,
            high: None,
            decoded: 0,
            multibyte: match encoding {
                Encoding::Multibyte(profile) => {
                    Some(MultibyteDecoder::at_offset(profile, range.start))
                }
                _ => None,
            },
        }
    }

    pub(super) fn decoded_len(&self) -> usize {
        self.decoded
    }

    fn scalar(&mut self, value: u32) -> Result<Scalar> {
        let value = char::from_u32(value).ok_or(Error::Invalid("invalid Unicode scalar"))?;
        self.decoded = self
            .decoded
            .checked_add(value.len_utf8())
            .ok_or(Error::Overflow)?;
        Ok(Scalar {
            value,
            start: self.scalar_start,
            end: self.at,
            decoded_end: self.decoded,
        })
    }

    fn poll_multibyte(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Scalar>> {
        loop {
            cancelled(cancel)?;
            let decoder = self
                .multibyte
                .as_mut()
                .ok_or(Error::Invalid("multibyte cursor absent"))?;
            if self.at == self.end {
                decoder.finish(cancel)?;
                return Ok(Poll::End);
            }
            let byte = *bytes
                .get(self.at)
                .ok_or(Error::Invalid("multibyte source range"))?;
            match decoder.push(byte, work, cancel)? {
                DecodeStep::Yield => return Ok(Poll::Yield),
                DecodeStep::NeedInput => self.at = decoder.position(),
                DecodeStep::Scalar(scalar) => {
                    self.at = scalar.range.end;
                    self.scalar_start = scalar.range.start;
                    return self.scalar(u32::from(scalar.value)).map(Poll::Ready);
                }
            }
        }
    }

    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Scalar>> {
        if self.multibyte.is_some() {
            return self.poll_multibyte(bytes, work, cancel);
        }
        loop {
            cancelled(cancel)?;
            if self.at == self.end {
                if self.continuation != 0 || self.unit_first.is_some() || self.high.is_some() {
                    return Err(Error::Invalid("incomplete Unicode scalar"));
                }
                return Ok(Poll::End);
            }
            if !work.try_charge(1) {
                return Ok(Poll::Yield);
            }
            let byte = *bytes
                .get(self.at)
                .ok_or(Error::Invalid("Unicode source range"))?;
            let byte_at = self.at;
            self.at = self.at.checked_add(1).ok_or(Error::Overflow)?;
            match self.encoding {
                Encoding::Utf8 => {
                    if self.continuation != 0 {
                        if byte & 0xc0 != 0x80 {
                            return Err(Error::Invalid("UTF-8 continuation"));
                        }
                        self.value = (self.value << 6) | u32::from(byte & 0x3f);
                        self.continuation -= 1;
                        if self.continuation == 0 {
                            if self.value < self.minimum {
                                return Err(Error::Invalid("overlong UTF-8"));
                            }
                            return self.scalar(self.value).map(Poll::Ready);
                        }
                    } else {
                        self.scalar_start = byte_at;
                        match byte {
                            0..=0x7f => return self.scalar(u32::from(byte)).map(Poll::Ready),
                            0xc2..=0xdf => {
                                self.value = u32::from(byte & 0x1f);
                                self.minimum = 0x80;
                                self.continuation = 1;
                            }
                            0xe0..=0xef => {
                                self.value = u32::from(byte & 0x0f);
                                self.minimum = 0x800;
                                self.continuation = 2;
                            }
                            0xf0..=0xf4 => {
                                self.value = u32::from(byte & 0x07);
                                self.minimum = 0x10000;
                                self.continuation = 3;
                            }
                            _ => return Err(Error::Invalid("UTF-8 lead byte")),
                        }
                    }
                }
                Encoding::Utf16Be | Encoding::Utf16Le => {
                    let Some(first) = self.unit_first.take() else {
                        if self.high.is_none() {
                            self.scalar_start = byte_at;
                        }
                        self.unit_first = Some(byte);
                        continue;
                    };
                    let unit = if self.encoding == Encoding::Utf16Be {
                        u16::from_be_bytes([first, byte])
                    } else {
                        u16::from_le_bytes([first, byte])
                    };
                    if let Some(high) = self.high.take() {
                        if !(0xdc00..=0xdfff).contains(&unit) {
                            return Err(Error::Invalid("UTF-16 surrogate pair"));
                        }
                        let value =
                            0x10000 + (u32::from(high - 0xd800) << 10) + u32::from(unit - 0xdc00);
                        return self.scalar(value).map(Poll::Ready);
                    }
                    match unit {
                        0xd800..=0xdbff => self.high = Some(unit),
                        0xdc00..=0xdfff => {
                            return Err(Error::Invalid("unpaired UTF-16 low surrogate"));
                        }
                        _ => return self.scalar(u32::from(unit)).map(Poll::Ready),
                    }
                }
                _ => return Err(Error::Invalid("unresolved character encoding")),
            }
        }
    }
}

/// Only the at-most-two-byte encoding selector is examined here. Its cursor
/// survives work=1; the subsequent scalar cursor performs actual validation.
#[derive(Debug)]
pub(crate) struct Probe {
    range: Range<usize>,
    requested: Encoding,
    bytes: [u8; 2],
    read: usize,
}

impl Probe {
    pub(super) fn new(range: Range<usize>, requested: Encoding) -> Self {
        Self {
            range,
            requested,
            bytes: [0; 2],
            read: 0,
        }
    }
    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<(Encoding, Range<usize>)>> {
        match self.requested {
            Encoding::Utf8 | Encoding::Utf16Be | Encoding::Utf16Le | Encoding::Multibyte(_) => {
                return Ok(Poll::Ready((self.requested, self.range.clone())));
            }
            Encoding::Opaque => return Err(Error::Unsupported("declared charset conversion")),
            Encoding::Utf16Bom | Encoding::Tx3gInBand => {}
        }
        let maximum = (self.range.end - self.range.start).min(2);
        while self.read < maximum {
            cancelled(cancel)?;
            if !work.try_charge(1) {
                return Ok(Poll::Yield);
            }
            self.bytes[self.read] = *bytes
                .get(self.range.start + self.read)
                .ok_or(Error::Invalid("encoding selector range"))?;
            self.read += 1;
        }
        let resolved = match (self.requested, self.read, self.bytes) {
            (_, 2, [0xfe, 0xff]) => (Encoding::Utf16Be, self.range.start + 2..self.range.end),
            (Encoding::Utf16Bom, 2, [0xff, 0xfe]) => {
                (Encoding::Utf16Le, self.range.start + 2..self.range.end)
            }
            (Encoding::Utf16Bom, _, _) => return Err(Error::Invalid("UTF-16 BOM required")),
            (Encoding::Tx3gInBand, _, _) => (Encoding::Utf8, self.range.clone()),
            _ => return Err(Error::Invalid("encoding selector")),
        };
        Ok(Poll::Ready(resolved))
    }
}
