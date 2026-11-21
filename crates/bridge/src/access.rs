//! The access units a decoder takes, framed from any reader's packets through the
//! neutral views: Annex B for AVC and HEVC, ADTS for AAC with a configuration
//! record, and the packet itself for every other codec.
//!
//! [`AccessUnits::for_track`] reads the track's [`TrackInfo`] configuration record;
//! [`AccessUnits::packet`] frames one packet of the track. Every step is bounded by
//! the caller's [`WorkBudget`], and every buffer is charged to the pool.
use std::sync::atomic::AtomicBool;
use triergon_core::container::{CodecId, PacketInfo, RandomAccess, TrackId, TrackInfo, Trim};
use triergon_core::{Budget, Bytes, Error, Result, SourceId, SourceLocation, TimeBase, WorkBudget};
use triergon_framing::{
    BuildStep,
    aac::{AacAdtsParameters, AacAdtsWriter, AacAscParser, AacConfigLimits, AacSource},
    avc::{AvcConfigLimits, AvcConfigParser, AvcConfigSource},
    hevc::{HvcConfigLimits, HvcConfigParser, HvcConfigSource},
    nal::{NalPacket, NalPacketConfiguration, NalPacketJob, NalPacketLimits, NalPacketSession},
};

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
    /// Largest access unit, in bytes, inserted parameter sets and headers included
    /// (2 MiB).
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

#[allow(
    clippy::large_enum_variant,
    reason = "one session per track, kept inline in its framing"
)]
#[derive(Debug)]
enum Framing {
    Nal(NalPacketSession),
    Adts(AacAdtsParameters),
    Packet,
}

/// The framing of one track's packets for its decoder.
#[derive(Debug)]
pub struct AccessUnits {
    pool: Budget,
    limits: AccessUnitLimits,
    framing: Framing,
}

#[allow(
    clippy::large_enum_variant,
    reason = "one setup per track, polled in place"
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
    /// Starts framing the packets of `track`, with buffers charged to `pool`, from its
    /// configuration record: the AVC or HEVC decoder configuration record, or the AAC
    /// AudioSpecificConfig. A track without a record is taken to carry Annex B or ADTS
    /// already, as an MPEG-TS track does. Fails with `Limit` for a record above
    /// `limits`, or as the record's parser does.
    pub fn for_track(
        pool: &Budget,
        track: &dyn TrackInfo,
        limits: AccessUnitLimits,
    ) -> Result<AccessUnitsSetup> {
        let record = || -> Result<Bytes> {
            let raw = track.configuration();
            if raw.len() > limits.configuration_bytes {
                return Err(Error::Limit {
                    requested: raw.len(),
                    maximum: limits.configuration_bytes,
                });
            }
            Bytes::copy_from_slice(pool, raw)
        };
        let setup = match track.codec() {
            CodecId::H264 if !track.configuration().is_empty() => Setup::Avc(AvcConfigParser::new(
                pool,
                AvcConfigSource {
                    source: UNNAMED,
                    generation: 0,
                    offset: 0,
                },
                record()?,
                AvcConfigLimits {
                    maximum_record_bytes: limits.configuration_bytes,
                    maximum_nals: limits.configuration_nals,
                    maximum_nal_bytes: limits.configuration_bytes,
                },
            )?),
            CodecId::Hevc if !track.configuration().is_empty() => {
                Setup::Hevc(HvcConfigParser::new(
                    pool,
                    HvcConfigSource {
                        source: UNNAMED,
                        generation: 0,
                        offset: 0,
                    },
                    record()?,
                    HvcConfigLimits {
                        maximum_record_bytes: limits.configuration_bytes,
                        maximum_arrays: limits.configuration_nals,
                        maximum_nals: limits.configuration_nals,
                        maximum_nal_bytes: limits.configuration_bytes,
                    },
                )?)
            }
            CodecId::Aac if !track.configuration().is_empty() => Setup::Aac(AacAscParser::new(
                pool,
                AAC_SOURCE,
                record()?,
                AacConfigLimits {
                    maximum_record_bytes: limits.configuration_bytes,
                    maximum_pce_elements: 64,
                    maximum_comment_bytes: 255,
                },
            )?),
            _ => Setup::Ready(Some(Framing::Packet)),
        };
        Ok(AccessUnitsSetup {
            pool: pool.clone(),
            limits,
            setup,
        })
    }

    /// Starts framing `packet`, a packet of the track. Fails with `Limit` for a packet
    /// above the limits, or as the session's framing does.
    pub fn packet<'a, P: PacketInfo>(&'a mut self, packet: &'a P) -> Result<AccessUnitJob<'a, P>> {
        let payload = packet.payload();
        if payload.len() > self.limits.packet_bytes {
            return Err(Error::Limit {
                requested: payload.len(),
                maximum: self.limits.packet_bytes,
            });
        }
        let job = match &mut self.framing {
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
            Framing::Packet => Job::Packet { done: false },
        };
        Ok(AccessUnitJob { packet, job })
    }
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
                    Framing::Adts(AacAdtsParameters::from_asc(record.facts())?)
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
    Packet { done: bool },
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

/// One access unit: the bytes a decoder takes, with the track, times and trim of the
/// packet they were framed from.
#[derive(Clone, Debug)]
pub struct AccessUnit<'a, P> {
    packet: &'a P,
    bytes: Unit<'a>,
}

impl<P> AccessUnit<'_, P> {
    /// The framed bytes, shared with the framing's output; `None` for a packet taken as
    /// it is, whose bytes are the packet's own.
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
