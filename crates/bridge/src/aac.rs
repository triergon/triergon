//! MP4 AAC access units as ADTS frames: an [`AacBridge`] is bound to one track and
//! configuration revision of a plan, with the AudioSpecificConfig the caller read for it.
use std::sync::atomic::AtomicBool;
use triergon_bitstream::bits_at;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};
use triergon_framing::{
    BuildStep,
    aac::{
        AacAdtsOutput, AacAdtsParameters, AacAdtsWriter, AacAudioSpecificConfig, AacProgramConfig,
        AacSource,
    },
};
use triergon_playback::mp4::Mp4Packet;
/// Wraps the raw AAC access units of one MP4 track and configuration revision in
/// ADTS headers.
#[derive(Debug)]
pub struct AacBridge {
    pool: Budget,
    source: triergon_core::SourceId,
    track: u32,
    revision: u64,
    configuration: AacAudioSpecificConfig,
    parameters: AacAdtsParameters,
    maximum: usize,
    /// Whether the next published frame opens with the program config element of
    /// `configuration`.
    program_pending: bool,
}
/// The ADTS framing of one MP4 packet, stepped within a work budget.
#[derive(Debug)]
pub struct AacPacketJob<'a> {
    source: Option<Mp4Packet>,
    writer: AacAdtsWriter,
    /// The bridge's `program_pending` when this frame opens with the program config
    /// element, cleared once the frame is published.
    program_pending: Option<&'a mut bool>,
}
/// One MP4 packet framed as ADTS.
#[derive(Debug)]
pub struct AacPacketOutput {
    /// The MP4 packet the frame was made from, with its clocks and configuration.
    pub source: Mp4Packet,
    /// The ADTS header and the packet's payload.
    pub adts: AacAdtsOutput,
}
impl AacBridge {
    /// A bridge for `track` of `source` at configuration `revision`, writing frames of at
    /// most `maximum` bytes. Fails with `ForeignPool` when `configuration` is not charged
    /// to `pool` and with `Invalid` for track 0 or a `maximum` below 7. It also fails
    /// when ADTS cannot carry the configuration.
    pub fn new(
        pool: &Budget,
        source: triergon_core::SourceId,
        track: u32,
        revision: u64,
        configuration: AacAudioSpecificConfig,
        maximum: usize,
    ) -> Result<Self> {
        if !configuration.source_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if track == 0 || maximum < 7 {
            return Err(Error::Invalid("AAC bridge policy"));
        }
        let parameters = AacAdtsParameters::from_asc(configuration.facts())?;
        let program_pending = configuration.program_config().is_some();
        Ok(Self {
            pool: pool.clone(),
            source,
            track,
            revision,
            configuration,
            parameters,
            maximum,
            program_pending,
        })
    }
    /// Starts framing `packet`. With channel configuration 0, the first frame the
    /// bridge publishes opens its raw data block with the configuration's program
    /// config element, unless the packet's block opens with one; a job dropped
    /// before it publishes leaves the element to the next. Fails with
    /// `SourceMismatch` for another source and with `Invalid` when its track,
    /// revision or `mp4a` sample entry differs from the bridge's, and with `Limit`
    /// when the frame exceeds the bridge's maximum.
    pub fn packet(&mut self, packet: Mp4Packet) -> Result<AacPacketJob<'_>> {
        if packet.source.source != self.source {
            return Err(Error::SourceMismatch);
        }
        if packet.configuration.track.track_id != self.track
            || packet.configuration.revision != self.revision
            || packet.configuration.description.format != *b"mp4a"
        {
            return Err(Error::Invalid(
                "AAC source configuration association changed",
            ));
        }
        if self.program_pending {
            return self.program_packet(packet);
        }
        let writer = AacAdtsWriter::new(
            &self.pool,
            payload_source(&packet),
            packet.source.payload.clone(),
            self.parameters,
            self.maximum,
        )?;
        Ok(AacPacketJob {
            source: Some(packet),
            writer,
            program_pending: None,
        })
    }
    /// The job of the frame that carries the program config element, out of the
    /// per-packet path: it runs once per bridge unless a job is dropped.
    #[cold]
    #[inline(never)]
    fn program_packet(&mut self, packet: Mp4Packet) -> Result<AacPacketJob<'_>> {
        let configuration = &self.configuration;
        let payload = match configuration.program_config() {
            Some(program) => {
                let element = raw_block_program(&self.pool, configuration.source_bytes(), program)?;
                opening_block(&self.pool, &element, &packet.source.payload)?
            }
            None => None,
        }
        .unwrap_or_else(|| packet.source.payload.clone());
        let writer = AacAdtsWriter::new(
            &self.pool,
            payload_source(&packet),
            payload,
            self.parameters,
            self.maximum,
        )?;
        Ok(AacPacketJob {
            source: Some(packet),
            writer,
            program_pending: Some(&mut self.program_pending),
        })
    }
}
impl AacPacketJob<'_> {
    /// Writes the frame within `w`: `Ready` once, then `Complete`.
    pub fn step(
        &mut self,
        w: &mut triergon_core::WorkBudget,
        c: &AtomicBool,
    ) -> Result<BuildStep<AacPacketOutput>> {
        Ok(match self.writer.step(w, c)? {
            BuildStep::Yield => BuildStep::Yield,
            BuildStep::Complete => BuildStep::Complete,
            BuildStep::Ready(adts) => {
                if let Some(pending) = self.program_pending.take() {
                    *pending = false;
                }
                BuildStep::Ready(AacPacketOutput {
                    source: self
                        .source
                        .take()
                        // A typed job phase raises AAC access-unit instruction counts by 0.30–0.31%.
                        .ok_or(Error::Invalid("AAC source output already consumed"))?,
                    adts,
                })
            }
        })
    }
}

/// Where the payload of `packet` starts in its source.
fn payload_source(packet: &Mp4Packet) -> AacSource {
    AacSource {
        source: packet.source.source,
        generation: packet.source.token.generation(),
        offset: packet.source.sample.range.start,
    }
}

/// `ID_PCE`, the identifier a program config element has among the syntactic
/// elements of a raw data block.
const ID_PCE: u8 = 5;

/// `program`, the program config element of the AudioSpecificConfig `record`, as the
/// first syntactic element of an ADTS raw data block, charged to `pool`.
///
/// With channel configuration 0 an ADTS header names no layout: a program config
/// element that opens the raw data block gives it (ISO/IEC 14496-3, 1.A.4.3), as
/// the one in the GASpecificConfig does for an AudioSpecificConfig (1.6.2.1, 4.4.1).
/// The element written is `ID_PCE`, the record's fields from `element_instance_tag`
/// through the last list entry, zero bits up to a byte boundary counted from the
/// start of the block (4.4.1.1), then `comment_field_bytes` and the comment.
#[cold]
#[inline(never)]
pub(crate) fn raw_block_program(
    pool: &Budget,
    record: &[u8],
    program: &AacProgramConfig,
) -> Result<Bytes> {
    let facts = program.facts();
    let comment = program.comment_bytes();
    let fields = comment
        .len()
        .checked_add(1)
        .and_then(|bytes| bytes.checked_mul(8))
        .and_then(|bits| bits.checked_add(usize::from(facts.alignment_bits)))
        .and_then(|tail| facts.source_bits.bit_count.checked_sub(tail))
        .ok_or(Error::Invalid("AAC program config element extent"))?;
    let head = fields.checked_add(3).ok_or(Error::Overflow)?.div_ceil(8);
    let mut element = BudgetVec::try_with_owner_capacity(pool, head + 1 + comment.len())?;
    let (mut word, mut held) = (u64::from(ID_PCE), 3);
    let mut position = facts.source_bits.start_bit;
    let end = position + fields;
    while position < end {
        let width = (end - position).min(24);
        let bits = bits_at(record, position, width as u32).ok_or(Error::Truncated)?;
        word = (word << width) | u64::from(bits);
        held += width;
        position += width;
        while held >= 8 {
            held -= 8;
            element.try_push((word >> held) as u8)?;
        }
        word &= (1 << held) - 1;
    }
    if held > 0 {
        element.try_push((word << (8 - held)) as u8)?;
    }
    element.try_push(comment.len() as u8)?;
    element.try_extend_from_slice(comment)?;
    Ok(element.freeze()?)
}

/// The raw data block `payload` behind the program config element `program`, in one
/// allocation charged to `pool`; `None` when the block already opens with a program
/// config element.
pub(crate) fn opening_block(
    pool: &Budget,
    program: &[u8],
    payload: &[u8],
) -> Result<Option<Bytes>> {
    if payload.first().is_some_and(|byte| byte >> 5 == ID_PCE) {
        return Ok(None);
    }
    let length = program
        .len()
        .checked_add(payload.len())
        .ok_or(Error::Overflow)?;
    let mut block = BudgetVec::try_with_owner_capacity(pool, length)?;
    block.try_extend_from_slice(program)?;
    block.try_extend_from_slice(payload)?;
    Ok(Some(block.freeze()?))
}
