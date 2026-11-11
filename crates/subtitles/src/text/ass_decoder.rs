//! ASS packets decoded as FFmpeg's `ass` decoder decodes them through
//! `avcodec_decode_subtitle2`, re-exported as [`crate::compat::ass`].
//!
//! The decoder opens with the codec configuration, which is the subtitle
//! header, and optionally a charset label (`sub_charenc`); a label the
//! receiver's repertoire lacks ([`receiver_charset_with_glibc_euc_kr`])
//! refuses the open. Each packet then gives at most one subtitle:
//!
//! - an empty packet gives none;
//! - with a label the packet is converted to UTF-8, and a refused conversion
//!   gives none;
//! - the text is the packet up to its first NUL;
//! - text that is not UTF-8, or that holds U+FFFE, gives none.

use super::Limits;
use super::charset::{Charset, CharsetJob, CharsetStatus};
use super::decoded::{Clock, Subtitle};
use super::receiver_charset::receiver_charset_with_glibc_euc_kr;
use super::unicode::cancelled;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, TimeBase, WorkBudget};

/// The decoder options that change its output.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// The packet time base; without one a subtitle has no PTS or duration.
    pub packet_timebase: Option<TimeBase>,
}

/// What [`Decoder::poll`] produced.
#[derive(Debug)]
pub enum Status {
    /// The work budget ran out; poll again.
    Pending,
    /// The packet is decoded, to a subtitle or to none.
    Decoded(Option<Subtitle>),
    /// No packet is being decoded.
    NeedInput,
}

/// FFmpeg's `ass` decoder.
#[derive(Debug)]
pub struct Decoder {
    pool: Budget,
    limits: Limits,
    options: Options,
    header: Option<Bytes>,
    charset: Option<(Bytes, Charset)>,
    /// The clock of the packet being decoded.
    clock: Clock,
    /// Converting the packet from the label's charset.
    converter: Option<CharsetJob>,
    /// Checking the text, with how many of its bytes are paid for.
    check: Option<(Bytes, usize)>,
    /// The packet is decoded to no subtitle.
    nothing: bool,
}

impl Decoder {
    /// Opens the decoder with the codec configuration `header` and the
    /// charset `label`; `Error::Unsupported` for a label the receiver lacks.
    pub fn new(
        pool: &Budget,
        header: Option<&Bytes>,
        label: Option<&Bytes>,
        options: Options,
        limits: Limits,
    ) -> Result<Self> {
        let charset = label
            .map(|label| {
                Ok::<_, Error>((
                    label.clone(),
                    receiver_charset_with_glibc_euc_kr(label.as_ref())?,
                ))
            })
            .transpose()?;
        Ok(Self {
            pool: pool.clone(),
            limits,
            options,
            header: header.cloned(),
            charset,
            clock: Clock::default(),
            converter: None,
            check: None,
            nothing: false,
        })
    }

    /// The subtitle header: the codec configuration, or nothing.
    pub fn header(&self) -> &[u8] {
        self.header.as_ref().map_or(&[], Bytes::as_ref)
    }

    /// Starts decoding `packet`; [`poll`](Self::poll) until it is decoded.
    pub fn provide(&mut self, packet: &Bytes, clock: Clock) -> Result<()> {
        if self.converter.is_some() || self.check.is_some() || self.nothing {
            return Err(Error::Invalid("a packet is already being decoded"));
        }
        self.clock = clock;
        match &self.charset {
            _ if packet.is_empty() => self.nothing = true,
            Some((label, charset)) => {
                self.converter = Some(CharsetJob::new(
                    packet,
                    Some(label),
                    *charset,
                    &self.pool,
                    self.limits,
                )?);
            }
            None => self.check = Some((packet.clone(), 0)),
        }
        Ok(())
    }

    /// Advances the packet being decoded; each byte converted or checked
    /// costs one unit.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        cancelled(cancel)?;
        if std::mem::take(&mut self.nothing) {
            return Ok(Status::Decoded(None));
        }
        if let Some(converter) = self.converter.as_mut() {
            match converter.poll(work, cancel) {
                Ok(CharsetStatus::Pending) => return Ok(Status::Pending),
                Ok(CharsetStatus::Ready) => {
                    self.check = Some((converter.take_output(cancel)?.utf8, 0));
                    self.converter = None;
                }
                Err(error @ (Error::Cancelled | Error::Backpressure { .. })) => return Err(error),
                Err(_) => {
                    self.converter = None;
                    return Ok(Status::Decoded(None));
                }
            }
        }
        let Some((text, paid)) = self.check.as_mut() else {
            return Ok(Status::NeedInput);
        };
        *paid +=
            usize::try_from(work.take((text.len() - *paid) as u64)).map_err(|_| Error::Overflow)?;
        if *paid < text.len() {
            return Ok(Status::Pending);
        }
        let Some((text, _)) = self.check.take() else {
            return Ok(Status::NeedInput);
        };
        let end = text
            .as_ref()
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(text.len());
        let valid =
            std::str::from_utf8(&text.as_ref()[..end]).is_ok_and(|text| !text.contains('\u{fffe}'));
        Ok(Status::Decoded(if valid {
            Some(Subtitle::at(
                text.slice(0..end)?,
                self.clock,
                self.options.packet_timebase,
            )?)
        } else {
            None
        }))
    }
}
