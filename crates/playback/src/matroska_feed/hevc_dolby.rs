//! HEVC Annex B and AC-3 family feeds with borrowed packet storage.

mod clock;
mod dolby_clock;
pub use clock::MatroskaHevcClock;
pub use dolby_clock::{
    MatroskaDolbyClock, MatroskaDolbyClockPrefix, MatroskaDolbyClockProbe, MatroskaDolbyTiming,
};

use super::{Association, check_limit, probe_clock::syncframe_duration_ns};
use crate::{Output, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    Ac3Family, Ac3FrameDetails, Ac3FrameHeader, BuildStep, HevcAnnexBAccessUnit, HevcAnnexBConfig,
    HevcAnnexBJob, HevcAnnexBLimits, HvcConfigLimits, HvcConfigSource, HvcConfigViewParser,
    probe_ac3_header,
};
use triergon_matroska::typed::{Configuration, Packet};

#[derive(Clone, Copy, Debug)]
pub struct MatroskaHevcDolbyLimits {
    pub hevc_config: HvcConfigLimits,
    pub hevc_packet: HevcAnnexBLimits,
    pub dolby_packet: MatroskaDolbyLimits,
}

#[derive(Clone, Copy, Debug)]
pub struct MatroskaDolbyLimits {
    pub maximum_packet_bytes: usize,
    pub maximum_syncframes: usize,
}

#[derive(Clone, Copy)]
enum DolbyCodec {
    Ac3,
    Ac3Bsid(u8),
    Eac3,
}

impl DolbyCodec {
    fn family(self) -> Ac3Family {
        match self {
            Self::Ac3 | Self::Ac3Bsid(_) => Ac3Family::Ac3,
            Self::Eac3 => Ac3Family::Eac3,
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "the bounded inline parser avoids a separate uncharged setup allocation"
)]
enum SetupCodec {
    Hevc(HvcConfigViewParser),
    Dolby(DolbyCodec),
}

/// Bind one selected HEVC, AC-3 or E-AC-3 track to its current configuration.
/// Dolby syncframes are self-describing. Any codec private data stays in the
/// source configuration for the consumer.
pub struct MatroskaHevcDolbySetup {
    association: Option<Association>,
    codec: SetupCodec,
    limits: MatroskaHevcDolbyLimits,
}

impl MatroskaHevcDolbySetup {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        configuration: &Configuration,
        track: usize,
        limits: MatroskaHevcDolbyLimits,
    ) -> Result<Self> {
        if !configuration.belongs_to(pool) {
            return Err(Error::ForeignPool.into());
        }
        let descriptor = configuration
            .tracks()
            .get(track)
            .ok_or(Error::Invalid("Matroska feed track index"))?;
        let codec = match (descriptor.kind, descriptor.codec.as_str()) {
            (1, "V_MPEGH/ISO/HEVC") => {
                let init = configuration
                    .initialization(track)
                    .ok_or(Error::Invalid("missing Matroska initialization"))?;
                check_limit(init.data.len(), limits.hevc_config.maximum_record_bytes)?;
                let record = match configuration.initialization_bytes(track) {
                    Some(record) => record,
                    None => Bytes::copy_from_slice(pool, init.data)?,
                };
                SetupCodec::Hevc(HvcConfigViewParser::new(
                    pool,
                    HvcConfigSource {
                        source,
                        generation: init.generation,
                        offset: 0,
                    },
                    record,
                    limits.hevc_config,
                )?)
            }
            (2, "A_AC3") => SetupCodec::Dolby(DolbyCodec::Ac3),
            (2, "A_AC3/BSID9") => SetupCodec::Dolby(DolbyCodec::Ac3Bsid(9)),
            (2, "A_AC3/BSID10") => SetupCodec::Dolby(DolbyCodec::Ac3Bsid(10)),
            (2, "A_EAC3") => SetupCodec::Dolby(DolbyCodec::Eac3),
            _ => return Err(Error::Unsupported("Matroska HEVC/Dolby feed codec").into()),
        };
        Ok(Self {
            association: Some(Association {
                configuration: configuration.clone(),
                track,
            }),
            codec,
            limits,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<MatroskaHevcDolbyFeed>> {
        if self.association.is_none() {
            return Ok(BuildStep::Complete);
        }
        match self.step_codec(work, cancel) {
            Ok(Some(codec)) => {
                let association = self
                    .association
                    .take()
                    .ok_or(Error::Invalid("HEVC/Dolby setup consumed"))?;
                Ok(BuildStep::Ready(MatroskaHevcDolbyFeed {
                    association,
                    codec,
                    limits: self.limits,
                }))
            }
            Ok(None) => Ok(BuildStep::Yield),
            Err(error) => {
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    self.association = None;
                }
                Err(error.into())
            }
        }
    }

    fn step_codec(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<Option<FeedCodec>> {
        match &mut self.codec {
            SetupCodec::Hevc(parser) => match parser.step(work, cancel)? {
                BuildStep::Ready(record) => {
                    Ok(Some(FeedCodec::Hevc(HevcAnnexBConfig::new(record)?)))
                }
                BuildStep::Yield => Ok(None),
                BuildStep::Complete => Err(Error::Invalid("HEVC setup lost its configuration")),
            },
            SetupCodec::Dolby(codec) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                Ok(work.try_charge(1).then_some(FeedCodec::Dolby(*codec)))
            }
        }
    }
}

enum FeedCodec {
    Hevc(HevcAnnexBConfig),
    Dolby(DolbyCodec),
}

pub struct MatroskaHevcDolbyFeed {
    association: Association,
    codec: FeedCodec,
    limits: MatroskaHevcDolbyLimits,
}

impl MatroskaHevcDolbyFeed {
    /// Use the player's current-generation and pause checks before submitting.
    pub fn packet<'a>(&'a self, output: &'a Output) -> Result<Option<MatroskaHevcDolbyJob<'a>>> {
        let Some(source) = self.association.selected_packet(output)? else {
            return Ok(None);
        };
        let data = source.packet.data.as_ref();
        let job = match &self.codec {
            FeedCodec::Hevc(config) => PacketJob::Hevc(config.job(data, self.limits.hevc_packet)?),
            FeedCodec::Dolby(codec) => {
                PacketJob::Dolby(DolbyJob::new(*codec, data, self.limits.dolby_packet)?)
            }
        };
        Ok(Some(MatroskaHevcDolbyJob { source, job }))
    }
}

enum PacketJob<'a> {
    Hevc(HevcAnnexBJob<'a>),
    Dolby(DolbyJob<'a>),
}

pub struct MatroskaHevcDolbyJob<'a> {
    source: &'a Packet,
    job: PacketJob<'a>,
}

impl<'a> MatroskaHevcDolbyJob<'a> {
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<MatroskaHevcDolbyPacket<'a>>> {
        let payload = match &mut self.job {
            PacketJob::Hevc(job) => match job.step(work, cancel)? {
                BuildStep::Ready(unit) => MatroskaHevcDolbyPayload::Hevc(unit),
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
            PacketJob::Dolby(job) => match job.step(work, cancel)? {
                BuildStep::Ready(raw) => MatroskaHevcDolbyPayload::Dolby {
                    family: job.codec.family(),
                    raw,
                },
                BuildStep::Yield => return Ok(BuildStep::Yield),
                BuildStep::Complete => return Ok(BuildStep::Complete),
            },
        };
        Ok(BuildStep::Ready(MatroskaHevcDolbyPacket {
            source: self.source,
            payload,
        }))
    }
}

pub struct MatroskaHevcDolbyPacket<'a> {
    pub source: &'a Packet,
    pub payload: MatroskaHevcDolbyPayload<'a>,
}

impl MatroskaHevcDolbyPacket<'_> {
    /// Return the declared duration on the native packet clock, or project
    /// the final CRC-valid independent syncframe of an admitted Dolby packet.
    /// The fallback checks up to 32 complete syncframes. The typed source
    /// keeps its original container timing.
    pub fn native_duration_ns(&self) -> Result<Option<u64>> {
        let timing = self.source.packet.timing;
        if let Some(declared) = timing.ffmpeg9()?.duration {
            return Ok(Some(declared.rounded_duration_ns()?));
        }
        let MatroskaHevcDolbyPayload::Dolby { family, raw } = self.payload else {
            return Ok(None);
        };
        Ok(dolby_duration_ns(family, raw, timing.scale_ns))
    }
}

fn dolby_duration_ns(family: Ac3Family, raw: &[u8], scale_ns: u64) -> Option<u64> {
    let frame = dolby_final_syncframe(family, raw)?;
    syncframe_duration_ns(frame.coded_samples(), frame.sample_rate_hz, scale_ns)
}

fn dolby_final_syncframe(family: Ac3Family, mut raw: &[u8]) -> Option<Ac3FrameHeader> {
    for _ in 0..32 {
        let header = probe_ac3_header(raw).ok()?;
        if header.family != family
            || matches!(
                header.details,
                Ac3FrameDetails::Eac3 {
                    stream_type: 1 | 3,
                    ..
                } | Ac3FrameDetails::Eac3 {
                    substream_id: 1..,
                    ..
                }
            )
        {
            return None;
        }
        let (frame, remaining) = raw.split_at_checked(header.frame_bytes)?;
        if remaining.is_empty() {
            // The native header parser takes its duration from the final
            // syncframe. The CRC covers bytes after the sync word.
            if !dolby_crc_valid(&frame[2..]) {
                return None;
            }
            return Some(header);
        }
        raw = remaining;
    }
    None
}

// ATSC A/52 CRC-16 with generator x^16 + x^15 + x^2 + 1.
const DOLBY_CRC: [u16; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = (index as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x8005 } else { 0 };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

fn dolby_crc_valid(data: &[u8]) -> bool {
    data.iter().fold(0u16, |crc, &byte| {
        (crc << 8) ^ DOLBY_CRC[usize::from(((crc >> 8) as u8) ^ byte)]
    }) == 0
}

pub enum MatroskaHevcDolbyPayload<'a> {
    Hevc(HevcAnnexBAccessUnit<'a>),
    Dolby { family: Ac3Family, raw: &'a [u8] },
}

impl MatroskaHevcDolbyPayload<'_> {
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Hevc(unit) => unit.byte_len(),
            Self::Dolby { raw, .. } => raw.len(),
        }
    }

    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        let (hevc, dolby) = match self {
            Self::Hevc(unit) => (Some(unit.segments()), None),
            Self::Dolby { raw, .. } => (None, Some(*raw)),
        };
        hevc.into_iter().flatten().chain(dolby)
    }
}

struct DolbyJob<'a> {
    codec: DolbyCodec,
    data: &'a [u8],
    remaining: Option<&'a [u8]>,
    frames: usize,
    maximum_syncframes: usize,
    failure: Option<Error>,
}

impl<'a> DolbyJob<'a> {
    fn new(
        codec: DolbyCodec,
        data: &'a [u8],
        limits: MatroskaDolbyLimits,
    ) -> triergon_core::Result<Self> {
        if data.is_empty() {
            return Err(Error::Invalid("empty Dolby packet"));
        }
        check_limit(data.len(), limits.maximum_packet_bytes)?;
        Ok(Self {
            codec,
            data,
            remaining: Some(data),
            frames: 0,
            maximum_syncframes: limits.maximum_syncframes,
            failure: None,
        })
    }

    fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<BuildStep<&'a [u8]>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.scan(work, cancel);
        if let Err(error) = result {
            self.remaining = None;
            self.failure = Some(error);
        }
        result
    }

    fn scan(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<BuildStep<&'a [u8]>> {
        while let Some(remaining) = self.remaining {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            self.frames = self.frames.checked_add(1).ok_or(Error::Overflow)?;
            check_limit(self.frames, self.maximum_syncframes)?;
            let header = probe_ac3_header(remaining)?;
            if header.family != self.codec.family()
                || matches!(self.codec, DolbyCodec::Ac3Bsid(bsid) if bsid != header.bsid)
            {
                return Err(Error::Invalid("Dolby syncframe disagrees with track codec"));
            }
            let rest = remaining
                .get(header.frame_bytes..)
                .ok_or(Error::Truncated)?;
            if rest.is_empty() {
                self.remaining = None;
                return Ok(BuildStep::Ready(self.data));
            }
            self.remaining = Some(rest);
        }
        Ok(BuildStep::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: MatroskaDolbyLimits = MatroskaDolbyLimits {
        maximum_packet_bytes: 8 * 1024,
        maximum_syncframes: 2,
    };

    fn eac3_frame() -> &'static [u8] {
        // The independently projected mixed fixture stores the untouched Dolby
        // packet after its 15-byte test record header.
        let mut feed = include_bytes!("../../tests/fixtures/mkv-feed/hevc-dolby.feed").as_slice();
        while let Some((header, remaining)) = feed.split_at_checked(15) {
            let size = u32::from_be_bytes(header[11..].try_into().unwrap()) as usize;
            let (payload, tail) = remaining.split_at(size);
            if header[0] == 2 {
                return payload;
            }
            feed = tail;
        }
        panic!("missing E-AC-3 test packet")
    }

    #[test]
    fn native_duration_uses_whole_ticks_of_the_final_independent_syncframe() {
        let data = eac3_frame();
        let duration = |raw: &[u8], scale| dolby_duration_ns(Ac3Family::Eac3, raw, scale);
        assert_eq!(duration(data, 1_000_000), Some(32_000_000));
        assert_eq!(duration(data, 3_000_000), Some(30_000_000));
        assert_eq!(duration(data, 64_000_000), None);
        assert_eq!(duration(&data[..data.len() - 1], 1_000_000), None);
        assert_eq!(duration(&data.repeat(2), 1_000_000), Some(32_000_000));
        assert_eq!(duration(&data.repeat(33), 1_000_000), None);
        let mut damaged = data.repeat(2);
        *damaged.last_mut().unwrap() ^= 1;
        assert_eq!(duration(&damaged, 1_000_000), None);

        let mut dependent = data.to_vec();
        dependent[2] = (dependent[2] & 0x3f) | 0x40;
        assert!(matches!(
            probe_ac3_header(&dependent).unwrap().details,
            Ac3FrameDetails::Eac3 { stream_type: 1, .. }
        ));
        assert_eq!(duration(&dependent, 1_000_000), None);
    }

    #[test]
    fn complete_dolby_packet_keeps_its_storage_and_checks_every_syncframe() {
        let packet = eac3_frame().repeat(2);
        let cancel = AtomicBool::new(false);
        let mut job = DolbyJob::new(DolbyCodec::Eac3, &packet, LIMITS).unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel),
            Ok(BuildStep::Yield)
        ));
        let BuildStep::Ready(bytes) = job.step(&mut WorkBudget::new(1), &cancel).unwrap() else {
            panic!("complete syncframes were not published");
        };
        assert!(std::ptr::eq(bytes, packet.as_slice()));
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel),
            Ok(BuildStep::Complete)
        ));

        let cases = [
            (
                DolbyCodec::Eac3,
                &packet[..packet.len() - 1],
                LIMITS,
                Error::Truncated,
            ),
            (
                DolbyCodec::Ac3,
                packet.as_slice(),
                LIMITS,
                Error::Invalid("Dolby syncframe disagrees with track codec"),
            ),
            (
                DolbyCodec::Eac3,
                packet.as_slice(),
                MatroskaDolbyLimits {
                    maximum_syncframes: 1,
                    ..LIMITS
                },
                Error::Limit {
                    requested: 2,
                    maximum: 1,
                },
            ),
        ];
        for (codec, data, limits, expected) in cases {
            let mut job = DolbyJob::new(codec, data, limits).unwrap();
            for _ in 0..2 {
                assert_eq!(
                    job.step(&mut WorkBudget::new(2), &cancel).err(),
                    Some(expected)
                );
            }
        }

        let mut job = DolbyJob::new(DolbyCodec::Eac3, &packet, LIMITS).unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel),
            Ok(BuildStep::Yield)
        ));
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancelled).err(),
            Some(Error::Cancelled)
        );
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancel).err(),
            Some(Error::Cancelled)
        );
    }
}
