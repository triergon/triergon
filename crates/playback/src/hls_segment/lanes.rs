use super::HlsSegmentLimits;
use crate::{
    TsNativeCarriage, TsNativeConfiguration,
    hls_aac::{HlsAac, HlsAacFrame},
    hls_avc::{AvcRaster, AvcTsAccessUnit, AvcTsClock, AvcTsReader, AvcTsStep},
    hls_dolby::{HlsDolby, HlsDolbyFrame},
    hls_hevc::{HlsHevc, HlsHevcPicture},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, Result, Time, WorkBudget};
use triergon_mpegts::{
    AssemblyStep,
    compat::pes::{DemuxPacket, DemuxTimeView, RegisteredCarriage},
};

/// The legacy AVC/AAC reader uses concrete lanes. Only the codec-negotiated
/// reader needs the variant dispatch or larger lane and output storage.
pub(super) trait LaneAssembler: Sized {
    type Unit;

    fn bind(pool: &Budget, config: TsNativeConfiguration, limits: HlsSegmentLimits)
    -> Result<Self>;
    fn push(&mut self, packet: &mut Option<DemuxPacket>, clock: DemuxTimeView) -> Result<bool>;
    fn finish(&mut self);
    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Self::Unit>>;
    fn unit_bytes(unit: &Self::Unit) -> usize;
}

pub(super) trait VideoPicture {
    fn raster(&self) -> AvcRaster;
    fn pts90(&self) -> Option<i64>;
}

impl VideoPicture for AvcTsAccessUnit {
    fn raster(&self) -> AvcRaster {
        self.raster
    }

    fn pts90(&self) -> Option<i64> {
        self.clock.and_then(|clock| clock.pts90)
    }
}

impl LaneAssembler for AvcTsReader {
    type Unit = AvcTsAccessUnit;

    fn bind(
        pool: &Budget,
        config: TsNativeConfiguration,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        Self::new(pool, config, limits.avc)
    }

    fn push(&mut self, packet: &mut Option<DemuxPacket>, clock: DemuxTimeView) -> Result<bool> {
        Self::push(self, packet, clock)
    }

    fn finish(&mut self) {
        Self::finish(self);
    }

    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Self::Unit>> {
        Ok(match self.step(work, cancel)? {
            AvcTsStep::Ready(unit) => AssemblyStep::Output(unit),
            AvcTsStep::NeedInput => AssemblyStep::NeedPayload,
            AvcTsStep::Yield => AssemblyStep::Yield,
            AvcTsStep::End => AssemblyStep::Finished,
        })
    }

    fn unit_bytes(unit: &Self::Unit) -> usize {
        unit.feed_spans().map(<[u8]>::len).sum()
    }
}

impl LaneAssembler for HlsAac {
    type Unit = HlsAacFrame;

    fn bind(pool: &Budget, config: TsNativeConfiguration, _: HlsSegmentLimits) -> Result<Self> {
        Self::new(pool, config, 8191)
    }

    fn push(&mut self, packet: &mut Option<DemuxPacket>, clock: DemuxTimeView) -> Result<bool> {
        Self::push(self, packet, clock)
    }

    fn finish(&mut self) {
        Self::finish(self);
    }

    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Self::Unit>> {
        Self::poll(self, work, cancel)
    }

    fn unit_bytes(unit: &Self::Unit) -> usize {
        unit.bytes.len()
    }
}

enum Picture {
    Avc(AvcTsAccessUnit),
    Hevc(HlsHevcPicture),
}

pub struct HlsMediaVideoUnit(Picture);

impl HlsMediaVideoUnit {
    pub fn clock(&self) -> Option<AvcTsClock> {
        match &self.0 {
            Picture::Avc(unit) => unit.clock,
            Picture::Hevc(unit) => unit.clock,
        }
    }

    pub fn random_access(&self) -> bool {
        match &self.0 {
            Picture::Avc(unit) => unit.idr,
            Picture::Hevc(unit) => unit.random_access,
        }
    }

    /// Borrow the complete Annex B feed, including inserted parameter sets.
    pub fn feed_spans(&self) -> impl Iterator<Item = &[u8]> {
        let (avc, hevc) = match &self.0 {
            Picture::Avc(unit) => (Some(unit), None),
            Picture::Hevc(unit) => (None, Some(unit)),
        };
        avc.into_iter()
            .flat_map(AvcTsAccessUnit::feed_spans)
            .chain(hevc.into_iter().flat_map(HlsHevcPicture::feed_spans))
    }

    pub fn contiguous_feed(&self, pool: &Budget) -> Result<Bytes> {
        match &self.0 {
            Picture::Avc(unit) => unit.contiguous_feed(pool),
            Picture::Hevc(unit) => unit.contiguous_feed(pool),
        }
    }
}

impl VideoPicture for HlsMediaVideoUnit {
    fn raster(&self) -> AvcRaster {
        match &self.0 {
            Picture::Avc(unit) => unit.raster,
            Picture::Hevc(unit) => unit.raster,
        }
    }

    fn pts90(&self) -> Option<i64> {
        self.clock().and_then(|clock| clock.pts90)
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "retain the selected video assembler inline in the bounded session"
)]
pub(super) enum VideoAssembler {
    Avc(AvcTsReader),
    Hevc(HlsHevc),
}

impl LaneAssembler for VideoAssembler {
    type Unit = HlsMediaVideoUnit;

    fn bind(
        pool: &Budget,
        config: TsNativeConfiguration,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        match config.carriage {
            TsNativeCarriage::Registered(RegisteredCarriage::Avc) => {
                AvcTsReader::bind(pool, config, limits).map(Self::Avc)
            }
            TsNativeCarriage::Registered(RegisteredCarriage::Hevc) => {
                HlsHevc::new(pool, config, limits.avc).map(Self::Hevc)
            }
            _ => Err(Error::Unsupported("HLS video carriage")),
        }
    }

    fn push(&mut self, packet: &mut Option<DemuxPacket>, clock: DemuxTimeView) -> Result<bool> {
        match self {
            Self::Avc(reader) => reader.push(packet, clock),
            Self::Hevc(reader) => reader.push(packet, clock),
        }
    }

    fn finish(&mut self) {
        match self {
            Self::Avc(reader) => reader.finish(),
            Self::Hevc(reader) => reader.finish(),
        }
    }

    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Self::Unit>> {
        match self {
            Self::Avc(reader) => Ok(map_output(reader.poll(work, cancel)?, |unit| {
                HlsMediaVideoUnit(Picture::Avc(unit))
            })),
            Self::Hevc(reader) => Ok(map_output(reader.poll(work, cancel)?, |unit| {
                HlsMediaVideoUnit(Picture::Hevc(unit))
            })),
        }
    }

    fn unit_bytes(unit: &Self::Unit) -> usize {
        match &unit.0 {
            Picture::Avc(unit) => AvcTsReader::unit_bytes(unit),
            Picture::Hevc(unit) => unit.feed_size(),
        }
    }
}

/// Complete feed bytes and the exact audio frame clock before playlist mapping.
pub struct HlsMediaAudioUnit {
    pub bytes: Bytes,
    pub pts: Option<Time>,
    pub dts: Option<Time>,
    pub duration: Time,
}

impl From<HlsAacFrame> for HlsMediaAudioUnit {
    fn from(frame: HlsAacFrame) -> Self {
        Self {
            bytes: frame.bytes,
            pts: frame.pts,
            dts: frame.dts,
            duration: frame.duration,
        }
    }
}

impl From<HlsDolbyFrame> for HlsMediaAudioUnit {
    fn from(frame: HlsDolbyFrame) -> Self {
        Self {
            bytes: frame.bytes,
            pts: frame.pts,
            dts: frame.dts,
            duration: frame.duration,
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "retain the selected audio assembler inline in the bounded session"
)]
pub(super) enum AudioAssembler {
    Aac(HlsAac),
    Dolby(HlsDolby),
}

impl LaneAssembler for AudioAssembler {
    type Unit = HlsMediaAudioUnit;

    fn bind(
        pool: &Budget,
        config: TsNativeConfiguration,
        limits: HlsSegmentLimits,
    ) -> Result<Self> {
        match config.carriage {
            TsNativeCarriage::Registered(RegisteredCarriage::AacAdts) => {
                HlsAac::bind(pool, config, limits).map(Self::Aac)
            }
            TsNativeCarriage::PrivateAudio(_) => HlsDolby::new(pool, config).map(Self::Dolby),
            _ => Err(Error::Unsupported("HLS audio carriage")),
        }
    }

    fn push(&mut self, packet: &mut Option<DemuxPacket>, clock: DemuxTimeView) -> Result<bool> {
        match self {
            Self::Aac(reader) => reader.push(packet, clock),
            Self::Dolby(reader) => reader.push(packet, clock),
        }
    }

    fn finish(&mut self) {
        match self {
            Self::Aac(reader) => reader.finish(),
            Self::Dolby(reader) => reader.finish(),
        }
    }

    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<Self::Unit>> {
        match self {
            Self::Aac(reader) => Ok(map_output(reader.poll(work, cancel)?, Into::into)),
            Self::Dolby(reader) => Ok(map_output(reader.poll(work, cancel)?, Into::into)),
        }
    }

    fn unit_bytes(unit: &Self::Unit) -> usize {
        unit.bytes.len()
    }
}

fn map_output<T, U>(step: AssemblyStep<T>, map: impl FnOnce(T) -> U) -> AssemblyStep<U> {
    match step {
        AssemblyStep::Output(unit) => AssemblyStep::Output(map(unit)),
        AssemblyStep::NeedPayload => AssemblyStep::NeedPayload,
        AssemblyStep::Yield => AssemblyStep::Yield,
        AssemblyStep::Finished => AssemblyStep::Finished,
    }
}
