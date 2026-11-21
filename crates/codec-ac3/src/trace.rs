//! Bit positions of the syntax elements the dither-stripping and patching tools rewrite
//! (feature `reference-trace`).
#![cfg_attr(
    not(feature = "reference-trace"),
    allow(
        unreachable_pub,
        dead_code,
        reason = "the trace is read only through the reference-trace API"
    )
)]

use crate::FrameFeatures;

/// Syncframe records of the last decoded access unit, when enabled with
/// `Decoder::set_trace`.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub(crate) enabled: bool,
    /// One record per decoded syncframe of the last access unit.
    pub frames: Vec<FrameTrace>,
}

impl Trace {
    /// Recording is on; always off without the `reference-trace` feature.
    #[inline(always)]
    pub(crate) fn enabled(&self) -> bool {
        cfg!(feature = "reference-trace") && self.enabled
    }

    /// The record of the syncframe being decoded, while recording.
    #[inline(always)]
    pub(crate) fn cur(&mut self) -> Option<&mut FrameTrace> {
        if self.enabled() {
            self.frames.last_mut()
        } else {
            None
        }
    }
}

/// Bit positions within one syncframe.
#[derive(Clone, Debug, Default)]
pub struct FrameTrace {
    /// Byte offset of the syncframe within the access unit.
    pub offset: usize,
    /// Syncframe length in bytes.
    pub frame_bytes: usize,
    /// The syncframe uses the E-AC-3 syntax.
    pub eac3: bool,
    /// Audio blocks in the syncframe.
    pub num_blocks: usize,
    /// Full-bandwidth channels.
    pub nfchans: usize,
    /// Bit positions (within the syncframe) of every dithflag bit.
    pub dithflag_bits: Vec<usize>,
    /// E-AC-3: bit position of dithflage.
    pub dithflage_bit: Option<usize>,
    /// E-AC-3: where per-block dithflags would be inserted.
    pub dithflag_insert_bits: Vec<usize>,
    /// Bit position just after the last mantissa of the last block.
    pub audio_end_bit: usize,
    /// E-AC-3: bit positions of every ecplinu flag (blocks with a coupling strategy).
    pub ecplinu_bits: Vec<usize>,
    /// E-AC-3: bit position of transproce.
    pub transproce_bit: Option<usize>,
    /// E-AC-3: bit position of the (possibly absent) transient pre-noise data.
    pub transproc_data_bit: Option<usize>,
    /// E-AC-3: bit position of dbaflde.
    pub dbaflde_bit: Option<usize>,
    /// One record per audio block with coupling coordinates or delta bit allocation.
    pub blocks: Vec<BlockTrace>,
    /// Coding tools the syncframe used.
    pub features: FrameFeatures,
}

/// Bit positions within one audio block.
#[derive(Clone, Debug, Default)]
pub struct BlockTrace {
    /// Audio block index.
    pub blk: usize,
    /// A new coupling strategy was read in this block.
    pub cplstre: bool,
    /// Bit position of phsflginu, when present.
    pub phsflginu_bit: Option<usize>,
    /// Bit position where the phase flags start.
    pub phsflg_bit: usize,
    /// Coupling bands.
    pub ncplbnd: usize,
    /// Coupling coordinates were sent for channel 0 or 1.
    pub cplcoe_any: bool,
    /// Bit position of deltbaie.
    pub dba_bit: usize,
}
