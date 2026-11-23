//! Frames container packets for decoders using [`TrackInfo`] and [`PacketInfo`].
//! AVC and HEVC become Annex B, AAC with a configuration record becomes ADTS (the
//! first frame carrying the record's program config element when it has one), and
//! other codecs pass through. Work and storage use the caller's budgets.
use crate::aac::{opening_block, raw_block_program};
use crate::matroska::superseded_aac_lc_track_parameters;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{CodecId, PacketInfo, RandomAccess, TrackId, TrackInfo, Trim};
use triergon_core::{Budget, Bytes, Error, Result, SourceId, SourceLocation, TimeBase, WorkBudget};
use triergon_framing::{
    BuildStep,
    aac::{AacAdtsParameters, AacAdtsWriter, AacAscParser, AacConfigLimits, AacSource},
    avc::{AvcConfigLimits, AvcConfigParser, AvcConfigSource},
    compat::nal::{
        NalPacket, NalPacketConfiguration, NalPacketJob, NalPacketLimits, NalPacketSession,
    },
    hevc::{HvcConfigLimits, HvcConfigParser, HvcConfigSource},
};
use triergon_matroska::Track as MatroskaTrack;

/// The source the framing jobs report: packets come through [`PacketInfo`], which
/// carries no source, so diagnostics name this one at offset zero.
const UNNAMED: SourceId = SourceId(0);
const AAC_SOURCE: AacSource = AacSource {
    source: UNNAMED,
    generation: 0,
    offset: 0,
};

/// Bounds of [`AccessUnits`].
#[derive(Clone, Copy, Debug)]
pub struct AccessUnitLimits {
    /// Largest configuration record, in bytes (1 MiB by default).
    pub configuration_bytes: usize,
    /// Most parameter sets, or HEVC parameter-set arrays, in one record (256).
    pub configuration_nals: usize,
    /// Largest packet, in bytes (1 MiB).
    pub packet_bytes: usize,
    /// Largest access unit in bytes, including inserted sets and headers (2 MiB).
    pub unit_bytes: usize,
    /// Most NAL units in one packet (1,024).
    pub packet_nals: usize,
}

impl Default for AccessUnitLimits {
    fn default() -> Self {
        Self {
            configuration_bytes: 1 << 20,
            configuration_nals: 256,
            packet_bytes: 1 << 20,
            unit_bytes: 2 << 20,
            packet_nals: 1024,
        }
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "each packet reads its framing in place; a box would add an indirection per packet and an allocation the pool does not charge"
)]
#[derive(Debug)]
enum Framing {
    Nal(NalPacketSession),
    Adts(AacAdtsParameters),
    /// ADTS whose next published frame opens its raw data block with `program`,
    /// the configuration's program config element.
    AdtsProgram {
        parameters: AacAdtsParameters,
        program: Bytes,
    },
    Packet,
}

impl Framing {
    /// Plain ADTS from now on, once a frame has carried the program config element.
    fn program_sent(&mut self) {
        if let Self::AdtsProgram { parameters, .. } = *self {
            *self = Self::Adts(parameters);
        }
    }
}

/// The framing of one track's packets for its decoder.
#[derive(Debug)]
pub struct AccessUnits {
    pool: Budget,
    limits: AccessUnitLimits,
    framing: Framing,
}

#[expect(
    clippy::large_enum_variant,
    reason = "one setup per track, stepped in place; a box would be an allocation the pool does not charge"
)]
#[derive(Debug)]
enum Setup {
    Avc(AvcConfigParser),
    Hevc(HvcConfigParser),
    Aac(AacAscParser),
    Ready(Option<Framing>),
}

/// Reads a track's configuration record; poll [`Self::step`] until it is ready.
#[derive(Debug)]
pub struct AccessUnitsSetup {
    pool: Budget,
    limits: AccessUnitLimits,
    setup: Setup,
}

impl AccessUnits {
    /// Uses the track's configuration record and charges buffers to `pool`.
    /// Fails with `Limit` if the record exceeds `limits`, and with `Unsupported`
    /// for a Matroska AAC track without a record, which
    /// [`for_matroska`](Self::for_matroska) frames. Annex B and ADTS tracks can
    /// pass through with an empty record.
    pub fn for_track(
        pool: &Budget,
        track: &dyn TrackInfo,
        limits: AccessUnitLimits,
    ) -> Result<AccessUnitsSetup> {
        Self::with_record(pool, track, track.configuration(), limits)
    }

    /// [`for_track`](Self::for_track) for a Matroska track. An AAC-LC codec ID that
    /// `A_AAC` supersedes (`A_AAC/MPEG2/LC`, `A_AAC/MPEG4/LC`) carries raw access units and no
    /// initialization record (a zero-length CodecPrivate is none): their ADTS headers
    /// take the indexed sampling frequency and a count of one to six channels from the
    /// track's Audio element, as [`MatroskaFeedSetup`](crate::matroska::MatroskaFeedSetup)'s
    /// do. Fails with `Unsupported` for such a track whose rate or channel count ADTS
    /// cannot index, and as `for_track` does.
    pub fn for_matroska(
        pool: &Budget,
        track: &MatroskaTrack,
        limits: AccessUnitLimits,
    ) -> Result<AccessUnitsSetup> {
        match superseded_aac_lc_track_parameters(track)? {
            Some(parameters) => Ok(AccessUnitsSetup {
                pool: pool.clone(),
                limits,
                setup: Setup::Ready(Some(Framing::Adts(parameters))),
            }),
            None => Self::for_track(pool, track, limits),
        }
    }

    /// Overrides the track's record. Empty Matroska AAC records give `Unsupported`.
    pub fn with_record(
        pool: &Budget,
        track: &dyn TrackInfo,
        record: &[u8],
        limits: AccessUnitLimits,
    ) -> Result<AccessUnitsSetup> {
        let owned = || -> Result<Bytes> {
            if record.len() > limits.configuration_bytes {
                return Err(Error::Limit {
                    requested: record.len(),
                    maximum: limits.configuration_bytes,
                });
            }
            Bytes::copy_from_slice(pool, record)
        };
        let setup = match track.codec() {
            CodecId::H264 if !record.is_empty() => Setup::Avc(AvcConfigParser::new(
                pool,
                AvcConfigSource {
                    source: UNNAMED,
                    generation: 0,
                    offset: 0,
                },
                owned()?,
                AvcConfigLimits {
                    maximum_record_bytes: limits.configuration_bytes,
                    maximum_nals: limits.configuration_nals,
                    maximum_nal_bytes: limits.configuration_bytes,
                },
            )?),
            CodecId::Hevc if !record.is_empty() => Setup::Hevc(HvcConfigParser::new(
                pool,
                HvcConfigSource {
                    source: UNNAMED,
                    generation: 0,
                    offset: 0,
                },
                owned()?,
                HvcConfigLimits {
                    maximum_record_bytes: limits.configuration_bytes,
                    maximum_arrays: limits.configuration_nals,
                    maximum_nals: limits.configuration_nals,
                    maximum_nal_bytes: limits.configuration_bytes,
                },
            )?),
            CodecId::Aac if !record.is_empty() => Setup::Aac(AacAscParser::new(
                pool,
                AAC_SOURCE,
                owned()?,
                AacConfigLimits {
                    maximum_record_bytes: limits.configuration_bytes,
                    maximum_pce_elements: 64,
                    maximum_comment_bytes: 255,
                },
            )?),
            // Matroska stores AAC frames raw under every one of its AAC codec
            // identifiers, so without a record they cannot pass as ADTS.
            CodecId::Aac if track.codec_tag().starts_with(b"A_AAC") => {
                return Err(Error::Unsupported(
                    "Matroska AAC track without an initialization record",
                ));
            }
            _ => Setup::Ready(Some(Framing::Packet)),
        };
        Ok(AccessUnitsSetup {
            pool: pool.clone(),
            limits,
            setup,
        })
    }

    /// Starts framing a packet of the track. Oversized packets fail with `Limit`.
    ///
    /// For AAC with channel configuration 0, the first frame published opens its
    /// raw data block with the configuration's program config element, unless the
    /// packet's block opens with one; a job dropped before it publishes leaves the
    /// element to the next.
    pub fn packet<'a, P: PacketInfo>(&'a mut self, packet: &'a P) -> Result<AccessUnitJob<'a, P>> {
        let payload = packet.payload();
        if payload.len() > self.limits.packet_bytes {
            return Err(Error::Limit {
                requested: payload.len(),
                maximum: self.limits.packet_bytes,
            });
        }
        let framing = &mut self.framing;
        let job = match framing {
            Framing::Nal(session) => Job::Nal(session.packet(
                NalPacket {
                    bytes: Bytes::copy_from_slice(&self.pool, payload)?,
                    location: SourceLocation {
                        source: UNNAMED,
                        offset: 0,
                    },
                    generation: 0,
                    fields: (),
                    new_extradata: None,
                },
                None,
            )?),
            Framing::Adts(parameters) => Job::Adts(AacAdtsWriter::new(
                &self.pool,
                AAC_SOURCE,
                Bytes::copy_from_slice(&self.pool, payload)?,
                *parameters,
                self.limits.unit_bytes,
            )?),
            Framing::AdtsProgram {
                parameters,
                program,
            } => {
                let writer = program_writer(
                    &self.pool,
                    *parameters,
                    program,
                    payload,
                    self.limits.unit_bytes,
                )?;
                Job::AdtsProgram(writer, framing)
            }
            Framing::Packet => Job::Packet { done: false },
        };
        Ok(AccessUnitJob { packet, job })
    }
}

/// The writer of the frame that carries `program` ahead of `payload`, out of the
/// per-packet path: it runs once per framing unless a job is dropped.
#[cold]
#[inline(never)]
fn program_writer(
    pool: &Budget,
    parameters: AacAdtsParameters,
    program: &[u8],
    payload: &[u8],
    unit_bytes: usize,
) -> Result<AacAdtsWriter> {
    let block = match opening_block(pool, program, payload)? {
        Some(block) => block,
        None => Bytes::copy_from_slice(pool, payload)?,
    };
    AacAdtsWriter::new(pool, AAC_SOURCE, block, parameters, unit_bytes)
}

impl AccessUnitsSetup {
    /// Reads the record within `work`: `Ready` once, then `Complete`.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AccessUnits>> {
        let session = |configuration| {
            NalPacketSession::new(
                &self.pool,
                configuration,
                NalPacketLimits {
                    maximum_packet_bytes: self.limits.packet_bytes,
                    maximum_output_bytes: self.limits.unit_bytes,
                    maximum_nals: self.limits.packet_nals,
                    maximum_segments: self.limits.packet_nals.saturating_mul(2),
                },
            )
            .map(Framing::Nal)
        };
        let framing = match &mut self.setup {
            Setup::Avc(parser) => match parser.step(work, cancel)? {
                BuildStep::Ready(record) => session(NalPacketConfiguration::Avc(record))?,
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            Setup::Hevc(parser) => match parser.step(work, cancel)? {
                BuildStep::Ready(record) => session(NalPacketConfiguration::Hevc(record))?,
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            Setup::Aac(parser) => match parser.step(work, cancel)? {
                BuildStep::Ready(record) => {
                    let parameters = AacAdtsParameters::from_asc(record.facts())?;
                    match record.program_config() {
                        Some(program) => Framing::AdtsProgram {
                            parameters,
                            program: raw_block_program(&self.pool, record.source_bytes(), program)?,
                        },
                        None => Framing::Adts(parameters),
                    }
                }
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            Setup::Ready(framing) => match framing.take() {
                Some(framing) => framing,
                None => return Ok(BuildStep::Complete),
            },
        };
        self.setup = Setup::Ready(None);
        Ok(BuildStep::Ready(AccessUnits {
            pool: self.pool.clone(),
            limits: self.limits,
            framing,
        }))
    }
}

#[derive(Debug)]
enum Job<'a> {
    Nal(NalPacketJob<'a, ()>),
    Adts(AacAdtsWriter),
    /// The frame that carries the program config element of the track's framing.
    AdtsProgram(AacAdtsWriter, &'a mut Framing),
    Packet {
        done: bool,
    },
}

/// The framing of one packet, stepped within a work budget.
#[derive(Debug)]
pub struct AccessUnitJob<'a, P> {
    packet: &'a P,
    job: Job<'a>,
}

impl<'a, P: PacketInfo> AccessUnitJob<'a, P> {
    /// Frames the packet within `work`: `Ready` once, then `Complete`.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AccessUnit<'a, P>>> {
        let bytes = match &mut self.job {
            Job::Nal(job) => match job.step(work, cancel)? {
                BuildStep::Ready(output) => Unit::Framed(output.bytes().clone()),
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            Job::Adts(writer) => match writer.step(work, cancel)? {
                BuildStep::Ready(output) => Unit::Framed(output.bytes().clone()),
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            Job::AdtsProgram(writer, framing) => match writer.step(work, cancel)? {
                BuildStep::Ready(output) => {
                    framing.program_sent();
                    Unit::Framed(output.bytes().clone())
                }
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            Job::Packet { done: true } => return Ok(BuildStep::Complete),
            Job::Packet { done } => {
                if !work.try_charge(1) {
                    return Ok(BuildStep::Yield);
                }
                *done = true;
                Unit::Packet(self.packet.payload())
            }
        };
        Ok(BuildStep::Ready(AccessUnit {
            packet: self.packet,
            bytes,
        }))
    }
}

#[derive(Clone, Debug)]
enum Unit<'a> {
    Framed(Bytes),
    Packet(&'a [u8]),
}

/// Decoder bytes with the original packet's track, times and trim.
#[derive(Clone, Debug)]
pub struct AccessUnit<'a, P> {
    packet: &'a P,
    bytes: Unit<'a>,
}

impl<P> AccessUnit<'_, P> {
    /// The bytes framing made, or `None` when the unit is the packet's own payload,
    /// borrowed.
    pub fn framed(&self) -> Option<&Bytes> {
        match &self.bytes {
            Unit::Framed(bytes) => Some(bytes),
            Unit::Packet(_) => None,
        }
    }
}

impl<P: PacketInfo> PacketInfo for AccessUnit<'_, P> {
    fn track(&self) -> TrackId {
        self.packet.track()
    }
    /// The access unit's bytes.
    fn payload(&self) -> &[u8] {
        match &self.bytes {
            Unit::Framed(bytes) => bytes.as_ref(),
            Unit::Packet(bytes) => bytes,
        }
    }
    fn time_base(&self) -> TimeBase {
        self.packet.time_base()
    }
    fn pts(&self) -> Option<i64> {
        self.packet.pts()
    }
    fn dts(&self) -> Option<i64> {
        self.packet.dts()
    }
    fn duration(&self) -> Option<i64> {
        self.packet.duration()
    }
    fn random_access(&self) -> RandomAccess {
        self.packet.random_access()
    }
    fn trim(&self) -> Trim {
        self.packet.trim()
    }
}
