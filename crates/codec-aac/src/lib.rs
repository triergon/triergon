//! MPEG-4 AAC decoder for ADTS access units: AAC Main / LC / LTP core, HE-AAC (SBR,
//! implicit signalling) and HE-AAC v2 (Parametric Stereo). Pure safe Rust, no dependencies.
//!
//! Output matches FFmpeg's native float AAC decoder conventions (sample rate after implicit
//! SBR, plane order = FFmpeg native channel order, float full scale 1.0). Not qualified;
//! see `research/d4/aac/STATUS.md`.
#![forbid(unsafe_code)]

pub mod adts;
mod bits;
mod cce;
mod decoder;
mod error;
mod fft;
mod filterbank;
mod huffman;
mod huffman_tables;
mod ics;
pub mod layout;
mod ltp;
mod pce;
mod pred;
mod sbr;
mod tables;

pub use adts::AdtsHeader;
pub use error::{DecodeError, Feature};
pub use layout::{ChannelLayout, Speaker};

use bits::BitReader;

/// Decoded PCM for one access unit, planar float (full scale 1.0).
#[derive(Clone, Debug, Default)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub layout: ChannelLayout,
    pub samples_per_channel: usize,
    /// One plane per channel, in `layout` order (FFmpeg native order).
    pub planes: Vec<Vec<f32>>,
}

/// ADTS AAC decoder.
pub struct Decoder {
    core: decoder::Core,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            core: decoder::Core::new(),
        }
    }

    /// Decode exactly one ADTS frame (header included). `out` is cleared and refilled with
    /// this frame's output (1024 or, with SBR, 2048 samples per raw_data_block). On error
    /// `out` is left empty; the decoder stays usable for the next frame.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        for p in &mut out.planes {
            p.clear();
        }
        out.samples_per_channel = 0;
        let r = self.decode_inner(au, out);
        if r.is_err() {
            for p in &mut out.planes {
                p.clear();
            }
            out.samples_per_channel = 0;
        }
        r
    }

    fn decode_inner(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let h = AdtsHeader::parse(au)?;
        if au.len() < h.frame_length {
            return Err(DecodeError::Truncated);
        }
        let core = &mut self.core;
        core.configure_adts(&h)?;
        let mut hdr = 7;
        if !h.protection_absent {
            // raw_data_block_position[1..N] (16 bit each) + crc_check (16)
            hdr += 2 * (h.raw_data_blocks as usize - 1) + 2;
        }
        if h.frame_length < hdr {
            return Err(DecodeError::InvalidData("ADTS frame length"));
        }
        let payload = &au[hdr..h.frame_length];
        #[cfg(feature = "inspect")]
        core.sbr_fil_ranges.clear();
        let mut br = BitReader::new(payload);
        // planes sized for the current layout; a mid-frame layout change restarts collection
        let mut planes: Vec<Vec<f32>> = std::mem::take(&mut out.planes);
        for _ in 0..h.raw_data_blocks {
            let start = br.position();
            core.decode_block(&mut br, start)?;
            core.emit_planes(&mut planes);
            if !h.protection_absent && h.raw_data_blocks > 1 {
                br.skip(16); // adts_raw_data_block_error_check
            }
        }
        if core.skip_samples > 0 {
            let n = planes.first().map_or(0, |p| p.len());
            let k = core.skip_samples.min(n);
            for p in &mut planes {
                p.drain(..k);
            }
            core.skip_samples -= k;
        }
        core.frames_out += 1;
        out.planes = planes;
        out.sample_rate = core.output_rate();
        out.layout = core.layout.clone();
        out.samples_per_channel = out.planes.first().map_or(0, |p| p.len());
        Ok(())
    }

    /// Reset overlap, prediction, PNS and SBR/PS signal history after a discontinuity.
    pub fn reset(&mut self) {
        self.core.reset_history();
    }

    /// Development aid: payload bit ranges of the SBR fill elements of the last frame.
    #[cfg(feature = "inspect")]
    pub fn sbr_fill_ranges(&self) -> &[(usize, usize)] {
        &self.core.sbr_fil_ranges
    }

    /// Development aid: collect [`Self::stats`] from now on (it formats text per frame).
    #[cfg(feature = "inspect")]
    pub fn enable_stats(&mut self) {
        self.core.stats_enabled = true;
    }

    /// Development statistics of the last decoded block (unstable format).
    #[cfg(feature = "inspect")]
    pub fn stats(&self) -> &str {
        &self.core.stats
    }

    /// Current output sample rate (0 before the first frame).
    pub fn sample_rate(&self) -> u32 {
        if self.core.configured {
            self.core.output_rate()
        } else {
            0
        }
    }
}
