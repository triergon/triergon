//! QuickTime version-0 sound source facts and bounded PCM sample carriage.
//! Apple QTFF sound-description/sound-data contracts are the source basis.
//! Native catalogue, aggregation and presentation are separate projections.
use crate::{Sample, SampleDescription, SampleReader, SourcePacket, TrackHeader, fields};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error, InputCursor, InputToken, Result, SourceId, Step, WorkBudget};

/// Literal source fields, including values outside the selected PCM layout.
/// Version 1/2 is refused before reusing any version-0 field offsets.
#[derive(Clone, Debug)]
pub struct QtSoundDescriptionV0 {
    entry: SampleDescription,
    revision: u16,
    vendor: [u8; 4],
    channels: u16,
    sample_bits: u16,
    compression_id: i16,
    packet_size: u16,
    sample_rate_16_16: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QtPcmEncodingV0 {
    Unsigned8,
    Signed16BigEndian,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QtPcmLayoutV0 {
    encoding: QtPcmEncodingV0,
    channels: u16,
    frame_bytes: usize,
    sample_rate_16_16: u32,
}
impl QtPcmLayoutV0 {
    pub fn encoding(self) -> QtPcmEncodingV0 {
        self.encoding
    }
    pub fn channels(self) -> u16 {
        self.channels
    }
    pub fn frame_bytes(self) -> usize {
        self.frame_bytes
    }
    pub fn sample_rate_16_16(self) -> u32 {
        self.sample_rate_16_16
    }
}
impl QtSoundDescriptionV0 {
    /// The handler is required: `raw ` is also a visual entry coding name.
    pub fn parse(header: &TrackHeader, entry: &SampleDescription) -> Result<Self> {
        if header.handler != *b"soun"
            || fields::fourcc(header.raw_hdlr.payload(), 8)? != header.handler
        {
            return Err(Error::Invalid("QT sound description needs sound handler"));
        }
        let body = entry.raw().payload();
        if entry.index == 0
            || entry.format != entry.raw().header().kind
            || entry.data_reference_index != fields::u16_at(body, 6)?
        {
            return Err(Error::Invalid(
                "QT sound entry fields differ from raw source",
            ));
        }
        if fields::u16_at(body, 8)? != 0 {
            return Err(Error::Unsupported("QT sound description version1/2"));
        }
        fields::slice(body, 0, 28)?;
        Ok(Self {
            entry: entry.clone(),
            revision: fields::u16_at(body, 10)?,
            vendor: fields::fourcc(body, 12)?,
            channels: fields::u16_at(body, 16)?,
            sample_bits: fields::u16_at(body, 18)?,
            compression_id: i16::from_be_bytes(fields::u16_at(body, 20)?.to_be_bytes()),
            packet_size: fields::u16_at(body, 22)?,
            sample_rate_16_16: fields::u32_at(body, 24)?,
        })
    }
    pub fn source(&self) -> &SampleDescription {
        &self.entry
    }
    pub fn revision(&self) -> u16 {
        self.revision
    }
    pub fn vendor(&self) -> [u8; 4] {
        self.vendor
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn sample_bits(&self) -> u16 {
        self.sample_bits
    }
    pub fn compression_id(&self) -> i16 {
        self.compression_id
    }
    pub fn packet_size(&self) -> u16 {
        self.packet_size
    }
    pub fn sample_rate_16_16(&self) -> u32 {
        self.sample_rate_16_16
    }
    /// Exact 16.16 numerator/denominator. It is not the track's media clock.
    pub fn sample_rate_fraction(&self) -> (u32, u32) {
        (self.sample_rate_16_16, 65536)
    }
    pub fn pcm_layout(&self) -> Result<QtPcmLayoutV0> {
        if self.compression_id != 0 || self.packet_size != 0 {
            return Err(Error::Unsupported("QT v0 compressed/nonzero packet layout"));
        }
        if !(1..=2).contains(&self.channels) {
            return Err(Error::Unsupported("QT v0 selected PCM channel count"));
        }
        if self.sample_rate_16_16 == 0 {
            return Err(Error::Invalid("QT PCM zero source sample rate"));
        }
        let (encoding, bytes) = match (&self.entry.format, self.sample_bits) {
            (b"raw ", 8) => (QtPcmEncodingV0::Unsigned8, 1_usize),
            (b"twos", 16) => (QtPcmEncodingV0::Signed16BigEndian, 2_usize),
            _ => return Err(Error::Unsupported("QT v0 selected PCM coding/width")),
        };
        Ok(QtPcmLayoutV0 {
            encoding,
            channels: self.channels,
            frame_bytes: usize::from(self.channels)
                .checked_mul(bytes)
                .ok_or(Error::Overflow)?,
            sample_rate_16_16: self.sample_rate_16_16,
        })
    }
}

/// Complete source sample and its layout; no native packet aggregation, clock
/// repair, sample deletion or decoded-audio operation is implied.
#[derive(Debug)]
pub struct QtPcmSourcePacketV0 {
    packet: SourcePacket,
    description: QtSoundDescriptionV0,
    layout: QtPcmLayoutV0,
    complete_frames: usize,
}
impl QtPcmSourcePacketV0 {
    pub fn packet(&self) -> &SourcePacket {
        &self.packet
    }
    pub fn description(&self) -> &QtSoundDescriptionV0 {
        &self.description
    }
    pub fn layout(&self) -> QtPcmLayoutV0 {
        self.layout
    }
    pub fn complete_frames(&self) -> usize {
        self.complete_frames
    }
}

/// Reuses the shared fallible incremental source reader, including its bounded
/// payload allocation, source/range binding, yields and retained-capacity owner.
#[derive(Debug)]
pub struct QtPcmSampleReaderV0 {
    inner: Option<SampleReader>,
    description: Option<QtSoundDescriptionV0>,
    layout: QtPcmLayoutV0,
    pool: Budget,
    source: SourceId,
    token: InputToken,
    position: u64,
    failed: Option<Error>,
}
impl QtPcmSampleReaderV0 {
    pub fn new(
        input: &mut InputCursor,
        description: QtSoundDescriptionV0,
        sample: Sample,
        media: Range<u64>,
        maximum_packet: usize,
    ) -> Result<Self> {
        if !description
            .source()
            .raw()
            .bytes()
            .belongs_to(input.budget())
        {
            return Err(Error::ForeignPool);
        }
        if sample.description_index != description.source().index {
            return Err(Error::Invalid("QT PCM source description mismatch"));
        }
        let layout = description.pcm_layout()?;
        let bytes = sample
            .range
            .end
            .checked_sub(sample.range.start)
            .ok_or(Error::Overflow)?;
        if bytes == 0
            || bytes % u64::try_from(layout.frame_bytes).map_err(|_| Error::Overflow)? != 0
        {
            return Err(Error::Invalid("QT PCM incomplete source frame"));
        }
        let inner = SampleReader::new(input, sample, media, maximum_packet)?;
        Ok(Self {
            inner: Some(inner),
            description: Some(description),
            layout,
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            position: input.position(),
            failed: None,
        })
    }
    fn poison(&mut self, error: Error) -> Error {
        self.inner = None;
        self.description = None;
        self.failed = Some(error);
        error
    }
    pub fn poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<QtPcmSourcePacketV0>> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        match self.inner_poll(input, work, cancel) {
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
            step => step,
        }
    }
    fn inner_poll(
        &mut self,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<QtPcmSourcePacketV0>> {
        if !input.budget().same_pool(&self.pool) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        let token = input.token();
        if token.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: token.generation(),
            });
        }
        if token != self.token {
            return Err(Error::StaleRequest {
                expected: self.token.request(),
                received: token.request(),
            });
        }
        if input.position() != self.position {
            return Err(Error::Invalid("QT PCM input moved outside reader"));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        input.read_into(&mut [], work, cancel)?;
        if work.remaining() == 0 {
            return Ok(Step::Yield);
        }
        let step = self
            .inner
            .as_mut()
            .ok_or(Error::Invalid("QT PCM source reader finished"))?
            .poll(input, work, cancel);
        self.position = input.position();
        match step? {
            Step::Ready(packet) => {
                let frames = packet.payload.len() / self.layout.frame_bytes;
                self.inner = None;
                Ok(Step::Ready(QtPcmSourcePacketV0 {
                    packet,
                    layout: self.layout,
                    description: self
                        .description
                        .take()
                        .ok_or(Error::Invalid("QT PCM source description absent"))?,
                    complete_frames: frames,
                }))
            }
            Step::NeedInput(request) => Ok(Step::NeedInput(request)),
            Step::Yield => Ok(Step::Yield),
            Step::End(end) => Ok(Step::End(end)),
        }
    }
}
