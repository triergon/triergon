//! DVB subtitle delivery: the bitmap queue's actions from FFmpeg's `dvbsub`
//! decoder ([`Decoder`]).
//!
//! [`DvbTsBridge`] takes the strict PES of one selected service of an active
//! PMT, cuts each into the codec packet FFmpeg's demuxer hands its decoder
//! ([`PesParser`]) and decodes it. Matroska tracks feed the same decoder
//! through [`MatroskaDvbPacket`](crate::subtitles::matroska::MatroskaDvbPacket).
use crate::subtitles::ts::{PesBinding, PesData, Taken, TakenPes};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, Rounding, TimeBase, WorkBudget};
use triergon_mpegts::psi::{CodecIdentification, ElementaryStream, descriptor_tag};
use triergon_playback::Output;
use triergon_subtitles::compat::dvb::{
    Options, PesPacket, PesParser, PesProgress, Decoder, Rect, Status, Subtitle,
};
use triergon_subtitles::dvb::{PacketEnvelope, Size, SourceConfiguration};

/// The most rectangles a queued picture carries.
const MAX_RECTANGLES: usize = 8;

/// One selected service from one active PMT. A new playback generation, PMT
/// epoch or TS clock discontinuity needs a new bridge.
#[derive(Debug)]
pub struct DvbTsBridge {
    binding: PesBinding,
    configuration: SourceConfiguration,
    parser: PesParser,
    decoder: Decoder,
    pending: Option<Pending>,
    /// Whether the last PES time was on the player's timeline, so that
    /// subtitles have a time.
    anchored: bool,
    eof: bool,
}

/// What a pushed output left for the next poll.
#[expect(
    clippy::large_enum_variant,
    reason = "one pending PES per bridge, stepped in place; a box would be an allocation the pool does not charge"
)]
#[derive(Debug)]
enum Pending {
    /// An accepted PES on its way to the decoder.
    Pes(PesWork),
    /// The fault the player reported for a PES it discarded.
    Discarded(Error),
}

/// An accepted PES on its way to the decoder.
#[derive(Debug)]
struct PesWork {
    data: PesData,
    /// The PES time in 90 kHz ticks, on the player's timeline when it has
    /// one, else as coded: the parser groups segments by it.
    pts: Option<i128>,
    parser_input: bool,
    codec: Option<PesPacket>,
}

/// A service in the active elementary stream's descriptor loop, in wire order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbServiceSelection {
    /// Zero-based ordinal among DVB subtitling descriptors.
    pub descriptor: usize,
    /// Zero-based service entry within that descriptor.
    pub entry: usize,
}

/// What a decoded subtitle asks of the bitmap queue.
#[derive(Clone, Copy, Debug)]
pub enum DvbQueueAction<'a> {
    /// A subtitle without rectangles clears the screen.
    Clear {
        /// The packet-clock time of the clear, in nanoseconds.
        at_ns: i64,
    },
    /// Show a subtitle.
    Show(DvbQueuePicture<'a>),
}

/// A subtitle with rectangles, borrowed from the decoder until its next call.
#[derive(Clone, Copy, Debug)]
pub struct DvbQueuePicture<'a> {
    /// The start on the packet clock, in nanoseconds.
    pub start_ns: i64,
    /// The display size the rectangles are placed on, when plausible.
    pub canvas: Option<Size>,
    subtitle: Subtitle<'a>,
}

impl<'a> DvbQueuePicture<'a> {
    /// Indexed rectangles and their ARGB palettes, in the decoder's order.
    pub fn rectangles(self) -> impl Iterator<Item = Rect<'a>> {
        self.subtitle.rects().take(MAX_RECTANGLES)
    }
}

impl<'a> DvbQueueAction<'a> {
    pub(crate) fn from_subtitle(subtitle: Subtitle<'a>) -> Self {
        let start_ns = subtitle.pts.map_or(0, |us| us.saturating_mul(1000));
        if subtitle.rects().len() == 0 {
            return Self::Clear { at_ns: start_ns };
        }
        Self::Show(DvbQueuePicture {
            start_ns,
            canvas: subtitle.display_size.filter(|size| {
                (160..=8192).contains(&size.width) && (160..=8192).contains(&size.height)
            }),
            subtitle,
        })
    }
}

/// `ticks` of `clock` in microseconds, the decoder's clock.
pub(crate) fn microseconds(ticks: i128, clock: TimeBase) -> Result<i64> {
    let ticks = i64::try_from(ticks).map_err(|_| Error::Overflow)?;
    clock.rescale(ticks, TimeBase::new(1, 1_000_000)?, Rounding::NearestAway)
}

/// What a poll did.
#[derive(Debug)]
pub enum DvbStep<'a> {
    /// Push the next PES.
    NeedPacket,
    /// The work budget ran out; poll again.
    Yield,
    /// A subtitle came out of the packet.
    Action(DvbQueueAction<'a>),
    /// The decoder refused the PES as malformed; it presents nothing. Push the
    /// next.
    Refused(Error),
    /// The player discarded a PES of the PID and reported this fault in its
    /// place (`Error::Invalid` for an unbounded PES, for example). Push the
    /// next.
    Discarded(Error),
    /// Input finished and everything was decoded.
    Finished,
}

impl DvbTsBridge {
    /// Binds the one DVB subtitling service of `pid`: its PMT entry, in a DVB
    /// namespace, carries one subtitling descriptor that names one service, beside
    /// any other descriptors. Fails with `Unsupported` otherwise;
    /// [`new_for_service`](Self::new_for_service) selects one of several. The
    /// descriptor aliases the active PMT's storage.
    pub fn new(
        output: &Output,
        pid: u16,
        packet_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::with_service(output, pid, None, packet_limit, cancel)
    }

    /// Select one explicitly identified service in the active PMT. The
    /// decoder decodes only that service's composition and ancillary pages.
    pub fn new_for_service(
        output: &Output,
        pid: u16,
        service: DvbServiceSelection,
        packet_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        Self::with_service(output, pid, Some(service), packet_limit, cancel)
    }

    fn with_service(
        output: &Output,
        pid: u16,
        service: Option<DvbServiceSelection>,
        packet_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let (binding, stream) = PesBinding::new(output, pid, packet_limit)?;
        let is_dvb = stream.codec_identification() == CodecIdentification::DvbSubtitles;
        let (descriptor, entry) = match service {
            Some(service) if is_dvb => (
                subtitling_descriptor(stream, Some(service.descriptor), cancel)?,
                service.entry,
            ),
            Some(_) => {
                return Err(Error::Unsupported(
                    "DVB bridge requires DVB subtitling signalling",
                ));
            }
            None if is_dvb => (subtitling_descriptor(stream, None, cancel)?, 0),
            None => return Err(ONE_SERVICE),
        };
        let pool = output.pool();
        let configuration = SourceConfiguration::from_subtitling_descriptor(
            pool,
            &descriptor,
            entry,
            TimeBase::HZ_90000,
            PacketEnvelope::PesDataField,
        )?;
        let options = Options {
            service: Some(0),
            ..Options::default()
        };
        let decoder = Decoder::new(pool, &configuration.service_private_view(), options);
        Ok(Self {
            configuration,
            parser: PesParser::new(pool),
            decoder,
            binding,
            pending: None,
            anchored: false,
            eof: false,
        })
    }

    /// The selected service's configuration, aliasing the PMT descriptor.
    pub fn configuration(&self) -> &SourceConfiguration {
        &self.configuration
    }

    /// Moves the next strict PES of the PID out of `mailbox`, or the fault the
    /// player reported for the PID in place of one, which the next poll returns
    /// as [`DvbStep::Discarded`]. A busy bridge returns `false`, and validation
    /// errors leave the caller's output intact.
    pub fn push(&mut self, mailbox: &mut Option<Output>) -> Result<bool> {
        if mailbox.is_some() && self.eof {
            return Err(Error::Invalid("DVB bridge input after EOF"));
        }
        if self.pending.is_some() {
            return Ok(false);
        }
        self.pending = match self.binding.take(mailbox)? {
            None => return Ok(false),
            Some(Taken::Discarded(error)) => Some(Pending::Discarded(error)),
            Some(Taken::Pes(TakenPes {
                pes,
                pts: source_pts,
                ..
            })) => {
                if let Some(timestamp) =
                    source_pts.filter(|t| t.raw33.is_some() || t.ticks90.is_some())
                {
                    self.anchored = timestamp.ticks90.is_some();
                }
                Some(Pending::Pes(PesWork {
                    data: PesData::new(pes, self.binding.pool()),
                    pts: source_pts.and_then(|timestamp| {
                        timestamp.ticks90.or(timestamp.raw33.map(i128::from))
                    }),
                    parser_input: false,
                    codec: None,
                }))
            }
        };
        Ok(true)
    }

    /// Marks the end of the PID's PES.
    pub fn finish_input(&mut self) {
        self.eof = true;
    }

    /// Gathers, cuts and decodes the pending PES under `work`. A PES that
    /// presents nothing asks for the next, as does one the decoder refuses as
    /// malformed, after [`DvbStep::Refused`]. A PES too large for the pool
    /// (`Limit`) is dropped with its error; one refused for memory
    /// (`Backpressure`) stays pending.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DvbStep<'_>> {
        if cancel.load(Ordering::Relaxed) {
            self.decoder.abandon();
            self.pending = None;
            self.parser.reset();
            self.eof = true;
            return Err(Error::Cancelled);
        }
        let pending = match &mut self.pending {
            None => {
                return Ok(if self.eof {
                    DvbStep::Finished
                } else {
                    DvbStep::NeedPacket
                });
            }
            Some(Pending::Discarded(error)) => {
                let error = *error;
                self.pending = None;
                return Ok(DvbStep::Discarded(error));
            }
            Some(Pending::Pes(pending)) => pending,
        };
        let Some(codec) = pending.cut(&mut self.parser, self.binding.pool(), work, cancel)? else {
            return Ok(DvbStep::Yield);
        };
        if work.remaining() == 0 {
            return Ok(DvbStep::Yield);
        }
        let anchored = self.anchored;
        let pts = codec
            .codec_pts()
            .filter(|_| anchored)
            .map(|ticks| microseconds(ticks, self.configuration.clock()))
            .transpose()?;
        // A packet the decoder keeps is the pending PES: it yielded, stalled
        // or was cancelled, and is resumed.
        let status = if self.decoder.has_kept_packet() {
            self.decoder.resume(work, cancel)
        } else {
            self.decoder.decode(codec.codec(), pts, work, cancel)
        };
        match status {
            Ok(Status::Ready(subtitle)) => {
                self.pending = None;
                Ok(DvbStep::Action(DvbQueueAction::from_subtitle(subtitle)))
            }
            Ok(Status::Yield) => Ok(DvbStep::Yield),
            Ok(Status::Nothing) => {
                self.pending = None;
                Ok(DvbStep::NeedPacket)
            }
            Err(error @ Error::Invalid(_)) => {
                self.pending = None;
                Ok(DvbStep::Refused(error))
            }
            // The decoder keeps the packet, or refused all of it for want of
            // memory: either way the next poll tries it again. A cancelled
            // packet is dropped by the poll that sees the cancellation.
            Ok(Status::NeedOutput(error))
            | Err(error @ (Error::Backpressure { .. } | Error::Cancelled)) => Err(error),
            Err(error) => {
                self.pending = None;
                Err(error)
            }
        }
    }
}

/// The refusal of a default binding the PMT does not settle.
const ONE_SERVICE: Error = Error::Unsupported("DVB bridge requires one subtitling service");

/// The subtitling descriptor of `stream` that `selected` counts to among them in wire
/// order, or, for `None`, the only one, which must name one service; a slice of the PMT's
/// storage. Other descriptors of the loop are skipped.
fn subtitling_descriptor(
    stream: &ElementaryStream,
    selected: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Bytes> {
    let mut index = 0;
    let mut offset = 0_usize;
    let mut only = None;
    for descriptor in stream.descriptors() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let descriptor = descriptor?;
        let end = offset
            .checked_add(descriptor.source.len())
            .ok_or(Error::Overflow)?;
        if descriptor.tag == descriptor_tag::DVB_SUBTITLING {
            match selected {
                Some(selected) if index == selected => {
                    return stream.descriptor_bytes.slice(offset..end);
                }
                Some(_) => {}
                None if only.is_some() => return Err(ONE_SERVICE),
                // A service entry is eight bytes (EN 300 468 6.2.41).
                None => only = Some((offset..end, descriptor.data.len() == 8)),
            }
            index += 1;
        }
        offset = end;
    }
    match (selected, only) {
        (None, Some((range, true))) => stream.descriptor_bytes.slice(range),
        (None, _) => Err(ONE_SERVICE),
        (Some(_), _) => Err(Error::Invalid("DVB subtitling descriptor index")),
    }
}

impl PesWork {
    /// Gather the PES data field and cut it into its codec packet; `None` to
    /// yield.
    fn cut(
        &mut self,
        parser: &mut PesParser,
        pool: &Budget,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<&PesPacket>> {
        if self.codec.is_some() {
            return Ok(self.codec.as_ref());
        }
        if !self.parser_input {
            let Some(raw) = self.data.poll(pool, work)? else {
                return Ok(None);
            };
            parser.provide(raw, self.pts)?;
            self.parser_input = true;
        }
        match parser.step(work, cancel)? {
            PesProgress::Yield => Ok(None),
            PesProgress::Ready(packet) => Ok(Some(self.codec.insert(packet))),
            // The parser has this PES until it reports completion.
            PesProgress::NeedInput => Err(Error::Invalid("DVB parser input disappeared")),
        }
    }
}
