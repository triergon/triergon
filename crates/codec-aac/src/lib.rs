#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(feature = "simd", feature(portable_simd))]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
pub mod adts;
mod cce;
pub mod compat;
mod decoder;
mod error;
mod fft;
mod filterbank;
mod huffman;
mod huffman_probed;
mod huffman_tables;
mod ics;
#[cfg(test)]
mod inspect;
mod ltp;
mod pce;
mod pred;
mod sbr;
mod tables;

pub use adts::AdtsHeader;
pub use error::{DecodeError, Feature};
pub use triergon_codec::{AudioFrame, Budget, ChannelLayout, Error, Speaker};

use triergon_bitstream::CachedBitReader;
use triergon_codec::{AudioDecoder, DownmixLevels};

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of storage that depends on the stream: element and coupling-channel state
    /// (overlap, prediction, LTP, spectra, SBR and PS) and the output frame the decoder
    /// fills. The default, 32 MiB, holds 48 elements (16 tags each of SCE, CPE and LFE,
    /// the most a PCE switch between two layouts holds) with SBR and Main prediction, 16
    /// coupling channels and four raw data blocks of 93 channels.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 32 << 20,
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
        #[expect(
            clippy::expect_used,
            reason = "the default limit is far above a pool's own charge"
        )]
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`. Fails only for a
    /// limit below a pool's own charge.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        Decoder {
            core: decoder::Core::new(budget),
        }
    }

    /// Configure how this ADTS decoder handles an encoder delay signature.
    /// The policy persists across discontinuities.
    #[must_use]
    pub fn with_priming_policy(mut self, policy: compat::ffmpeg::priming::Policy) -> Self {
        self.core.priming_policy = policy;
        self.core.skip_samples = 0;
        self
    }

    /// Decodes one ADTS frame including its header, yielding 1024 samples per raw data
    /// block or 2048 with spectral band replication.
    /// Replaces `out`, leaving `samples_per_channel == 0` on refusal.
    ///
    /// # Errors
    ///
    /// Refuses malformed, truncated or unsupported input and storage charges the pool
    /// cannot cover. The decoder stays usable for the next frame.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        decode_with::<false>(&mut self.core, out, |core, out| decode_adts(core, au, out))
    }

    /// Reset overlap, prediction, PNS and SBR/PS signal history after a discontinuity.
    pub fn reset(&mut self) {
        self.core.reset_history();
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
    type Error = DecodeError;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeError> {
        Decoder::decode(self, au, out)?;
        Ok(None)
    }

    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

/// Decoder for one raw AAC-LC access unit per packet.
/// Supply the indexed core rate and layout from a checked AudioSpecificConfig
/// with 1024 core samples and no core dependency.
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
        Self::with_layout_in(
            frequency_index,
            channel_configuration,
            compat::ffmpeg::layout::Policy::Specification,
            budget,
        )
    }

    /// Set up the configured decoder with an explicit speaker mapping.
    pub fn with_layout_in(
        frequency_index: u8,
        channel_configuration: u8,
        layout: compat::ffmpeg::layout::Policy,
        budget: &Budget,
    ) -> Result<Self, DecodeError> {
        if !matches!(channel_configuration, 1..=7 | 11 | 12 | 14) {
            return Err(DecodeError::Invalid("raw AAC channel configuration"));
        }
        let mut core = decoder::Core::new(budget);
        core.configure(
            2,
            frequency_index,
            channel_configuration,
            decoder::FixedLayout::Raw(layout),
        )?;
        Ok(Self { core })
    }

    /// Set up an explicit program layout from the byte-aligned PCE in an
    /// AudioSpecificConfig with channel configuration zero. Pass the complete
    /// `program_config_element()` field through its comment, starting at its tag.
    pub fn with_program_config_in(
        frequency_index: u8,
        program_config: &[u8],
        budget: &Budget,
    ) -> Result<Self, DecodeError> {
        let mut reader = CachedBitReader::new(program_config);
        let program = pce::parse_pce(&mut reader, 0)?;
        if reader.left() != 0 {
            return Err(DecodeError::Invalid("raw AAC program extent"));
        }
        let core = decoder::Core::with_program(budget, frequency_index, &program)?;
        Ok(Self { core })
    }

    /// Decodes one complete raw data block from a container sample.
    /// Replaces `out`, leaving `samples_per_channel == 0` on refusal.
    ///
    /// # Errors
    ///
    /// Refuses malformed or unsupported raw blocks and blocks exceeding the byte or
    /// storage limits.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        decode_with::<true>(&mut self.core, out, |core, out| {
            if au.len() > Self::MAX_ACCESS_UNIT_BYTES {
                return Err(DecodeError::Policy("raw AAC access unit"));
            }
            decode_blocks::<true>(core, au, 1, false, out)
        })
    }

    /// Reset overlap, prediction and PNS signal history after a discontinuity.
    pub fn reset(&mut self) {
        self.core.reset_history();
    }
}

impl AudioDecoder for RawLcDecoder {
    type Error = DecodeError;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeError> {
        RawLcDecoder::decode(self, au, out)?;
        Ok(None)
    }

    fn reset(&mut self) {
        RawLcDecoder::reset(self);
    }
}

fn decode_with<const RAW: bool>(
    core: &mut decoder::Core,
    out: &mut AudioFrame,
    decode: impl FnOnce(&mut decoder::Core, &mut AudioFrame) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    if RAW {
        // The raw decoder overwrites the entire reserved frame on success.
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
    core.configure(
        h.profile + 1,
        h.sf_index,
        h.channel_config,
        decoder::FixedLayout::Adts,
    )?;
    let mut header_bytes = 7;
    if !h.protection_absent {
        // raw_data_block_position[1..N] (16 bits each) and crc_check (16 bits).
        header_bytes += 2 * (usize::from(h.raw_data_blocks) - 1) + 2;
    }
    if h.frame_length < header_bytes {
        return Err(DecodeError::Invalid("ADTS frame length"));
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
        let samples = out.planes.first().map_or(0, Vec::len);
        let skip = core.skip_samples.min(samples);
        for plane in &mut out.planes {
            plane.drain(..skip);
        }
        core.skip_samples -= skip;
    }
    core.frames_out += 1;
    out.sample_rate = core.output_rate();
    out.layout = core.layout;
    out.samples_per_channel = out.planes.first().map_or(0, Vec::len);
    Ok(())
}
