//! Independent AC-3 / E-AC-3 prefix facts from ATSC A/52:2018. Complete frame
//! envelopes are not CRC, BSI-tail, audio-block or decoder conformance proofs.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, Time, WorkBudget,
};

// ATSC Table 5.18 / ETSI Table 4.13. Numeric standard data, not implementation
// material. The independent oracle uses sample-count/bit-rate arithmetic.
pub(crate) const BIT_RATES_KBPS: [u16; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];
const WORDS_44100_EVEN: [u16; 19] = [
    69, 87, 104, 121, 139, 174, 208, 243, 278, 348, 417, 487, 557, 696, 835, 975, 1114, 1253, 1393,
];
const MAX_PREFIX: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ac3Family {
    Ac3,
    Eac3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ac3StreamSelection {
    Any,
    Ac3Only,
    Eac3Only,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eac3DualMono {
    pub dialnorm: u8,
    pub compression: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ac3FrameDetails {
    Ac3 {
        fscod: u8,
        frmsizecod: u8,
        bsmod: u8,
        crc1: u16,
        center_mix: Option<u8>,
        surround_mix: Option<u8>,
        surround_mode: Option<u8>,
        dialnorm: u8,
        compression: Option<u8>,
    },
    Eac3 {
        stream_type: u8,
        substream_id: u8,
        frame_size_code: u16,
        fscod: u8,
        /// fscod2 when fscod == 3; otherwise the physically present numblkscod.
        rate_or_blocks_code: u8,
        dialnorm: u8,
        compression: Option<u8>,
        dual_mono: Option<Eac3DualMono>,
        /// None outside dependent streams; Some(false) retains explicit absence.
        channel_map_present: Option<bool>,
        channel_map: Option<u16>,
    },
}

/// Bits: 1 reserved mix/surround value; 2 zero dialogue-normalization code;
/// 4 reserved E-AC-3 stream type. No repair or decoder policy is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3HeaderIssues(u8);

impl Ac3HeaderIssues {
    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3FrameHeader {
    pub family: Ac3Family,
    pub frame_bytes: usize,
    pub sample_rate_hz: u32,
    pub blocks: u8,
    pub bsid: u8,
    pub acmod: u8,
    pub lfe_on: bool,
    /// Exact bit boundary of the selected probe prefix, not the full BSI end.
    pub probe_bits: u8,
    pub details: Ac3FrameDetails,
}

impl Ac3FrameHeader {
    pub const fn eac3_fscod2(self) -> Option<u8> {
        match self.details {
            Ac3FrameDetails::Eac3 {
                fscod: 3,
                rate_or_blocks_code,
                ..
            } => Some(rate_or_blocks_code),
            _ => None,
        }
    }
    pub const fn eac3_numblkscod(self) -> Option<u8> {
        match self.details {
            Ac3FrameDetails::Eac3 {
                fscod,
                rate_or_blocks_code,
                ..
            } if fscod != 3 => Some(rate_or_blocks_code),
            _ => None,
        }
    }
    pub fn coded_samples(self) -> u16 {
        u16::from(self.blocks) * 256
    }
    /// Coded sample span only. A dependent frame is not additional programme
    /// time and must not be added to its independent stream's presentation span.
    pub fn sample_span(self) -> Result<Time> {
        Time::new(
            i128::from(self.coded_samples()),
            u128::from(self.sample_rate_hz),
        )
    }
    /// Exact unreduced (bits * samples/second, samples) rate ratio. At 44.1 kHz
    /// this need not equal the AC-3 nominal bit-rate code's integral rate.
    pub fn bit_rate_ratio(self) -> Result<(u64, u16)> {
        let numerator = u64::try_from(self.frame_bytes)
            .map_err(|_| Error::Overflow)?
            .checked_mul(8)
            .and_then(|value| value.checked_mul(u64::from(self.sample_rate_hz)))
            .ok_or(Error::Overflow)?;
        let denominator = self.coded_samples();
        if denominator == 0 {
            return Err(Error::Invalid("zero coded audio blocks"));
        }
        Ok((numerator, denominator))
    }
    pub fn nominal_bit_rate_bps(self) -> Option<u32> {
        match self.details {
            Ac3FrameDetails::Ac3 { frmsizecod, .. } => BIT_RATES_KBPS
                .get(usize::from(frmsizecod / 2))
                .map(|rate| u32::from(*rate) * 1000),
            Ac3FrameDetails::Eac3 { .. } => None,
        }
    }
    /// acmod's full-bandwidth count, not aggregate programme/channel-map layout.
    pub fn acmod_full_bandwidth_channels(self) -> Option<u8> {
        [2, 1, 2, 3, 3, 4, 4, 5]
            .get(usize::from(self.acmod))
            .copied()
    }
    pub fn syntax_issues(self) -> Ac3HeaderIssues {
        match self.details {
            Ac3FrameDetails::Ac3 {
                center_mix,
                surround_mix,
                surround_mode,
                dialnorm,
                ..
            } => Ac3HeaderIssues(
                u8::from([center_mix, surround_mix, surround_mode].contains(&Some(3)))
                    | (u8::from(dialnorm == 0) << 1),
            ),
            Ac3FrameDetails::Eac3 {
                stream_type,
                dialnorm,
                dual_mono,
                ..
            } => Ac3HeaderIssues(
                (u8::from(dialnorm == 0 || dual_mono.is_some_and(|value| value.dialnorm == 0))
                    << 1)
                    | (u8::from(stream_type == 3) << 2),
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct Layout {
    family: Ac3Family,
    frame_bytes: usize,
    rate: u32,
    blocks: u8,
    bsid: u8,
}

fn layout(prefix: &[u8]) -> Result<Layout> {
    if prefix.len() < 2 {
        return Err(Error::Truncated);
    }
    if prefix[..2] == [0x77, 0x0b] {
        return Err(Error::Unsupported("word-swapped AC-3 byte order"));
    }
    if prefix[..2] != [0x0b, 0x77] {
        return Err(Error::Invalid("AC-3 syncword at frame boundary"));
    }
    if prefix.len() < 6 {
        return Err(Error::Truncated);
    }
    let bsid = prefix[5] >> 3;
    let fscod = prefix[4] >> 6;
    if bsid <= 8 {
        if fscod == 3 {
            return Err(Error::Invalid("reserved AC-3 sample rate code"));
        }
        let code = prefix[4] & 63;
        let index = usize::from(code / 2);
        let rate = *BIT_RATES_KBPS
            .get(index)
            .ok_or(Error::Invalid("reserved AC-3 frame size code"))?;
        let words = match fscod {
            0 => rate.checked_mul(2).ok_or(Error::Overflow)?,
            1 => WORDS_44100_EVEN[index]
                .checked_add(u16::from(code & 1))
                .ok_or(Error::Overflow)?,
            2 => rate.checked_mul(3).ok_or(Error::Overflow)?,
            _ => return Err(Error::Invalid("AC-3 sample rate code")),
        };
        Ok(Layout {
            family: Ac3Family::Ac3,
            frame_bytes: usize::from(words).checked_mul(2).ok_or(Error::Overflow)?,
            rate: [48000, 44100, 32000][usize::from(fscod)],
            blocks: 6,
            bsid,
        })
    } else if (11..=16).contains(&bsid) {
        let size = (u16::from(prefix[2] & 7) << 8) | u16::from(prefix[3]);
        let frame_bytes = usize::from(size)
            .checked_add(1)
            .and_then(|value| value.checked_mul(2))
            .ok_or(Error::Overflow)?;
        if frame_bytes < 6 {
            return Err(Error::Invalid("E-AC-3 frame too small for fixed prefix"));
        }
        let second = (prefix[4] >> 4) & 3;
        let (rate, blocks) = if fscod == 3 {
            if second == 3 {
                return Err(Error::Invalid("reserved E-AC-3 reduced sample rate"));
            }
            ([24000, 22050, 16000][usize::from(second)], 6)
        } else {
            (
                [48000, 44100, 32000][usize::from(fscod)],
                [1, 2, 3, 6][usize::from(second)],
            )
        };
        Ok(Layout {
            family: Ac3Family::Eac3,
            frame_bytes,
            rate,
            blocks,
            bsid,
        })
    } else {
        Err(Error::Unsupported("AC-3/E-AC-3 bit stream identification"))
    }
}

struct PrefixBits<'a> {
    bytes: &'a [u8],
    position: usize,
    limit: usize,
}

impl PrefixBits<'_> {
    fn take(&mut self, count: usize) -> Result<u16> {
        if count > 16 {
            return Err(Error::Invalid("audio probe bit width"));
        }
        let end = self.position.checked_add(count).ok_or(Error::Overflow)?;
        if end > self.limit {
            return Err(Error::Invalid("audio BSI probe overlaps final CRC word"));
        }
        if end > self.bytes.len().checked_mul(8).ok_or(Error::Overflow)? {
            return Err(Error::Truncated);
        }
        if count == 0 {
            return Ok(0);
        }
        let word = match &self.bytes[self.position / 8..end.div_ceil(8)] {
            [a] => u32::from(*a),
            [a, b] => u32::from(u16::from_be_bytes([*a, *b])),
            [a, b, c] => u32::from_be_bytes([0, *a, *b, *c]),
            _ => return Err(Error::Invalid("audio probe bit width")),
        };
        let shift = (8 - end % 8) % 8;
        let value = (word >> shift) & ((1 << count) - 1);
        self.position = end;
        Ok(value as u16)
    }
    fn small(&mut self, count: usize) -> Result<u8> {
        u8::try_from(self.take(count)?).map_err(|_| Error::Overflow)
    }
    fn compression(&mut self) -> Result<Option<u8>> {
        if self.take(1)? != 0 {
            Ok(Some(self.small(8)?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
#[test]
fn prefix_bits_match_a_scalar_read_at_each_alignment() {
    let bytes = [0x86, 0x1d, 0xc7, 0x30];
    for position in 0..16 {
        for count in 0..=16 {
            let mut probe = PrefixBits {
                bytes: &bytes,
                position,
                limit: 32,
            };
            let expected = (position..position + count).fold(0_u16, |value, offset| {
                (value << 1) | u16::from((bytes[offset / 8] >> (7 - offset % 8)) & 1)
            });
            assert_eq!(probe.take(count).unwrap(), expected);
            assert_eq!(probe.position, position + count);
        }
    }
}

/// Reads at most twelve bytes / ninety selected bits. Extra supplied bytes are
/// not scanned. Truncated means the bounded prefix needs more bytes; unsupported
/// BSIDs and byte order are not guessed. CRCs/audio payload remain unchecked.
pub fn probe_ac3_header(prefix: &[u8]) -> Result<Ac3FrameHeader> {
    let prefix = &prefix[..prefix.len().min(MAX_PREFIX)];
    probe_with_layout(prefix, layout(prefix)?)
}

fn probe_with_layout(prefix: &[u8], info: Layout) -> Result<Ac3FrameHeader> {
    let limit = info
        .frame_bytes
        .checked_mul(8)
        .and_then(|value| value.checked_sub(16))
        .ok_or(Error::Overflow)?;
    let mut bits = PrefixBits {
        bytes: prefix,
        position: if info.family == Ac3Family::Ac3 {
            48
        } else {
            45
        },
        limit,
    };
    let (acmod, lfe_on, details) = match info.family {
        Ac3Family::Ac3 => {
            let acmod = bits.small(3)?;
            let center_mix = if acmod & 1 != 0 && acmod != 1 {
                Some(bits.small(2)?)
            } else {
                None
            };
            let surround_mix = if acmod & 4 != 0 {
                Some(bits.small(2)?)
            } else {
                None
            };
            let surround_mode = if acmod == 2 {
                Some(bits.small(2)?)
            } else {
                None
            };
            let lfe_on = bits.take(1)? != 0;
            let dialnorm = bits.small(5)?;
            let compression = bits.compression()?;
            (
                acmod,
                lfe_on,
                Ac3FrameDetails::Ac3 {
                    fscod: prefix[4] >> 6,
                    frmsizecod: prefix[4] & 63,
                    bsmod: prefix[5] & 7,
                    crc1: u16::from_be_bytes([prefix[2], prefix[3]]),
                    center_mix,
                    surround_mix,
                    surround_mode,
                    dialnorm,
                    compression,
                },
            )
        }
        Ac3Family::Eac3 => {
            let stream_type = prefix[2] >> 6;
            let acmod = (prefix[4] >> 1) & 7;
            let lfe_on = prefix[4] & 1 != 0;
            let dialnorm = bits.small(5)?;
            let compression = bits.compression()?;
            let dual_mono = if acmod == 0 {
                Some(Eac3DualMono {
                    dialnorm: bits.small(5)?,
                    compression: bits.compression()?,
                })
            } else {
                None
            };
            let channel_map_present = if stream_type == 1 {
                Some(bits.take(1)? != 0)
            } else {
                None
            };
            let channel_map = if channel_map_present == Some(true) {
                Some(bits.take(16)?)
            } else {
                None
            };
            (
                acmod,
                lfe_on,
                Ac3FrameDetails::Eac3 {
                    stream_type,
                    substream_id: (prefix[2] >> 3) & 7,
                    frame_size_code: (u16::from(prefix[2] & 7) << 8) | u16::from(prefix[3]),
                    fscod: prefix[4] >> 6,
                    rate_or_blocks_code: (prefix[4] >> 4) & 3,
                    dialnorm,
                    compression,
                    dual_mono,
                    channel_map_present,
                    channel_map,
                },
            )
        }
    };
    Ok(Ac3FrameHeader {
        family: info.family,
        frame_bytes: info.frame_bytes,
        sample_rate_hz: info.rate,
        blocks: info.blocks,
        bsid: info.bsid,
        acmod,
        lfe_on,
        probe_bits: u8::try_from(bits.position).map_err(|_| Error::Overflow)?,
        details,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3FrameLimits {
    pub maximum_frame_bytes: usize,
    pub maximum_frames: usize,
    pub maximum_input_window: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ac3FrameFacts {
    pub source: SourceId,
    pub generation: u64,
    pub offset: u64,
    pub header: Ac3FrameHeader,
}

#[derive(Clone, Debug)]
pub struct Ac3Frame {
    bytes: Bytes,
    facts: Ac3FrameFacts,
}

impl Ac3Frame {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
    pub const fn facts(&self) -> Ac3FrameFacts {
        self.facts
    }
    /// Raw last 16 bits, not a CRC-validation result.
    pub fn crc2_word(&self) -> u16 {
        let length = self.bytes.len();
        u16::from_be_bytes([self.bytes[length - 2], self.bytes[length - 1]])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Start,
    Common,
    Layout,
    ProbeBytes,
    Probe,
    Reserve,
    Prefix,
    Body,
    Freeze,
    End,
}

/// Strict aligned big-endian elementary syncframes. No search/recovery or byte
/// swap occurs. A complete frame is emitted only after its declared envelope is
/// present; later input failure cannot invalidate earlier owned outputs.
pub struct Ac3FrameReader {
    cursor: InputCursor,
    selection: Ac3StreamSelection,
    limits: Ac3FrameLimits,
    phase: Phase,
    prefix: [u8; MAX_PREFIX],
    used: usize,
    target: usize,
    copied: usize,
    offset: u64,
    frame_bytes: usize,
    layout: Option<Layout>,
    header: Option<Ac3FrameHeader>,
    buffer: Option<BudgetVec<u8>>,
    completed: usize,
    failure: Option<Error>,
}

impl Ac3FrameReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        selection: Ac3StreamSelection,
        limits: Ac3FrameLimits,
    ) -> Result<Self> {
        if limits.maximum_frame_bytes == 0
            || limits.maximum_frames == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero AC-3 frame policy"));
        }
        Ok(Self {
            cursor: InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?,
            selection,
            limits,
            phase: Phase::Start,
            prefix: [0; MAX_PREFIX],
            used: 0,
            target: 6,
            copied: 0,
            offset: 0,
            frame_bytes: 0,
            layout: None,
            header: None,
            buffer: None,
            completed: 0,
            failure: None,
        })
    }
    pub fn request(&self) -> InputRequest {
        self.cursor.request()
    }
    pub fn token(&self) -> InputToken {
        self.cursor.token()
    }
    pub fn generation(&self) -> u64 {
        self.cursor.generation()
    }
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide(window)
    }
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.provide_owned(window)
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.cursor.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.stop(Error::Truncated);
        }
        result
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.cursor.fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }
    fn stop(&mut self, error: Error) {
        if error == Error::Cancelled {
            let _ = self.cursor.fail(
                self.cursor.source(),
                self.cursor.token(),
                SourceFailure::Cancelled,
            );
        }
        self.failure = Some(error);
        self.buffer = None;
    }
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<Ac3Frame>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            self.stop(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        let result = self.step_inner(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(error);
        }
        result
    }
    fn buffer(&mut self) -> Result<&mut BudgetVec<u8>> {
        self.buffer
            .as_mut()
            .ok_or(Error::Invalid("missing audio frame builder"))
    }

    fn step_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<Ac3Frame>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Start => {
                    self.offset = self.cursor.position();
                    self.used = 0;
                    self.copied = 0;
                    self.target = 6;
                    self.header = None;
                    self.phase = Phase::Common;
                }
                Phase::Common | Phase::ProbeBytes => {
                    if self.used == self.target {
                        self.phase = if self.phase == Phase::Common {
                            Phase::Layout
                        } else {
                            Phase::Probe
                        };
                        continue;
                    }
                    match self.cursor.read_into(
                        &mut self.prefix[self.used..self.target],
                        work,
                        cancel,
                    )? {
                        Step::Ready(count) => {
                            self.used = self.used.checked_add(count).ok_or(Error::Overflow)?;
                            if self.used >= 2 {
                                if self.prefix[..2] == [0x77, 0x0b] {
                                    return Err(Error::Unsupported("word-swapped AC-3 byte order"));
                                }
                                if self.prefix[..2] != [0x0b, 0x77] {
                                    return Err(Error::Invalid("AC-3 syncword at frame boundary"));
                                }
                            }
                        }
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(location) => {
                            if self.phase == Phase::Common && self.used == 0 {
                                self.phase = Phase::End;
                                return Ok(Step::End(location));
                            }
                            return Err(Error::Truncated);
                        }
                    }
                }
                Phase::Layout => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.completed >= self.limits.maximum_frames {
                        return Err(Error::Limit {
                            requested: self.completed.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.maximum_frames,
                        });
                    }
                    let info = layout(&self.prefix[..6])?;
                    if matches!(
                        (self.selection, info.family),
                        (Ac3StreamSelection::Ac3Only, Ac3Family::Eac3)
                            | (Ac3StreamSelection::Eac3Only, Ac3Family::Ac3)
                    ) {
                        return Err(Error::Unsupported(
                            "audio frame family differs from declared stream selection",
                        ));
                    }
                    if info.frame_bytes > self.limits.maximum_frame_bytes {
                        return Err(Error::Limit {
                            requested: info.frame_bytes,
                            maximum: self.limits.maximum_frame_bytes,
                        });
                    }
                    let end = self
                        .offset
                        .checked_add(u64::try_from(info.frame_bytes).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    if self
                        .cursor
                        .capabilities()
                        .length
                        .is_some_and(|length| end > length)
                    {
                        return Err(Error::Truncated);
                    }
                    self.frame_bytes = info.frame_bytes;
                    self.layout = Some(info);
                    self.target = info.frame_bytes.min(MAX_PREFIX);
                    self.phase = Phase::ProbeBytes;
                }
                Phase::Probe => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let info = self
                        .layout
                        .take()
                        .ok_or(Error::Invalid("missing audio frame layout"))?;
                    self.header = Some(probe_with_layout(&self.prefix[..self.used], info)?);
                    self.phase = Phase::Reserve;
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    if self.buffer.is_none() {
                        self.buffer = Some(BudgetVec::new(self.cursor.budget()));
                    }
                    let length = self.frame_bytes;
                    self.buffer()?.try_reserve_exact(length)?;
                    self.phase = Phase::Prefix;
                }
                Phase::Prefix => {
                    let remaining = self.used.checked_sub(self.copied).ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Body;
                        continue;
                    }
                    let count = usize::try_from(
                        work.take(u64::try_from(remaining).map_err(|_| Error::Overflow)?),
                    )
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(Step::Yield);
                    }
                    let start = self.copied;
                    let end = start.checked_add(count).ok_or(Error::Overflow)?;
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing audio frame builder"))?;
                    buffer.try_extend_from_slice(&self.prefix[start..end])?;
                    self.copied = end;
                }
                Phase::Body => {
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing audio frame builder"))?;
                    let remaining = self
                        .frame_bytes
                        .checked_sub(buffer.len())
                        .ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    match self
                        .cursor
                        .read_append(buffer, remaining.min(4096), work, cancel)?
                    {
                        Step::Ready(_) => {}
                        Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = self
                        .header
                        .ok_or(Error::Invalid("missing audio frame header"))?;
                    let buffer = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("missing audio frame builder"))?;
                    let bytes = match buffer.freeze() {
                        Ok(bytes) => bytes,
                        Err(rejected) => {
                            self.buffer = Some(rejected.buffer);
                            return Err(rejected.error);
                        }
                    };
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.phase = Phase::Start;
                    return Ok(Step::Ready(Ac3Frame {
                        bytes,
                        facts: Ac3FrameFacts {
                            source: self.cursor.source(),
                            generation: self.cursor.generation(),
                            offset: self.offset,
                            header,
                        },
                    }));
                }
                Phase::End => {
                    return Ok(Step::End(SourceLocation {
                        source: self.cursor.source(),
                        offset: self.cursor.position(),
                    }));
                }
            }
        }
    }
}
