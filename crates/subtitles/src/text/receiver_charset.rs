//! ASS packets converted with the charset labels FFmpeg accepts through
//! Linux/glibc `iconv`, re-exported as [`crate::compat::charset`].
//!
//! The source stays an [`Format::OpaqueAss`] cue with every byte. This
//! operation does not render ASS or reproduce decoder return codes, and its
//! labels hold for that repertoire only.
use super::charset::{
    Charset, CharsetJob, CharsetStatus, ConvertedText, multibyte::MultibyteCharset as M,
};
use super::unicode::cancelled;
use super::{Cue, Encoding, Extractor, Format, Input, Limits, Status};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, Result, WorkBudget};

/// The charset FFmpeg with Linux/glibc `iconv` converts for `label`, matched
/// ASCII case-insensitively: gbk, cp936, windows-936, big5, big-5,
/// shift_jis, shift-jis, sjis, euc-kr, cp932, cp949, cp950, latin1,
/// iso-8859-1, windows-1251 or windows-1252. `Error::Limit` for a label over
/// 256 bytes, `Error::Unsupported` for any other. It differs from
/// [`Charset::from_label`] and
/// [`MultibyteCharset::from_label`](M::from_label): big5, euc-kr and cp932
/// resolve to the `Receiver` profiles.
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
    // The single-byte labels: Latin-1 keeps 0x80 to 0x9F as the C1 controls,
    // and the Windows code pages refuse the bytes their tables leave undefined.
    for (name, charset) in [
        ("latin1", Charset::Latin1),
        ("iso-8859-1", Charset::Latin1),
        ("windows-1251", Charset::Windows1251),
        ("windows-1252", Charset::Windows1252),
    ] {
        if label.eq_ignore_ascii_case(name.as_bytes()) {
            return Ok(charset);
        }
    }
    Err(Error::Unsupported("captured receiver charset label"))
}
/// [`receiver_charset`], with euc-kr resolved to
/// [`MultibyteCharset::ReceiverEucKrGlibc239`](M::ReceiverEucKrGlibc239), the
/// same repertoire as the profile `receiver_charset` gives.
pub fn receiver_charset_with_glibc_euc_kr(label: &[u8]) -> Result<Charset> {
    if label.eq_ignore_ascii_case(b"euc-kr") {
        Ok(Charset::Multibyte(M::ReceiverEucKrGlibc239))
    } else {
        receiver_charset(label)
    }
}
/// How a packet's conversion ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The packet converted to UTF-8.
    Converted,
    /// The packet has no charset label, or one the repertoire lacks; nothing
    /// was converted.
    UnsupportedLabel,
    /// The conversion failed with this error, neither cancellation nor
    /// backpressure; nothing was converted.
    ConversionRefused(Error),
}
/// What [`AssPacketJob::poll`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketStatus {
    /// The work budget ran out, or the pool could not hold the cue; poll
    /// again.
    Pending,
    /// The view is ready; take it with
    /// [`take_output`](AssPacketJob::take_output).
    Ready,
}
/// A converted ASS packet: its source cue, the outcome, the whole converted
/// text, the part before its first NUL, which FFmpeg keeps, and the text
/// after the eighth comma. The ASS fields are not validated.
#[derive(Debug)]
pub struct PacketView {
    source: Cue,
    outcome: Outcome,
    converted: Option<ConvertedText>,
    visible: Range<usize>,
    text: Option<Range<usize>>,
}
impl PacketView {
    /// The cue the packet was read into, its input unchanged.
    pub fn source(&self) -> &Cue {
        &self.source
    }
    /// How the conversion ended.
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }
    /// The whole conversion, NULs included; `None` unless converted.
    pub fn converted(&self) -> Option<&ConvertedText> {
        self.converted.as_ref()
    }
    /// The converted text before its first NUL; `None` unless converted.
    pub fn visible(&self) -> Option<&[u8]> {
        self.converted
            .as_ref()
            .map(|c| &c.utf8.as_ref()[self.visible.clone()])
    }
    /// The visible text after its eighth comma, the event's `Text` field;
    /// `None` with fewer than eight commas or nothing converted.
    pub fn text(&self) -> Option<&[u8]> {
        self.text
            .as_ref()
            .and_then(|r| self.converted.as_ref().map(|c| &c.utf8.as_ref()[r.clone()]))
    }
}
/// Converts one complete ASS packet with its charset label, as FFmpeg's
/// `sub_charenc` conversion does, into a [`PacketView`]. The input is
/// checked as [`Extractor::provide`] checks it. Backpressure and
/// cancellation are resumable.
#[derive(Debug)]
pub struct AssPacketJob {
    pool: Budget,
    limits: Limits,
    glibc_euc_kr: bool,
    state: State,
}

#[derive(Debug)]
enum State {
    /// Reading the packet into its cue.
    Extract(Extractor),
    /// The cue, and how far its conversion has got.
    Packet(Cue, Step),
    /// The view, ready to take.
    Ready(PacketView),
    /// The view was taken.
    Taken,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "The step is advanced in place; the conversion stays inline rather than in an allocation the pool does not charge."
)]
enum Step {
    /// The charset label is not resolved yet.
    Label,
    /// Converting the payload.
    Convert(CharsetJob),
    /// Scanning the conversion for its first NUL and its eighth comma.
    Scan {
        converted: ConvertedText,
        at: usize,
        commas: u8,
        text_at: Option<usize>,
    },
}

impl AssPacketJob {
    /// A job converting `input`, whose label resolves with
    /// [`receiver_charset`]. `Error::Unsupported` unless the input is
    /// [`Format::OpaqueAss`] with [`Encoding::Opaque`]; an input that
    /// [`Extractor::provide`] refuses for `generation` and `limits` fails
    /// with its error.
    pub fn new(input: Input, pool: &Budget, limits: Limits, generation: u64) -> Result<Self> {
        if input.format != Format::OpaqueAss || input.encoding != Encoding::Opaque {
            return Err(Error::Unsupported("receiver requires opaque ASS packet"));
        }
        let mut extractor = Extractor::new(pool, limits, generation);
        extractor.provide(input).map_err(|e| e.error)?;
        Ok(Self {
            pool: pool.clone(),
            limits,
            glibc_euc_kr: false,
            state: State::Extract(extractor),
        })
    }
    /// [`new`](Self::new), with the label resolved by
    /// [`receiver_charset_with_glibc_euc_kr`].
    pub fn new_with_glibc_euc_kr(
        input: Input,
        pool: &Budget,
        limits: Limits,
        generation: u64,
    ) -> Result<Self> {
        let mut result = Self::new(input, pool, limits, generation)?;
        result.glibc_euc_kr = true;
        Ok(result)
    }
    /// Reads, converts and scans the packet, spending `work`: the
    /// conversion's units and one per converted byte scanned for commas and
    /// NUL. `Error::Invalid` once the view is taken.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<PacketStatus> {
        cancelled(cancel)?;
        match &self.state {
            State::Taken => return Err(Error::Invalid("receiver output already taken")),
            State::Ready(_) => return Ok(PacketStatus::Ready),
            State::Extract(_) | State::Packet(..) => {}
        }
        if work.remaining() == 0 {
            return Ok(PacketStatus::Pending);
        }
        let outcome = loop {
            match &mut self.state {
                State::Extract(extractor) => match extractor.step(work, cancel)? {
                    Status::Cue(cue) => self.state = State::Packet(cue, Step::Label),
                    Status::Yield | Status::NeedOutput => return Ok(PacketStatus::Pending),
                    _ => return Err(Error::Invalid("receiver expected opaque cue")),
                },
                State::Packet(source, step) => {
                    let label = Label {
                        pool: &self.pool,
                        limits: self.limits,
                        glibc_euc_kr: self.glibc_euc_kr,
                    };
                    match step.advance(source, label, work, cancel)? {
                        Some(outcome) => break outcome,
                        None => return Ok(PacketStatus::Pending),
                    }
                }
                State::Ready(_) => return Ok(PacketStatus::Ready),
                State::Taken => return Err(Error::Invalid("receiver output already taken")),
            }
        };
        if let State::Packet(source, step) = std::mem::replace(&mut self.state, State::Taken) {
            self.state = State::Ready(PacketView::new(source, outcome, step));
        }
        Ok(PacketStatus::Ready)
    }
    /// The view, once [`poll`](Self::poll) reported
    /// [`PacketStatus::Ready`]; `Error::Invalid` before then and once taken.
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<PacketView> {
        cancelled(cancel)?;
        match std::mem::replace(&mut self.state, State::Taken) {
            State::Ready(view) => Ok(view),
            state => {
                self.state = state;
                Err(Error::Invalid("receiver output not ready"))
            }
        }
    }
}

/// How a packet's charset label resolves, and what its conversion may use.
#[derive(Clone, Copy)]
struct Label<'a> {
    pool: &'a Budget,
    limits: Limits,
    glibc_euc_kr: bool,
}

impl Step {
    /// Advances the conversion of `source`; the outcome once it has ended,
    /// `None` when `work` runs out first.
    fn advance(
        &mut self,
        source: &Cue,
        label: Label<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Outcome>> {
        loop {
            match self {
                Self::Label => {
                    if !work.try_charge(1) {
                        return Ok(None);
                    }
                    let charset = match source.input.charset_label.as_ref() {
                        Some(name) if label.glibc_euc_kr => {
                            receiver_charset_with_glibc_euc_kr(name.as_ref())
                        }
                        Some(name) => receiver_charset(name.as_ref()),
                        None => Err(Error::Unsupported("receiver charset declaration absent")),
                    };
                    let charset = match charset {
                        Err(Error::Unsupported(_)) => return Ok(Some(Outcome::UnsupportedLabel)),
                        result => result?,
                    };
                    *self = Self::Convert(CharsetJob::new(
                        &source.input.payload,
                        source.input.charset_label.as_ref(),
                        charset,
                        label.pool,
                        label.limits,
                    )?);
                }
                Self::Convert(converter) => match converter.poll(work, cancel) {
                    Ok(CharsetStatus::Pending) => return Ok(None),
                    Ok(CharsetStatus::Ready) => {
                        *self = Self::Scan {
                            converted: converter.take_output(cancel)?,
                            at: 0,
                            commas: 0,
                            text_at: None,
                        };
                    }
                    Err(e @ (Error::Cancelled | Error::Backpressure { .. })) => return Err(e),
                    Err(e) => return Ok(Some(Outcome::ConversionRefused(e))),
                },
                Self::Scan {
                    converted,
                    at,
                    commas,
                    text_at,
                } => {
                    let bytes = converted.utf8.as_ref();
                    while *at < bytes.len() {
                        cancelled(cancel)?;
                        if !work.try_charge(1) {
                            return Ok(None);
                        }
                        let byte = bytes[*at];
                        if byte == 0 {
                            break;
                        }
                        if byte == b',' && *commas < 8 {
                            *commas += 1;
                            if *commas == 8 {
                                *text_at = Some(*at + 1);
                            }
                        }
                        *at += 1;
                    }
                    return Ok(Some(Outcome::Converted));
                }
            }
        }
    }
}

impl PacketView {
    /// The view of `source` once its conversion ended with `outcome`; the
    /// visible text ends where the scan stopped, at the first NUL or the end.
    fn new(source: Cue, outcome: Outcome, step: Step) -> Self {
        let (converted, end, text_at) = match step {
            Step::Scan {
                converted,
                at,
                text_at,
                ..
            } => (Some(converted), at, text_at),
            Step::Label | Step::Convert(_) => (None, 0, None),
        };
        Self {
            source,
            outcome,
            converted,
            visible: 0..end,
            text: text_at.map(|start| start..end),
        }
    }
}
