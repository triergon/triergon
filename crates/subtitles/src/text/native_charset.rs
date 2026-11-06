//! Explicit receiver view for the pinned D32/Linux charset observations.
//!
//! Source remains OpaqueAss and retains every byte. This opt-in operation does
//! not render ASS, reproduce decoder return codes, or claim a universal alias.
use super::charset::{
    Charset, CharsetJob, CharsetStatus, ConvertedText, multibyte::MultibyteCharset as M,
};
use super::unicode::cancelled;
use super::{Cue, Encoding, Event, Extractor, Format, Input, Limits, Status};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, Result, WorkBudget};

/// Deliberately separate from both original strict label resolvers. Availability
/// refers to the captured Linux/glibc receiver, not other libc installations.
pub fn receiver_charset(label: &[u8]) -> Result<Charset> {
    if label.len() > 256 {
        return Err(Error::Limit {
            requested: label.len(),
            maximum: 256,
        });
    }
    for (name, profile) in [
        ("gbk", M::GbkCp936),
        ("cp936", M::GbkCp936),
        ("windows-936", M::GbkCp936),
        ("big5", M::ReceiverBig5Glibc239),
        ("big-5", M::ReceiverBig5Glibc239),
        ("shift_jis", M::ShiftJisUnicode),
        ("shift-jis", M::ShiftJisUnicode),
        ("sjis", M::ShiftJisUnicode),
        ("euc-kr", M::ReceiverEucKrC1),
        ("cp932", M::ReceiverWindows932Pua),
        ("cp949", M::Windows949),
        ("cp950", M::Windows950),
    ] {
        if label.eq_ignore_ascii_case(name.as_bytes()) {
            return Ok(Charset::Multibyte(profile));
        }
    }
    Err(Error::Unsupported("captured receiver charset label"))
}
/// Explicit receiver v2 adds the observed GNU EUC-KR A2E8 mapping. The
/// v1 resolver and constructor keep their repertoire/refusals.
pub fn receiver_charset_v2(label: &[u8]) -> Result<Charset> {
    if label.eq_ignore_ascii_case(b"euc-kr") {
        Ok(Charset::Multibyte(M::ReceiverEucKrGlibc239))
    } else {
        receiver_charset(label)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Converted,
    UnsupportedLabel,
    ConversionRefused(Error),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketStatus {
    Pending,
    Ready,
}
/// Full converted owner and source owner are retained even when the receiver's
/// C-string prefix ends earlier. Text is the suffix after eight literal commas;
/// its presence does not validate ASS fields or assert native output emission.
#[derive(Debug)]
pub struct PacketView {
    source: Cue,
    outcome: Outcome,
    converted: Option<ConvertedText>,
    visible: Range<usize>,
    text: Option<Range<usize>>,
}
impl PacketView {
    pub fn source(&self) -> &Cue {
        &self.source
    }
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }
    pub fn converted(&self) -> Option<&ConvertedText> {
        self.converted.as_ref()
    }
    pub fn visible(&self) -> Option<&[u8]> {
        self.converted
            .as_ref()
            .map(|c| &c.utf8.as_ref()[self.visible.clone()])
    }
    pub fn text(&self) -> Option<&[u8]> {
        self.text
            .as_ref()
            .and_then(|r| self.converted.as_ref().map(|c| &c.utf8.as_ref()[r.clone()]))
    }
}
/// One complete physical ASS packet, not a transport or a streaming document.
/// Construction checks every source owner through Extractor; a fabricated Cue
/// cannot act as an already-validated input. Backpressure and cancel are resumable.
#[derive(Debug)]
pub struct AssPacketJob {
    extractor: Extractor,
    source: Option<Cue>,
    pool: Budget,
    limits: Limits,
    converter: Option<CharsetJob>,
    converted: Option<ConvertedText>,
    setup: bool,
    receiver_v2: bool,
    at: usize,
    commas: u8,
    text_at: Option<usize>,
    visible_end: Option<usize>,
    output: Option<PacketView>,
    taken: bool,
}
impl AssPacketJob {
    pub fn new(input: Input, pool: &Budget, limits: Limits, generation: u64) -> Result<Self> {
        if input.format != Format::OpaqueAss || input.encoding != Encoding::Opaque {
            return Err(Error::Unsupported("receiver requires opaque ASS packet"));
        }
        let mut extractor = Extractor::new(pool, limits, generation);
        extractor.provide(input).map_err(|e| e.error)?;
        Ok(Self {
            extractor,
            source: None,
            pool: pool.clone(),
            limits,
            converter: None,
            converted: None,
            setup: false,
            receiver_v2: false,
            at: 0,
            commas: 0,
            text_at: None,
            visible_end: None,
            output: None,
            taken: false,
        })
    }
    /// Opt-in version2; no implicit upgrade of the original receiver operation.
    pub fn new_v2(input: Input, pool: &Budget, limits: Limits, generation: u64) -> Result<Self> {
        let mut result = Self::new(input, pool, limits, generation)?;
        result.receiver_v2 = true;
        Ok(result)
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<PacketStatus> {
        cancelled(cancel)?;
        if self.taken {
            return Err(Error::Invalid("receiver output already taken"));
        }
        if self.output.is_some() {
            return Ok(PacketStatus::Ready);
        }
        if work.remaining() == 0 {
            return Ok(PacketStatus::Pending);
        }
        if self.source.is_none() {
            match self.extractor.step(work, cancel)? {
                Status::Event(Event::Cue(cue)) => self.source = Some(cue),
                Status::Yield | Status::NeedOutput => return Ok(PacketStatus::Pending),
                _ => return Err(Error::Invalid("receiver expected opaque cue")),
            }
        }
        if !self.setup {
            if !work.try_charge(1) {
                return Ok(PacketStatus::Pending);
            }
            let source = self
                .source
                .as_ref()
                .ok_or(Error::Invalid("missing source cue"))?;
            let charset = match source.input.charset_label.as_ref() {
                Some(label) => {
                    if self.receiver_v2 {
                        receiver_charset_v2(label.as_ref())
                    } else {
                        receiver_charset(label.as_ref())
                    }
                }
                None => Err(Error::Unsupported("receiver charset declaration absent")),
            };
            match charset {
                Err(Error::Unsupported(_)) => {
                    self.publish(Outcome::UnsupportedLabel)?;
                    return Ok(PacketStatus::Ready);
                }
                Err(e) => return Err(e),
                Ok(charset) => {
                    self.converter = Some(CharsetJob::new(
                        &source.input.payload,
                        source.input.charset_label.as_ref(),
                        charset,
                        &self.pool,
                        self.limits,
                    )?);
                }
            }
            self.setup = true;
        }
        if self.converted.is_none() {
            let converter = self
                .converter
                .as_mut()
                .ok_or(Error::Invalid("missing receiver converter"))?;
            match converter.poll(work, cancel) {
                Ok(CharsetStatus::Pending) => return Ok(PacketStatus::Pending),
                Ok(CharsetStatus::Ready) => {
                    self.converted = Some(converter.take_output(cancel)?);
                    self.converter = None;
                }
                Err(e @ (Error::Cancelled | Error::Backpressure { .. })) => return Err(e),
                Err(e) => {
                    self.converter = None;
                    self.publish(Outcome::ConversionRefused(e))?;
                    return Ok(PacketStatus::Ready);
                }
            }
        }
        let bytes = self
            .converted
            .as_ref()
            .ok_or(Error::Invalid("missing receiver bytes"))?
            .utf8
            .as_ref();
        while self.at < bytes.len() {
            cancelled(cancel)?;
            if !work.try_charge(1) {
                return Ok(PacketStatus::Pending);
            }
            let b = bytes[self.at];
            if b == 0 {
                self.visible_end = Some(self.at);
                break;
            }
            if b == b',' && self.commas < 8 {
                self.commas += 1;
                if self.commas == 8 {
                    self.text_at = Some(self.at + 1);
                }
            }
            self.at += 1;
        }
        self.publish(Outcome::Converted)?;
        Ok(PacketStatus::Ready)
    }
    fn publish(&mut self, outcome: Outcome) -> Result<()> {
        let end = self.visible_end.unwrap_or(self.at);
        self.output = Some(PacketView {
            source: self
                .source
                .take()
                .ok_or(Error::Invalid("missing source cue"))?,
            outcome,
            converted: self.converted.take(),
            visible: 0..end,
            text: self.text_at.map(|start| start..end),
        });
        Ok(())
    }
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<PacketView> {
        cancelled(cancel)?;
        let out = self
            .output
            .take()
            .ok_or(Error::Invalid("receiver output not ready"))?;
        self.taken = true;
        Ok(out)
    }
}
