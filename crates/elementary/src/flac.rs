//! Native FLAC (RFC 9639): the `fLaC` marker, metadata blocks, then frames.
//!
//! A frame runs to the next frame header whose CRC-8 holds and that continues the
//! stream; the frame's CRC-16 confirms the end, and a damaged frame ends at the next
//! header that numbers on from it. Metadata blocks are kept as opaque bytes up to the
//! retention limit, PADDING by its length only; STREAMINFO and SEEKTABLE are also
//! read. A stream without the marker starts at its first frame.
use crate::stream::Limits;
use crate::syntax::{Gap, Length, Plan, Prelude, Probe, SeekContext, Syntax, TrackFacts, Verdict};
use triergon_bitstream::crc::crc16_a52;
use triergon_core::container::{CodecId, SeekMethod};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};
use triergon_framing::{FLAC_STREAMINFO, FlacBlocking, FlacFrameHeader, FlacStreamInfo};

/// The kind of a metadata block (RFC 9639 section 8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FlacBlockKind {
    StreamInfo,
    Padding,
    Application,
    SeekTable,
    VorbisComment,
    CueSheet,
    Picture,
    /// Types 7 to 126.
    Reserved(u8),
}

impl FlacBlockKind {
    fn from_code(code: u8) -> FlacBlockKind {
        match code {
            0 => FlacBlockKind::StreamInfo,
            1 => FlacBlockKind::Padding,
            2 => FlacBlockKind::Application,
            3 => FlacBlockKind::SeekTable,
            4 => FlacBlockKind::VorbisComment,
            5 => FlacBlockKind::CueSheet,
            6 => FlacBlockKind::Picture,
            other => FlacBlockKind::Reserved(other),
        }
    }
}

/// One metadata block.
#[derive(Clone, Debug)]
pub struct FlacMetadata {
    pub kind: FlacBlockKind,
    /// Body length in bytes.
    pub len: u32,
    /// The body, or `None` for PADDING and for blocks beyond the retention limit.
    pub body: Option<Bytes>,
}

/// A SEEKTABLE point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacSeekPoint {
    /// First sample of the target frame.
    pub sample: u64,
    /// Offset of the target frame from the first frame.
    pub offset: u64,
    /// Samples in the target frame.
    pub samples: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Marker,
    Block,
    Body {
        kind: FlacBlockKind,
        len: u32,
        last: bool,
    },
    Frames,
}

/// FLAC stream facts.
#[derive(Debug)]
pub struct Flac {
    expect: Expect,
    stream_info: Option<FlacStreamInfo>,
    first: Option<FlacFrameHeader>,
    metadata: BudgetVec<FlacMetadata>,
    seek_points: BudgetVec<FlacSeekPoint>,
    seek_point_limit: usize,
}

impl Flac {
    pub fn stream_info(&self) -> Option<&FlacStreamInfo> {
        self.stream_info.as_ref()
    }
    /// The metadata blocks in source order.
    pub fn metadata(&self) -> &[FlacMetadata] {
        &self.metadata
    }
    /// SEEKTABLE points in order, placeholders left out.
    pub fn seek_points(&self) -> &[FlacSeekPoint] {
        &self.seek_points
    }

    fn record(&mut self, kind: FlacBlockKind, len: u32, body: Option<Bytes>) -> Result<()> {
        self.metadata
            .try_push(FlacMetadata { kind, len, body })
            .map_err(|pushed| pushed.error)
    }

    fn read_seek_table(&mut self, body: &[u8]) -> Result<()> {
        let points = body.chunks_exact(18);
        if points.len() > self.seek_point_limit {
            return Ok(());
        }
        self.seek_points.try_reserve_exact(points.len())?;
        for point in points {
            let sample = u64::from_be_bytes(point[..8].try_into().map_err(|_| Error::Truncated)?);
            if sample == u64::MAX {
                continue;
            }
            let entry = FlacSeekPoint {
                sample,
                offset: u64::from_be_bytes(point[8..16].try_into().map_err(|_| Error::Truncated)?),
                samples: u16::from_be_bytes([point[16], point[17]]),
            };
            self.seek_points
                .try_push(entry)
                .map_err(|pushed| pushed.error)?;
        }
        Ok(())
    }

    fn after_block(&mut self, last: bool) {
        self.expect = if last { Expect::Frames } else { Expect::Block };
    }

    /// Samples of a fixed-blocking frame number: STREAMINFO's block size when the
    /// stream declares one, the frame's own otherwise (as FFmpeg reads streams that
    /// vary their block size under the fixed flag).
    fn fixed_block_size(&self, header: &FlacFrameHeader) -> u64 {
        match (self.stream_info, self.first) {
            (Some(info), _) if info.min_block_size == info.max_block_size => {
                u64::from(info.max_block_size)
            }
            (None, Some(first)) => u64::from(first.block_size),
            _ => u64::from(header.block_size),
        }
    }
}

impl Syntax for Flac {
    type Header = FlacFrameHeader;
    /// The longest frame header: sync and codes, a seven-byte number, two bytes each of
    /// block size and sample rate, and the CRC-8.
    const PROBE: usize = 16;
    const SYNC: u8 = 0xff;
    const GAP: Gap = Gap::AppendToPrevious;
    const CONFIRM: u8 = 1;
    const PREROLL: u32 = 0;

    fn new(budget: &Budget, limits: &Limits) -> Flac {
        Flac {
            expect: Expect::Marker,
            stream_info: None,
            first: None,
            metadata: BudgetVec::new(budget),
            seek_points: BudgetVec::new(budget),
            seek_point_limit: limits.seek_points,
        }
    }

    fn probe(&self, bytes: &[u8]) -> Probe<FlacFrameHeader> {
        match FlacFrameHeader::parse(bytes) {
            Ok(header) => Probe::Header(header),
            Err(Error::Truncated) => Probe::Short,
            Err(_) => Probe::Invalid,
        }
    }

    fn length(&self, header: &FlacFrameHeader) -> Length {
        // At least one byte of subframe and the CRC-16.
        Length::Search {
            minimum: header.len + 3,
        }
    }

    fn continues(&self, reference: &FlacFrameHeader, next: &FlacFrameHeader) -> bool {
        reference.blocking == next.blocking
    }

    fn verdict(
        &self,
        frame: &[u8],
        header: &FlacFrameHeader,
        next: Option<&FlacFrameHeader>,
    ) -> Verdict {
        if crc16_a52(0, frame) == 0 {
            return Verdict::Confirmed;
        }
        let expected = match header.blocking {
            FlacBlocking::Fixed => header.number + 1,
            FlacBlocking::Variable => header.number + u64::from(header.block_size),
        };
        match next {
            Some(next) if next.number == expected => Verdict::Plausible,
            _ => Verdict::No,
        }
    }

    fn samples(&self, header: &FlacFrameHeader) -> u32 {
        header.block_size
    }

    fn first_sample(&self, header: &FlacFrameHeader) -> Option<u64> {
        match header.blocking {
            FlacBlocking::Variable => Some(header.number),
            FlacBlocking::Fixed => header.number.checked_mul(self.fixed_block_size(header)),
        }
    }

    fn prelude(&mut self, view: &[u8], at_end: bool) -> Result<Prelude> {
        Ok(match self.expect {
            Expect::Marker => match view.get(..4) {
                None if !at_end && b"fLaC".starts_with(view) => Prelude::Need(4),
                Some(b"fLaC") => {
                    self.expect = Expect::Block;
                    Prelude::Skip(4)
                }
                _ => {
                    self.expect = Expect::Frames;
                    Prelude::Done
                }
            },
            Expect::Block => {
                let Some(header) = view.get(..4) else {
                    return Ok(Prelude::Need(4));
                };
                let kind = FlacBlockKind::from_code(header[0] & 0x7f);
                let len = u32::from_be_bytes([0, header[1], header[2], header[3]]);
                self.expect = Expect::Body {
                    kind,
                    len,
                    last: header[0] & 0x80 != 0,
                };
                Prelude::Skip(4)
            }
            Expect::Body { kind, len, last } => {
                if kind == FlacBlockKind::Padding || len == 0 {
                    self.record(kind, len, None)?;
                    self.after_block(last);
                    Prelude::Skip(u64::from(len))
                } else {
                    Prelude::Keep(u64::from(len))
                }
            }
            Expect::Frames => Prelude::Done,
        })
    }

    fn keep(&mut self, bytes: Bytes) -> Result<()> {
        let Expect::Body { kind, len, last } = self.expect else {
            return Err(Error::Invalid("FLAC metadata body out of place"));
        };
        match kind {
            FlacBlockKind::StreamInfo if self.stream_info.is_none() => {
                self.stream_info = Some(FlacStreamInfo::parse(&bytes)?);
            }
            FlacBlockKind::SeekTable if self.seek_points.is_empty() => {
                self.read_seek_table(&bytes)?;
            }
            _ => {}
        }
        self.record(kind, len, Some(bytes))?;
        self.after_block(last);
        Ok(())
    }

    fn omit(&mut self, len: u64) -> Result<()> {
        let Expect::Body { kind, last, .. } = self.expect else {
            return Err(Error::Invalid("FLAC metadata body out of place"));
        };
        if kind == FlacBlockKind::StreamInfo {
            return Err(Error::Invalid("FLAC STREAMINFO beyond the retention limit"));
        }
        self.record(kind, u32::try_from(len).map_err(|_| Error::Overflow)?, None)?;
        self.after_block(last);
        Ok(())
    }

    fn adopt(&mut self, header: &FlacFrameHeader, budget: &Budget) -> Result<TrackFacts> {
        self.first = Some(*header);
        let mut configuration = BudgetVec::new(budget);
        let (sample_rate, channels, duration) = match self.stream_info {
            Some(info) => {
                let body = self
                    .metadata
                    .iter()
                    .find(|block| block.kind == FlacBlockKind::StreamInfo)
                    .and_then(|block| block.body.as_ref())
                    .ok_or(Error::Invalid("FLAC STREAMINFO not kept"))?;
                configuration.try_extend_from_slice(b"fLaC")?;
                configuration.try_extend_from_slice(&[0x80 | FLAC_STREAMINFO, 0, 0, 34])?;
                configuration.try_extend_from_slice(&body[..FlacStreamInfo::BYTES])?;
                (
                    info.sample_rate,
                    info.channels,
                    (info.total_samples != 0).then_some(info.total_samples),
                )
            }
            None => (
                header.sample_rate.ok_or(Error::Invalid(
                    "FLAC frame defers its rate to an absent STREAMINFO",
                ))?,
                header.channels.channels(),
                None,
            ),
        };
        if sample_rate == 0 {
            return Err(Error::Invalid("FLAC sample rate zero"));
        }
        Ok(TrackFacts {
            codec_tag: b"fLaC",
            codec: CodecId::Flac,
            sample_rate,
            channels: Some(channels),
            configuration,
            duration,
        })
    }

    fn plan_seek(&self, target: u64, context: SeekContext) -> Plan {
        let points = self.seek_points();
        let below = points.iter().rposition(|point| point.sample <= target);
        let low = below.map_or((context.audio_start, 0), |i| {
            (context.audio_start + points[i].offset, points[i].sample)
        });
        let above = below.map_or(points.first(), |i| points.get(i + 1));
        let high = above
            .map(|point| context.audio_start + point.offset)
            .or(context.audio_end);
        match high {
            Some(high) => Plan::Bisect {
                low,
                high,
                method: if below.is_some() {
                    SeekMethod::Index
                } else {
                    SeekMethod::BoundedScan
                },
            },
            // Without the source length, read forward from the seek point.
            None if below.is_some() => Plan::Bisect {
                low,
                high: low.0,
                method: SeekMethod::Index,
            },
            None => Plan::Scan,
        }
    }

    fn reset(&mut self) {
        self.expect = Expect::Marker;
        self.stream_info = None;
        self.first = None;
        self.metadata.clear();
        self.seek_points.clear();
    }
}

impl crate::Format for Flac {}
