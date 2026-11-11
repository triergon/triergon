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
use triergon_codec::{AudioDecoder, Budget, DecodeFailure, DownmixLevels};

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Live storage that depends on the stream: element and coupling-channel state
    /// (overlap, prediction, LTP, spectra, SBR and PS) and the output frame the decoder
    /// fills. The default holds 48 elements (16 tags each of SCE, CPE and LFE, the most a
    /// PCE switch between two layouts holds) with SBR and Main prediction, 16 coupling
    /// channels and four raw data blocks of 93 channels.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 32 * 1024 * 1024,
        }
    }
}

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
    /// A decoder with a pool of the default limit.
    pub fn new() -> Self {
        #[allow(clippy::expect_used)] // The default limit is far above a pool's own charge.
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        Decoder {
            core: decoder::Core::new(budget),
        }
    }

    /// Decode exactly one ADTS frame (header included). `out` is cleared and refilled with
    /// this frame's output (1024 or, with SBR, 2048 samples per raw_data_block). On error
    /// `out` is left empty; the decoder stays usable for the next frame.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        decode_with::<false>(&mut self.core, out, |core, out| decode_adts(core, au, out))
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

/// Decoder for one raw AAC-LC access unit per packet with a fixed layout.
/// Supply the indexed core rate and channel configuration from a checked
/// AudioSpecificConfig with 1024 core samples and no core dependency.
#[derive(Debug)]
pub struct RawLcDecoder {
    core: decoder::Core,
}

impl RawLcDecoder {
    /// Maximum raw block size, including ancillary and fill data.
    pub const MAX_ACCESS_UNIT_BYTES: usize = 64 << 10;

    /// Set up the fixed layout and charge its decoder state to `budget`.
    pub fn new_in(
        frequency_index: u8,
        channel_configuration: u8,
        budget: &Budget,
    ) -> Result<Self, DecodeError> {
        if !matches!(channel_configuration, 1..=7 | 11 | 12 | 14) {
            return Err(DecodeError::InvalidData("raw AAC channel configuration"));
        }
        let mut core = decoder::Core::new(budget);
        core.configure(2, frequency_index, channel_configuration, true)?;
        Ok(Self { core })
    }

    /// Decode one complete raw data block from the container sample.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        decode_with::<true>(&mut self.core, out, |core, out| {
            if au.len() > Self::MAX_ACCESS_UNIT_BYTES {
                return Err(DecodeError::ResourceLimit("raw AAC access unit"));
            }
            decode_blocks::<true>(core, au, 1, false, out)
        })
    }
}

impl AudioDecoder for RawLcDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        RawLcDecoder::decode(self, au, out).map_err(DecodeFailure::from_error)?;
        Ok(None)
    }

    fn reset(&mut self) {
        self.core.reset_history();
    }
}

fn decode_with<const RAW: bool>(
    core: &mut decoder::Core,
    out: &mut AudioFrame,
    decode: impl FnOnce(&mut decoder::Core, &mut AudioFrame) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    if RAW {
        // The fixed decoder overwrites the entire reserved frame on success.
        out.samples_per_channel = 0;
    } else {
        out.clear();
    }
    // Restore delay and random state on a refused access unit.
    let (random, skip_samples) = (core.random, core.skip_samples);
    let coupling_elements = core.cce.len();
    let result = decode(core, out);
    if result.is_err() {
        core.random = random;
        core.skip_samples = skip_samples;
        if RAW {
            core.cce.truncate(coupling_elements);
        }
        out.clear();
    }
    result
}

fn decode_adts(
    core: &mut decoder::Core,
    au: &[u8],
    out: &mut AudioFrame,
) -> Result<(), DecodeError> {
    let h = AdtsHeader::parse(au)?;
    if au.len() < h.frame_length {
        return Err(DecodeError::Truncated);
    }
    core.configure(h.profile + 1, h.sf_index, h.channel_config, false)?;
    let mut header_bytes = 7;
    if !h.protection_absent {
        // raw_data_block_position[1..N] (16 bits each) and crc_check (16 bits).
        header_bytes += 2 * (usize::from(h.raw_data_blocks) - 1) + 2;
    }
    if h.frame_length < header_bytes {
        return Err(DecodeError::InvalidData("ADTS frame length"));
    }
    decode_blocks::<false>(
        core,
        &au[header_bytes..h.frame_length],
        h.raw_data_blocks,
        !h.protection_absent && h.raw_data_blocks > 1,
        out,
    )
}

fn decode_blocks<const RAW: bool>(
    core: &mut decoder::Core,
    payload: &[u8],
    blocks: u8,
    per_block_crc: bool,
    out: &mut AudioFrame,
) -> Result<(), DecodeError> {
    #[cfg(feature = "inspect")]
    core.sbr_fil_ranges.clear();
    let mut br = CachedBitReader::new(payload);
    core.reserve_output::<RAW>(out, usize::from(blocks))?;
    for block in 0..blocks {
        let start = br.position();
        core.decode_block::<RAW>(&mut br, start, out, usize::from(blocks - block))?;
        if !RAW {
            core.emit_planes(out)?;
        }
        if per_block_crc {
            br.skip(16);
        }
    }
    if core.skip_samples > 0 {
        let samples = out.planes.first().map_or(0, |p| p.len());
        let skip = core.skip_samples.min(samples);
        for plane in &mut out.planes {
            plane.drain(..skip);
        }
        core.skip_samples -= skip;
    }
    core.frames_out += 1;
    out.sample_rate = core.output_rate();
    out.layout = core.layout;
    out.samples_per_channel = out.planes.first().map_or(0, |p| p.len());
    Ok(())
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
