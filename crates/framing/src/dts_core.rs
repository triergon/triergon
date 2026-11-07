//! Selected canonical normal DTS core bitstream headers and frame envelopes.
//! Original implementation from ETSI TS 102 114 V1.6.1 clauses 3.4/5.3/5.4.2.
//! Encoded audio and internal extensions remain opaque, unchanged source bytes.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputToken, InputWindow, Result,
    SourceCapabilities, SourceFailure, SourceId, SourceLocation, Step, Time, WorkBudget,
};

const CORE_SYNC: u32 = 0x7ffe8001;
const EXTENSION_SYNC: u32 = 0x64582025;
const MAXIMUM_COPY: usize = 4096;

/// Raw source codes plus separately retained checked views of a selected header.
/// `hcrc` distinguishes physical absence from a coded zero. `None` for channel
/// count, nominal bitrate or dialog gain does not manufacture a coded default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreHeader {
    pub normal: u8,
    pub short: u8,
    pub cpf: u8,
    pub nblks: u8,
    pub fsize: u16,
    pub amode: u8,
    pub sfreq: u8,
    pub rate: u8,
    pub fixed: u8,
    pub dynamic: u8,
    pub timestamp: u8,
    pub auxiliary: u8,
    pub hdcd: u8,
    pub extension_id: u8,
    pub extension: u8,
    pub audio_sync: u8,
    pub lfe: u8,
    pub history: u8,
    pub hcrc: Option<u16>,
    pub filter: u8,
    pub version: u8,
    pub copy_history: u8,
    pub pcm_resolution: u8,
    pub sum_front: u8,
    pub sum_surround: u8,
    pub dialog_word: u8,
    pub header_bits: u16,
    pub frame_bytes: usize,
    pub core_rate_hz: u32,
    pub coding_window_samples: u32,
    pub full_bandwidth_channels: Option<u8>,
    pub nominal_bitrate: Option<u32>,
    pub open_bitrate: bool,
    pub dialog_gain_db: Option<i8>,
    /// Exact normal-frame core coding-window duration, not container PTS/DTS,
    /// an inferred extended output rate or a termination presentation duration.
    pub window: Time,
}

fn check_sync(input: &[u8]) -> Result<()> {
    let bytes: [u8; 4] = input
        .get(..4)
        .ok_or(Error::Truncated)?
        .try_into()
        .map_err(|_| Error::Truncated)?;
    match u32::from_be_bytes(bytes) {
        CORE_SYNC => Ok(()),
        EXTENSION_SYNC => Err(Error::Unsupported("DTS extension-substream input")),
        _ => Err(Error::Invalid("canonical DTS core sync")),
    }
}

struct HeaderBits<'a>(BitCursor<'a>);

impl HeaderBits<'_> {
    fn read(&mut self, width: u32) -> Result<u32> {
        self.0.read(width).map_err(|_| Error::Truncated)
    }
    fn byte(&mut self, width: u32) -> Result<u8> {
        u8::try_from(self.read(width)?).map_err(|_| Error::Overflow)
    }
    fn word(&mut self, width: u32) -> Result<u16> {
        u16::try_from(self.read(width)?).map_err(|_| Error::Overflow)
    }
}

/// Allocation-free bounded prefix probe. At most fifteen source bytes and
/// 120 bits are inspected, regardless of the size of the caller's slice.
/// Success does not certify a complete body, coded audio or decoder readiness.
pub fn probe_dts_core_header(input: &[u8]) -> Result<DtsCoreHeader> {
    check_sync(input)?;
    if input.len() < 11 {
        return Err(Error::Truncated);
    }
    let required = if input[4] & 2 == 0 { 13 } else { 15 };
    let source = input.get(..required).ok_or(Error::Truncated)?;
    let mut bits = HeaderBits(BitCursor::at(source, 32));
    let normal = bits.byte(1)?;
    let short = bits.byte(5)?;
    let cpf = bits.byte(1)?;
    let nblks = bits.byte(7)?;
    let fsize = bits.word(14)?;
    let amode = bits.byte(6)?;
    let sfreq = bits.byte(4)?;
    let rate = bits.byte(5)?;
    let fixed = bits.byte(1)?;
    let dynamic = bits.byte(1)?;
    let timestamp = bits.byte(1)?;
    let auxiliary = bits.byte(1)?;
    let hdcd = bits.byte(1)?;
    let extension_id = bits.byte(3)?;
    let extension = bits.byte(1)?;
    let audio_sync = bits.byte(1)?;
    let lfe = bits.byte(2)?;
    let history = bits.byte(1)?;
    let hcrc = if cpf == 0 { None } else { Some(bits.word(16)?) };
    let filter = bits.byte(1)?;
    let version = bits.byte(4)?;
    let copy_history = bits.byte(2)?;
    let pcm_resolution = bits.byte(3)?;
    let sum_front = bits.byte(1)?;
    let sum_surround = bits.byte(1)?;
    let dialog_word = bits.byte(4)?;

    let core_rate_hz: u32 = match sfreq {
        1 => 8000,
        2 => 16000,
        3 => 32000,
        6 => 11025,
        7 => 22050,
        8 => 44100,
        11 => 12000,
        12 => 24000,
        13 => 48000,
        _ => return Err(Error::Invalid("DTS core sample-rate code")),
    };
    if fsize < 95 || nblks < 5 || fixed != 0 {
        return Err(Error::Invalid("DTS frame/block size or fixed bit"));
    }
    if lfe == 3 || !matches!(pcm_resolution, 0 | 1 | 2 | 3 | 5 | 6) {
        return Err(Error::Invalid("DTS LFE or PCM-resolution code"));
    }
    const BITRATES: [u32; 25] = [
        32000, 56000, 64000, 96000, 112000, 128000, 192000, 224000, 256000, 320000, 384000, 448000,
        512000, 576000, 640000, 768000, 960000, 1024000, 1152000, 1280000, 1344000, 1408000,
        1411200, 1472000, 1536000,
    ];
    let nominal_bitrate = BITRATES.get(usize::from(rate)).copied();
    if nominal_bitrate.is_none() && rate != 29 {
        return Err(Error::Invalid("DTS nominal-bitrate code"));
    }
    if normal == 0 {
        return Err(Error::Unsupported("DTS termination frame"));
    }
    if version > 7 {
        return Err(Error::Unsupported("future DTS encoder version"));
    }
    if short != 31 || !matches!(nblks, 7 | 15 | 31 | 63 | 127) {
        return Err(Error::Invalid("normal DTS synchronization/block count"));
    }
    if extension != 0 && !matches!(extension_id, 0 | 2 | 6) {
        return Err(Error::Unsupported("reserved DTS internal-extension code"));
    }
    const CHANNELS: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
    let full_bandwidth_channels = CHANNELS.get(usize::from(amode)).copied();
    let frame_bytes = usize::from(fsize).checked_add(1).ok_or(Error::Overflow)?;
    let coding_window_samples = u32::from(nblks)
        .checked_add(1)
        .and_then(|blocks| blocks.checked_mul(32))
        .ok_or(Error::Overflow)?;
    let dialog = i8::try_from(dialog_word).map_err(|_| Error::Overflow)?;
    let dialog_gain_db = match version {
        6 => Some(-16 - dialog),
        7 => Some(-dialog),
        _ => None,
    };
    let window = Time::new(i128::from(coding_window_samples), u128::from(core_rate_hz))?;
    Ok(DtsCoreHeader {
        normal,
        short,
        cpf,
        nblks,
        fsize,
        amode,
        sfreq,
        rate,
        fixed,
        dynamic,
        timestamp,
        auxiliary,
        hdcd,
        extension_id,
        extension,
        audio_sync,
        lfe,
        history,
        hcrc,
        filter,
        version,
        copy_history,
        pcm_resolution,
        sum_front,
        sum_surround,
        dialog_word,
        header_bits: u16::try_from(bits.0.position()).map_err(|_| Error::Overflow)?,
        frame_bytes,
        core_rate_hz,
        coding_window_samples,
        full_bandwidth_channels,
        nominal_bitrate,
        open_bitrate: rate == 29,
        dialog_gain_db,
        window,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreFrameLimits {
    pub maximum_frame_bytes: usize,
    pub maximum_frames: usize,
    pub maximum_input_window: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreFrameFacts {
    pub source: SourceId,
    pub generation: u64,
    pub offset: u64,
    pub header: DtsCoreHeader,
}

/// Owned complete declared envelope. There is no public unchecked constructor.
#[derive(Clone, Debug)]
pub struct DtsCoreFrame {
    bytes: Bytes,
    facts: DtsCoreFrameFacts,
}

impl DtsCoreFrame {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub const fn facts(&self) -> DtsCoreFrameFacts {
        self.facts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    Sync,
    PrefixKind,
    Probe,
    Reserve,
    CopyPrefix,
    ReadBody,
    CopyBody,
    Freeze,
    Publish,
}

/// Complete declared-frame acquisition, without implicit resynchronization or
/// payload editing. Input visits and all dynamic-buffer copies are separately
/// work-bounded; allocation/probe/freeze/publication each cost one fixed unit.
/// Packing conversions, termination and HD-asset extraction remain unsupported.
#[derive(Debug)]
pub struct DtsCoreFrameReader {
    cursor: Option<InputCursor>,
    last_request: InputRequest,
    limits: DtsCoreFrameLimits,
    phase: Phase,
    prefix: [u8; 15],
    prefix_len: usize,
    prefix_target: usize,
    prefix_copied: usize,
    frame_offset: u64,
    header: Option<DtsCoreHeader>,
    buffer: Option<BudgetVec<u8>>,
    pending: Option<Bytes>,
    pending_copied: usize,
    output: Option<Bytes>,
    completed: usize,
    ended: Option<SourceLocation>,
    failure: Option<Error>,
}

impl DtsCoreFrameReader {
    pub fn new(
        pool: &Budget,
        source: SourceId,
        capabilities: SourceCapabilities,
        limits: DtsCoreFrameLimits,
    ) -> Result<Self> {
        if limits.maximum_frame_bytes == 0
            || limits.maximum_frames == 0
            || limits.maximum_input_window == 0
        {
            return Err(Error::Invalid("zero DTS frame/input policy"));
        }
        let cursor = InputCursor::new(pool, source, capabilities, limits.maximum_input_window)?;
        let last_request = cursor.request();
        Ok(Self {
            cursor: Some(cursor),
            last_request,
            limits,
            phase: Phase::Header,
            prefix: [0; 15],
            prefix_len: 0,
            prefix_target: 4,
            prefix_copied: 0,
            frame_offset: 0,
            header: None,
            buffer: None,
            pending: None,
            pending_copied: 0,
            output: None,
            completed: 0,
            ended: None,
            failure: None,
        })
    }
    pub fn generation(&self) -> u64 {
        self.cursor.as_ref().map_or(
            self.last_request.token.generation(),
            InputCursor::generation,
        )
    }
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.last_request.clone(), InputCursor::request)
    }
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.provide(window)
    }
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let result = self.cursor_mut()?.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.record_failure(Error::Truncated);
        }
        result
    }
    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.fail(source, token, failure)?;
        self.record_failure(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }

    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }
    fn cursor_mut(&mut self) -> Result<&mut InputCursor> {
        self.cursor
            .as_mut()
            .ok_or(Error::Invalid("missing healthy DTS cursor"))
    }
    fn record_failure(&mut self, error: Error) {
        self.failure = Some(error);
        if let Some(cursor) = self.cursor.take() {
            self.last_request = cursor.request();
        }
        self.buffer = None;
        self.pending = None;
        self.output = None;
        self.header = None;
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsCoreFrame>> {
        self.healthy()?;
        let result = self.step_inner(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.record_failure(error);
        }
        result
    }

    fn step_inner(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsCoreFrame>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(end) = self.ended {
                return Ok(Step::End(end));
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            match self.phase {
                Phase::Header => {
                    if self.prefix_len == 0 {
                        self.frame_offset = self.position();
                    }
                    let cursor = self
                        .cursor
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS header cursor"))?;
                    match cursor.read_into(
                        &mut self.prefix[self.prefix_len..self.prefix_target],
                        work,
                        cancel,
                    )? {
                        Step::Ready(count) => {
                            if count == 0 {
                                return Err(Error::Invalid("empty DTS header read"));
                            }
                            if self.completed >= self.limits.maximum_frames {
                                return Err(Error::Limit {
                                    requested: self
                                        .completed
                                        .checked_add(1)
                                        .ok_or(Error::Overflow)?,
                                    maximum: self.limits.maximum_frames,
                                });
                            }
                            self.prefix_len =
                                self.prefix_len.checked_add(count).ok_or(Error::Overflow)?;
                            if self.prefix_len == self.prefix_target {
                                self.phase = match self.prefix_target {
                                    4 => Phase::Sync,
                                    11 => Phase::PrefixKind,
                                    13 | 15 => Phase::Probe,
                                    _ => return Err(Error::Invalid("DTS prefix target")),
                                };
                            }
                        }
                        Step::NeedInput(needed) => return Ok(Step::NeedInput(needed)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(end) => {
                            if self.prefix_len != 0 {
                                return Err(Error::Truncated);
                            }
                            self.ended = Some(end);
                            return Ok(Step::End(end));
                        }
                    }
                }
                Phase::Sync => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    check_sync(&self.prefix[..4])?;
                    self.prefix_target = 11;
                    self.phase = Phase::Header;
                }
                Phase::PrefixKind => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    self.prefix_target = if self.prefix[4] & 2 == 0 { 13 } else { 15 };
                    self.phase = Phase::Header;
                }
                Phase::Probe => {
                    // Fixed <=120-bit inspection; no dynamic prefix/body copy.
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = probe_dts_core_header(&self.prefix[..self.prefix_len])?;
                    if header.frame_bytes > self.limits.maximum_frame_bytes {
                        return Err(Error::Limit {
                            requested: header.frame_bytes,
                            maximum: self.limits.maximum_frame_bytes,
                        });
                    }
                    let pool = self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS pool"))?
                        .budget();
                    let owner = header
                        .frame_bytes
                        .checked_add(Bytes::owner_overhead_bytes())
                        .ok_or(Error::Overflow)?;
                    let maximum = pool
                        .limit()
                        .checked_sub(Budget::control_storage_bytes())
                        .ok_or(Error::Overflow)?;
                    if owner > maximum {
                        return Err(Error::Limit {
                            requested: owner,
                            maximum,
                        });
                    }
                    self.header = Some(header);
                    self.phase = Phase::Reserve;
                }
                Phase::Reserve => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let size = self
                        .header
                        .ok_or(Error::Invalid("missing DTS allocation size"))?
                        .frame_bytes;
                    let pool = self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS reserve pool"))?
                        .budget();
                    let mut buffer = self.buffer.take().unwrap_or_else(|| BudgetVec::new(pool));
                    if !buffer.is_empty() {
                        self.buffer = Some(buffer);
                        return Err(Error::Invalid("DTS reserve would move copied bytes"));
                    }
                    let reserved = buffer.try_reserve_exact(size);
                    self.buffer = Some(buffer);
                    reserved?;
                    self.prefix_copied = 0;
                    self.phase = Phase::CopyPrefix;
                }
                Phase::CopyPrefix => {
                    let remaining = self
                        .prefix_len
                        .checked_sub(self.prefix_copied)
                        .ok_or(Error::Overflow)?;
                    let count = usize::try_from(
                        work.take(u64::try_from(remaining).map_err(|_| Error::Overflow)?),
                    )
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(Step::Yield);
                    }
                    let end = self
                        .prefix_copied
                        .checked_add(count)
                        .ok_or(Error::Overflow)?;
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS prefix buffer"))?;
                    let target = buffer.len().checked_add(count).ok_or(Error::Overflow)?;
                    if target > buffer.capacity() {
                        return Err(Error::Invalid("DTS prefix exceeds reserved storage"));
                    }
                    buffer.try_extend_from_slice(&self.prefix[self.prefix_copied..end])?;
                    self.prefix_copied = end;
                    if end == self.prefix_len {
                        self.phase = Phase::ReadBody;
                    }
                }
                Phase::ReadBody => {
                    let length = self
                        .buffer
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS body buffer"))?
                        .len();
                    let target = self
                        .header
                        .ok_or(Error::Invalid("missing DTS body size"))?
                        .frame_bytes;
                    let remaining = target.checked_sub(length).ok_or(Error::Overflow)?;
                    if remaining == 0 {
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    match self
                        .cursor_mut()?
                        .read_span(remaining.min(MAXIMUM_COPY), work, cancel)?
                    {
                        Step::Ready(bytes) => {
                            if bytes.is_empty() {
                                return Err(Error::Invalid("empty DTS body span"));
                            }
                            self.pending = Some(bytes);
                            self.pending_copied = 0;
                            self.phase = Phase::CopyBody;
                        }
                        Step::NeedInput(needed) => return Ok(Step::NeedInput(needed)),
                        Step::Yield => return Ok(Step::Yield),
                        Step::End(_) => return Err(Error::Truncated),
                    }
                }
                Phase::CopyBody => {
                    let pending = self
                        .pending
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS pending body span"))?;
                    let length = pending.len();
                    let remaining = length
                        .checked_sub(self.pending_copied)
                        .ok_or(Error::Overflow)?;
                    let count = usize::try_from(work.take(
                        u64::try_from(remaining.min(MAXIMUM_COPY)).map_err(|_| Error::Overflow)?,
                    ))
                    .map_err(|_| Error::Overflow)?;
                    if count == 0 {
                        return Ok(Step::Yield);
                    }
                    let end = self
                        .pending_copied
                        .checked_add(count)
                        .ok_or(Error::Overflow)?;
                    let buffer = self
                        .buffer
                        .as_mut()
                        .ok_or(Error::Invalid("missing DTS copy buffer"))?;
                    let target = buffer.len().checked_add(count).ok_or(Error::Overflow)?;
                    if target > buffer.capacity() {
                        return Err(Error::Invalid("DTS body exceeds reserved storage"));
                    }
                    buffer.try_extend_from_slice(&pending[self.pending_copied..end])?;
                    self.pending_copied = end;
                    if end == length {
                        self.pending = None;
                        self.phase = Phase::ReadBody;
                    }
                }
                Phase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let buffer = self
                        .buffer
                        .take()
                        .ok_or(Error::Invalid("missing DTS freeze buffer"))?;
                    match buffer.freeze() {
                        Ok(bytes) => {
                            self.output = Some(bytes);
                            self.phase = Phase::Publish;
                        }
                        Err(failure) => {
                            self.buffer = Some(failure.buffer);
                            return Err(failure.error);
                        }
                    }
                }
                Phase::Publish => {
                    if !work.try_charge(1) {
                        return Ok(Step::Yield);
                    }
                    let header = self
                        .header
                        .ok_or(Error::Invalid("missing DTS published header"))?;
                    let bytes = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing DTS published bytes"))?;
                    if bytes.len() != header.frame_bytes {
                        return Err(Error::Invalid("DTS output/envelope size disagreement"));
                    }
                    let cursor = self
                        .cursor
                        .as_ref()
                        .ok_or(Error::Invalid("missing DTS published source"))?;
                    let facts = DtsCoreFrameFacts {
                        source: cursor.source(),
                        generation: cursor.generation(),
                        offset: self.frame_offset,
                        header,
                    };
                    self.completed = self.completed.checked_add(1).ok_or(Error::Overflow)?;
                    self.header = None;
                    self.prefix_len = 0;
                    self.prefix_target = 4;
                    self.phase = Phase::Header;
                    return Ok(Step::Ready(DtsCoreFrame { bytes, facts }));
                }
            }
        }
    }
}
