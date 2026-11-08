//! DVB subtitle delivery: the bitmap queue's actions from FFmpeg's `dvbsub`
//! decoder ([`ReceiverDecoder`]).
//!
//! [`DvbTsBridge`] takes the strict PES of one selected service of an active
//! PMT, cuts each into the codec packet FFmpeg's demuxer hands its decoder
//! ([`PesParser`]) and decodes it. Matroska tracks feed the same decoder
//! through [`MatroskaDvbPacket`](crate::matroska_feed::MatroskaDvbPacket).
use crate::Output;
use crate::ts_pes::{PesBinding, PesData, TakenPes};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, Rounding, TimeBase, WorkBudget};
use triergon_mpegts::psi::CodecIdentification;
use triergon_subtitles::compat::dvb::{
    Options, PesPacket, PesParser, PesProgress, ReceiverDecoder, Rect, Status, Subtitle,
};
use triergon_subtitles::dvb::{PacketEnvelope, Size, SourceClock, SourceConfiguration};

/// The most rectangles a queued picture carries.
const MAX_RECTANGLES: usize = 8;

/// One selected service from one active PMT. A new playback generation, PMT
/// epoch or TS clock discontinuity needs a new bridge.
pub struct DvbTsBridge {
    binding: PesBinding,
    configuration: SourceConfiguration,
    parser: PesParser,
    decoder: ReceiverDecoder,
    pending: Option<Pending>,
    /// Whether the last PES time was on the player's timeline, so that
    /// subtitles have a time.
    anchored: bool,
    eof: bool,
}

/// An accepted PES on its way to the decoder.
struct Pending {
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
        at_ns: i64,
    },
    Show(DvbQueuePicture<'a>),
}

/// A subtitle with rectangles, borrowed from the decoder until its next call.
#[derive(Clone, Copy, Debug)]
pub struct DvbQueuePicture<'a> {
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
pub(crate) fn microseconds(ticks: i128, clock: SourceClock) -> Result<i64> {
    let ticks = i64::try_from(ticks).map_err(|_| Error::Overflow)?;
    clock
        .time_base()
        .rescale(ticks, TimeBase::new(1, 1_000_000)?, Rounding::NearestAway)
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
    /// Input finished and everything was decoded.
    Finished,
}

impl DvbTsBridge {
    /// Require an explicit DVB namespace and a single service descriptor for
    /// this PID. The complete descriptor aliases the active PMT's storage.
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
                subtitling_descriptor(stream, service.descriptor, cancel)?,
                service.entry,
            ),
            Some(_) => {
                return Err(Error::Unsupported(
                    "DVB bridge requires DVB subtitling signalling",
                ));
            }
            None if is_dvb && stream.descriptor_bytes.len() == 10 => {
                (stream.descriptor_bytes.clone(), 0)
            }
            None => {
                return Err(Error::Unsupported(
                    "DVB bridge requires one subtitling service",
                ));
            }
        };
        let pool = output.pool();
        let configuration = SourceConfiguration::from_subtitling_descriptor(
            pool,
            &descriptor,
            entry,
            SourceClock::new(1, 90_000)?,
            PacketEnvelope::PesDataField,
        )?;
        let options = Options {
            service: Some(0),
            ..Options::default()
        };
        let decoder = ReceiverDecoder::new(pool, &configuration.service_private_view(), options);
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

    /// Move a selected strict PES only on acceptance. A busy bridge returns
    /// `false` and validation errors leave the caller's output intact.
    pub fn push(&mut self, mailbox: &mut Option<Output>) -> Result<bool> {
        if mailbox.is_some() && self.eof {
            return Err(Error::Invalid("DVB bridge input after EOF"));
        }
        if self.pending.is_some() {
            return Ok(false);
        }
        let Some(TakenPes {
            pes,
            pts: source_pts,
            ..
        }) = self.binding.take(mailbox)?
        else {
            return Ok(false);
        };
        if let Some(timestamp) = source_pts.filter(|t| t.raw33.is_some() || t.ticks90.is_some()) {
            self.anchored = timestamp.ticks90.is_some();
        }
        let pts =
            source_pts.and_then(|timestamp| timestamp.ticks90.or(timestamp.raw33.map(i128::from)));
        self.pending = Some(Pending {
            data: PesData::new(pes, self.binding.pool()),
            pts,
            parser_input: false,
            codec: None,
        });
        Ok(true)
    }

    pub fn finish_input(&mut self) {
        self.eof = true;
    }

    /// Collect, cut and decode the pending PES under `work`. A packet the
    /// decoder refuses, or one that presents nothing, asks for the next.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<DvbStep<'_>> {
        if cancel.load(Ordering::Relaxed) {
            self.pending = None;
            self.parser.reset();
            self.eof = true;
            return Err(Error::Cancelled);
        }
        let Some(pending) = &mut self.pending else {
            return Ok(if self.eof {
                DvbStep::Finished
            } else {
                DvbStep::NeedPacket
            });
        };
        if !pending.cut(&mut self.parser, self.binding.pool(), work, cancel)?
            || work.remaining() == 0
        {
            return Ok(DvbStep::Yield);
        }
        let codec = pending
            .codec
            .as_ref()
            .ok_or(Error::Invalid("DVB codec packet absent"))?;
        let anchored = self.anchored;
        let pts = codec
            .codec_pts()
            .filter(|_| anchored)
            .map(|ticks| microseconds(ticks, self.configuration.clock()))
            .transpose()?;
        let status = self.decoder.decode(codec.codec(), pts, work, cancel);
        // A packet that yielded is decoded again at the next poll.
        if !matches!(status, Ok(Status::Yield)) {
            self.pending = None;
        }
        match status {
            Ok(Status::Subtitle(subtitle)) => {
                Ok(DvbStep::Action(DvbQueueAction::from_subtitle(subtitle)))
            }
            Ok(Status::Yield) => Ok(DvbStep::Yield),
            Ok(Status::Nothing) | Err(Error::Invalid(_)) => Ok(DvbStep::NeedPacket),
            Err(error) => Err(error),
        }
    }
}

fn subtitling_descriptor(
    stream: &triergon_mpegts::psi::ElementaryStream,
    selected: usize,
    cancel: &AtomicBool,
) -> Result<Bytes> {
    let mut index = 0;
    let mut offset = 0_usize;
    for descriptor in stream.descriptors() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let descriptor = descriptor?;
        let end = offset
            .checked_add(descriptor.source.len())
            .ok_or(Error::Overflow)?;
        if descriptor.tag == 0x59 {
            if index == selected {
                return stream.descriptor_bytes.slice(offset..end);
            }
            index += 1;
        }
        offset = end;
    }
    Err(Error::Invalid("DVB subtitling descriptor index"))
}

impl Pending {
    /// Gather the PES data field and cut it into its codec packet; `false`
    /// to yield.
    fn cut(
        &mut self,
        parser: &mut PesParser,
        pool: &Budget,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.codec.is_some() {
            return Ok(true);
        }
        if !self.parser_input {
            let Some(raw) = self.data.poll(pool, work)? else {
                return Ok(false);
            };
            parser.provide(raw, self.pts)?;
            self.parser_input = true;
        }
        match parser.step(work, cancel)? {
            PesProgress::Yield => Ok(false),
            PesProgress::Packet(packet) => {
                self.codec = Some(packet);
                Ok(true)
            }
            PesProgress::NeedPacket => Err(Error::Invalid("DVB parser input disappeared")),
        }
    }
}
