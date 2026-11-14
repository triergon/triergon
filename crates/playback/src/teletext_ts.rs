//! DVB teletext subtitles in TS (EN 300 472), decoded to timed text.
//!
//! [`TeletextTsBridge`] takes the strict PES of one teletext stream of an
//! active PMT (stream type 0x06 with a teletext descriptor in a DVB namespace)
//! and decodes each data field with FFmpeg's teletext decoder
//! ([`Decoder`]). A page becomes an opaque ASS [`TextSource`] for the
//! [`TextBridge`](crate::text_bridge::TextBridge); an empty page clears the
//! screen.
//!
//! Subtitles take the PES time on the player's timeline, bounded by the
//! program clock as FFmpeg's demuxer bounds it ([`pcr_bounded_pts`]): the
//! bridge reads the PCR from the transport packets the player delivers
//! ([`TeletextTsBridge::observe`]).
use crate::text_bridge::{AssIdentity, TextSource, receiver_timing};
use crate::ts_pes::{PesBinding, PesData, TakenPes};
use crate::{Output, RawEvent, TsEvent};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::CodecId;
use triergon_core::{Bytes, Error, Result, Rounding, Time, TimeBase, WorkBudget};
use triergon_mpegts::compat::pes::pcr_bounded_pts;
use triergon_mpegts::{Pes, TraceEvent, TransportPacket, clock::SourceTimestamp};
use triergon_subtitles::compat::teletext::{Decoded, Decoder, Options, Refusal};

/// Drain calls after the last packet, as the reference makes at most.
const MAX_DRAIN: usize = 64;
/// Unit starts remembered until their PES arrives.
const UNIT_STARTS: usize = 4;

/// What a poll did.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the cue moves its charged owners inline, as the text bridge's steps do"
)]
pub enum TeletextStep {
    /// Push the next PES.
    NeedPacket,
    /// The work budget ran out; poll again.
    Yield,
    /// A page for the text bridge.
    Cue(TextSource),
    /// An empty page: clear what is shown.
    Clear { at: Option<Time> },
    /// The decoder refused the packet; decoding continues with the next.
    Refused(Refusal),
    /// Input finished and the decoder drained.
    Finished,
}

/// One teletext stream of one active PMT. A new playback generation, PMT
/// epoch or TS clock epoch needs a new bridge.
pub struct TeletextTsBridge {
    binding: PesBinding,
    clock: ProgramClock,
    decoder: Decoder,
    header: Bytes,
    /// The pushed PES and its time in microseconds.
    pending: Option<(PesData, Option<i64>)>,
    sequence: u64,
    eof: bool,
    drains: usize,
}

impl TeletextTsBridge {
    /// Bind to `pid` of the active PMT `output`; it must carry teletext.
    pub fn new(output: &Output, pid: u16, options: Options, packet_limit: usize) -> Result<Self> {
        let (binding, stream) = PesBinding::new(output, pid, packet_limit)?;
        if stream.codec() != Some(CodecId::DvbTeletext) {
            return Err(Error::Unsupported(
                "teletext bridge requires DVB teletext signalling",
            ));
        }
        let decoder = Decoder::new(binding.pool(), options)?;
        let header = Bytes::copy_from_slice(binding.pool(), decoder.header().as_bytes())?;
        Ok(Self {
            clock: ProgramClock {
                pcr_pid: binding.pcr_pid,
                pid,
                pcr: None,
                starts: [None; UNIT_STARTS],
                next: 0,
            },
            binding,
            decoder,
            header,
            pending: None,
            sequence: 0,
            eof: false,
            drains: 0,
        })
    }

    /// The ASS script header the cues refer to.
    pub fn header(&self) -> &Bytes {
        &self.header
    }

    /// Read the program clock from a player output. Offer every output of the
    /// source in delivery order, before the PES it precedes is pushed; other
    /// events are ignored.
    pub fn observe(&mut self, output: &Output) -> Result<()> {
        if output.source_id() != self.binding.source() {
            return Err(Error::SourceMismatch);
        }
        if output.generation() != self.binding.generation() {
            return Err(Error::StaleGeneration {
                expected: self.binding.generation(),
                received: output.generation(),
            });
        }
        if let RawEvent::Ts(TsEvent {
            source: TraceEvent::Transport(packet),
            ..
        }) = output.event()
        {
            self.clock.observe(packet);
        }
        Ok(())
    }

    /// Move a PES of this stream out of `mailbox`. A busy bridge returns
    /// `false`; a refused output stays in the mailbox.
    pub fn push(&mut self, mailbox: &mut Option<Output>) -> Result<bool> {
        if mailbox.is_some() && self.eof {
            return Err(Error::Invalid("teletext bridge input after EOF"));
        }
        if self.pending.is_some() {
            return Ok(false);
        }
        let Some(TakenPes { pes, pts, .. }) = self.binding.take(mailbox)? else {
            return Ok(false);
        };
        let pts = self
            .clock
            .bounded(&pes, pts)
            .map(|ticks| {
                let ticks = i64::try_from(ticks).map_err(|_| Error::Overflow)?;
                TimeBase::new(1, 90_000)?.rescale(
                    ticks,
                    TimeBase::new(1, 1_000_000)?,
                    Rounding::NearestAway,
                )
            })
            .transpose()?;
        self.pending = Some((PesData::new(pes, self.binding.pool()), pts));
        Ok(true)
    }

    pub fn finish_input(&mut self) {
        self.eof = true;
    }

    /// Gather and decode the pushed PES. A decode needs
    /// [`Decoder::MAX_PACKET_WORK`] of `work`: a partly spent budget yields,
    /// a smaller fresh one is refused with [`Error::Backpressure`], and the
    /// packet is kept.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<TeletextStep> {
        if cancel.load(Ordering::Relaxed) {
            self.pending = None;
            self.eof = true;
            self.drains = MAX_DRAIN;
            return Err(Error::Cancelled);
        }
        let decoded = if let Some((data, pts)) = &mut self.pending {
            let Some(raw) = data.poll(self.binding.pool(), work)? else {
                return Ok(TeletextStep::Yield);
            };
            // Decode with a whole packet's work; a fresh budget below it is refused.
            if work.remaining() < Decoder::MAX_PACKET_WORK && work.spent() > 0 {
                return Ok(TeletextStep::Yield);
            }
            let decoded = self.decoder.decode(raw, *pts, work)?;
            self.pending = None;
            decoded
        } else if !self.eof {
            return Ok(TeletextStep::NeedPacket);
        } else if self.drains < MAX_DRAIN {
            let decoded = self.decoder.decode(&[], None, work)?;
            self.drains = match decoded {
                Decoded::Nothing => MAX_DRAIN,
                _ => self.drains + 1,
            };
            decoded
        } else {
            return Ok(TeletextStep::Finished);
        };
        let subtitle = match decoded {
            Decoded::Nothing => {
                return Ok(if self.drains == MAX_DRAIN {
                    TeletextStep::Finished
                } else if self.eof {
                    TeletextStep::Yield
                } else {
                    TeletextStep::NeedPacket
                });
            }
            Decoded::Refused(refusal) => return Ok(TeletextStep::Refused(refusal)),
            Decoded::Subtitle(subtitle) => subtitle,
        };
        let Some(line) = subtitle.ass else {
            let at = subtitle
                .pts
                .map(|us| Time::new(i128::from(us), 1_000_000))
                .transpose()?;
            return Ok(TeletextStep::Clear { at });
        };
        // `u32::MAX`: shown until the next page.
        let display = Some(subtitle.end_display).filter(|&ms| ms != u32::MAX);
        let cue = TextSource::ass_event(
            self.binding.pool(),
            &self.header,
            line.as_bytes(),
            receiver_timing(subtitle.pts, display)?,
            AssIdentity {
                source: self.binding.source(),
                generation: self.binding.generation(),
                sequence: self.sequence,
            },
        )?;
        self.sequence += 1;
        Ok(TeletextStep::Cue(cue))
    }
}

/// The program clock at the stream's unit starts, kept until their PES arrive.
#[derive(Debug)]
struct ProgramClock {
    pcr_pid: u16,
    pid: u16,
    /// The base of the last PCR on the PCR PID.
    pcr: Option<u64>,
    /// Unit starts by payload source offset, with the PCR base before each.
    starts: [Option<(u64, Option<u64>)>; UNIT_STARTS],
    next: usize,
}

impl ProgramClock {
    fn observe(&mut self, packet: &TransportPacket) {
        if packet.pid == self.pcr_pid
            && let Some(pcr) = packet.adaptation.as_ref().and_then(|a| a.pcr)
        {
            self.pcr = Some(pcr.base);
        }
        if packet.pid == self.pid && packet.payload_unit_start {
            self.starts[self.next] = Some((packet.payload_source_offset, self.pcr));
            self.next = (self.next + 1) % UNIT_STARTS;
        }
    }

    /// The PES time on the player's timeline, in 90 kHz ticks, bounded by
    /// the PCR before its unit start; `None` without one.
    fn bounded(&mut self, pes: &Pes, pts: Option<SourceTimestamp>) -> Option<i128> {
        let start = pes.header_source_ranges.as_slice().first()?.offset;
        let pcr = self
            .starts
            .iter_mut()
            .find(|entry| entry.is_some_and(|(offset, _)| offset == start))?
            .take()?
            .1;
        let pts = pts?;
        let raw = pts.raw33?;
        let bounded = pcr_bounded_pts(Some(raw), pcr)?;
        Some(pts.ticks90? + i128::from(bounded) - i128::from(raw))
    }
}
