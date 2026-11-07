#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr
)]
// Pending: `missing_docs`, `clippy::unwrap_used`, `clippy::expect_used`.
// Pending: these rewrites change the generated code of measured hot paths and are
// adopted with an A/B timing run.
#![allow(
    clippy::bool_to_int_with_if,
    clippy::needless_for_each,
    clippy::nonminimal_bool,
    clippy::redundant_closure_for_method_calls,
    clippy::single_match_else,
    clippy::unnested_or_patterns
)]
pub mod adts;
mod cce;
mod decoder;
mod error;
mod fft;
mod filterbank;
mod huffman;
mod huffman_tables;
mod ics;
mod ltp;
mod pce;
mod pred;
mod sbr;
mod tables;

pub use adts::AdtsHeader;
pub use error::{DecodeError, Feature};
pub use triergon_codec::{AudioFrame, ChannelLayout, Speaker};

use triergon_bitstream::CachedBitReader;
use triergon_codec::{AudioDecoder, DecodeFailure, DownmixLevels};

/// ADTS AAC decoder.
#[derive(Debug)]
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
        let mut br = CachedBitReader::new(payload);
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
        out.layout = core.layout;
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

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        Decoder::decode(self, au, out).map_err(DecodeFailure::from_error)?;
        Ok(None)
    }
    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

/// FFmpeg's name for `layout` where one exists, else `"N channels (A+B+...)"` as
/// ffprobe prints it.
pub fn layout_name(layout: &ChannelLayout) -> String {
    const NAMED: &[(u64, &str)] = &[
        (0x4, "mono"),
        (0x3, "stereo"),
        (0x7, "3.0"),
        (0x107, "4.0"),
        (0x37, "5.0(back)"),
        (0x607, "5.0"),
        (0x3f, "5.1(back)"),
        (0x60f, "5.1"),
        (0xff, "7.1(wide)"),
        (0x63f, "7.1"),
        (0x503f, "5.1.2(back)"),
        (0x13, "3.0(back)"),
        (0x33, "quad"),
        (0x603, "quad(side)"),
        (0xb, "2.1"),
        (0x70f, "6.1"),
        (0x13f, "6.1(back)"),
    ];
    let mask = layout.mask();
    if let Some((_, name)) = NAMED.iter().find(|(named, _)| *named == mask) {
        return (*name).to_string();
    }
    let names: Vec<&str> = layout.speakers().iter().map(|s| s.name()).collect();
    format!("{} channels ({})", names.len(), names.join("+"))
}
