//! AAC in ADTS (ISO/IEC 13818-7 section 6.2, ISO/IEC 14496-3 1.A.2).
//!
//! Packets are whole ADTS frames, header and CRC included, as FFmpeg's demuxer
//! delivers them; [`triergon_core::container::CodecId::Aac`] covers both raw and ADTS
//! access units. Bytes between frames are dropped and a frame cut short by the end of
//! the input is still delivered, also as FFmpeg does.
use crate::stream::Limits;
use crate::syntax::{Gap, Length, Probe, Syntax, TrackFacts};
use triergon_core::container::CodecId;
use triergon_core::{Budget, BudgetVec, Error, Result};
use triergon_framing::{AacAdtsHeader, inspect_aac_adts_header};

/// ADTS stream facts.
#[derive(Debug)]
pub struct Adts {
    first: Option<AacAdtsHeader>,
}

impl Adts {
    /// The first frame's header.
    pub fn header(&self) -> Option<AacAdtsHeader> {
        self.first
    }
}

impl Syntax for Adts {
    type Header = AacAdtsHeader;
    const PROBE: usize = 7;
    const SYNC: u8 = 0xff;
    const GAP: Gap = Gap::Drop;
    const CONFIRM: u8 = 1;
    /// The first frame of a decode overlaps with nothing.
    const PREROLL: u32 = 1;

    fn new(_budget: &Budget, _limits: &Limits) -> Adts {
        Adts { first: None }
    }

    fn probe(&self, bytes: &[u8]) -> Probe<AacAdtsHeader> {
        if bytes.len() < 7 {
            let sync = bytes.first().is_none_or(|&b| b == 0xff)
                && bytes.get(1).is_none_or(|&b| b & 0xf6 == 0xf0);
            return if sync { Probe::Short } else { Probe::Invalid };
        }
        let Ok(header) = inspect_aac_adts_header(bytes) else {
            return Probe::Invalid;
        };
        let header_bytes = if header.protection_absent() { 7 } else { 9 };
        let valid = header.syncword() == 0xfff
            && header.layer() == 0
            && header.coded_frequency_hz().is_some()
            && usize::from(header.frame_length()) > header_bytes;
        if valid {
            Probe::Header(header)
        } else {
            Probe::Invalid
        }
    }

    fn length(&self, header: &AacAdtsHeader) -> Length {
        Length::Known(usize::from(header.frame_length()))
    }

    fn continues(&self, reference: &AacAdtsHeader, next: &AacAdtsHeader) -> bool {
        reference.frequency_index() == next.frequency_index()
            && reference.mpeg2_id() == next.mpeg2_id()
    }

    fn samples(&self, header: &AacAdtsHeader) -> u32 {
        1024 * (u32::from(header.raw_blocks_minus_one()) + 1)
    }

    fn adopt(&mut self, header: &AacAdtsHeader, budget: &Budget) -> Result<TrackFacts> {
        self.first = Some(*header);
        Ok(TrackFacts {
            codec_tag: b"adts",
            codec: CodecId::Aac,
            sample_rate: header
                .coded_frequency_hz()
                .ok_or(Error::Invalid("ADTS sampling frequency index"))?,
            channels: header.coded_channel_count(),
            configuration: BudgetVec::new(budget),
            duration: None,
        })
    }

    fn reset(&mut self) {
        self.first = None;
    }
}

impl crate::Format for Adts {}
