//! Raw AC-3 and E-AC-3 syncframes (ATSC A/52).
//!
//! An access unit is an independent substream 0 frame with the dependent and further
//! independent substream frames that follow it, as FFmpeg's parser combines them.
//! Bytes between frames join the packet before them and a frame cut short by the end of
//! the input is still delivered, also as FFmpeg does.
use crate::stream::Limits;
use crate::syntax::{Gap, Length, Probe, Syntax, TrackFacts};
use triergon_core::container::CodecId;
use triergon_core::{Budget, BudgetVec, Error, Result};
use triergon_framing::{Ac3Family, Ac3FrameDetails, Ac3FrameHeader, probe_ac3_header};

/// AC-3 stream facts.
#[derive(Debug)]
pub struct Ac3 {
    first: Option<Ac3FrameHeader>,
}

impl Ac3 {
    /// The first frame's header.
    pub fn header(&self) -> Option<&Ac3FrameHeader> {
        self.first.as_ref()
    }
}

/// Whether a frame starts an access unit: any AC-3 frame, or an E-AC-3 independent
/// substream 0 frame.
fn starts_unit(header: &Ac3FrameHeader) -> bool {
    match header.details {
        Ac3FrameDetails::Ac3 { .. } => true,
        Ac3FrameDetails::Eac3 {
            stream_type,
            substream_id,
            ..
        } => stream_type != 1 && substream_id == 0,
    }
}

impl Syntax for Ac3 {
    type Header = Ac3FrameHeader;
    /// [`probe_ac3_header`] reads at most twelve bytes.
    const PROBE: usize = 12;
    const SYNC: u8 = 0x0b;
    const GAP: Gap = Gap::AppendToPrevious;
    const CONFIRM: u8 = 1;
    /// The first block of a decode overlaps with nothing.
    const PREROLL: u32 = 1;

    fn new(_budget: &Budget, _limits: &Limits) -> Ac3 {
        Ac3 { first: None }
    }

    fn probe(&self, bytes: &[u8]) -> Probe<Ac3FrameHeader> {
        if bytes.first().is_some_and(|&b| b != 0x0b) || bytes.get(1).is_some_and(|&b| b != 0x77) {
            return Probe::Invalid;
        }
        match probe_ac3_header(bytes) {
            Ok(header) => Probe::Header(header),
            Err(Error::Truncated) => Probe::Short,
            Err(_) => Probe::Invalid,
        }
    }

    fn length(&self, header: &Ac3FrameHeader) -> Length {
        Length::Known(header.frame_bytes)
    }

    fn continues(&self, reference: &Ac3FrameHeader, next: &Ac3FrameHeader) -> bool {
        reference.sample_rate_hz == next.sample_rate_hz
    }

    fn joins(&self, _unit: &Ac3FrameHeader, next: &Ac3FrameHeader) -> bool {
        !starts_unit(next)
    }

    fn samples(&self, header: &Ac3FrameHeader) -> u32 {
        u32::from(header.coded_samples())
    }

    fn adopt(&mut self, header: &Ac3FrameHeader, budget: &Budget) -> Result<TrackFacts> {
        self.first = Some(*header);
        let (codec_tag, codec): (&'static [u8], _) = match header.family {
            Ac3Family::Ac3 => (b"ac-3", CodecId::Ac3),
            Ac3Family::Eac3 => (b"ec-3", CodecId::Eac3),
        };
        Ok(TrackFacts {
            codec_tag,
            codec,
            sample_rate: header.sample_rate_hz,
            channels: header
                .acmod_full_bandwidth_channels()
                .map(|channels| channels + u8::from(header.lfe_on)),
            configuration: BudgetVec::new(budget),
            duration: None,
        })
    }

    fn reset(&mut self) {
        self.first = None;
    }
}

impl crate::Format for Ac3 {}
