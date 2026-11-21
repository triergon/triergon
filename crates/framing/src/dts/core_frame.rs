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

/// The header of a normal DTS core frame (ETSI TS 102 114 V1.6.1 clause 5.3,
/// Table 5-1): every field as coded, in bitstream order, then the values derived
/// from them. The ranges given are those [`probe_core_header`] returns.
/// `hcrc` distinguishes physical absence from a coded zero. `None` for channel
/// count, nominal bitrate or dialog gain means the code defines no value; no
/// default is substituted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreHeader {
    /// `FTYPE`, the frame type: 1, a normal frame.
    pub normal: u8,
    /// `SHORT`, the deficit sample count: 31 in a normal frame.
    pub short: u8,
    /// `CPF`: 1 when the header carries `HCRC`, otherwise 0.
    pub cpf: u8,
    /// `NBLKS`, the number of 32-sample blocks per channel minus one: 7, 15, 31,
    /// 63 or 127.
    pub nblks: u8,
    /// `FSIZE`, the frame size in bytes minus one: 95 to 16383.
    pub fsize: u16,
    /// `AMODE`, the audio channel arrangement code: 0 to 63. Codes 0 to 15 have
    /// a channel count; see `full_bandwidth_channels`.
    pub amode: u8,
    /// `SFREQ`, the core sampling frequency code: 1, 2, 3, 6, 7, 8, 11, 12 or 13.
    pub sfreq: u8,
    /// `RATE`, the transmission bit rate code: 0 to 24 for a nominal rate, or 29
    /// for an open rate.
    pub rate: u8,
    /// `FixedBit`, the reserved bit after `RATE`: 0.
    pub fixed: u8,
    /// `DYNF`, the embedded dynamic range flag: 0 or 1.
    pub dynamic: u8,
    /// `TIMEF`, the embedded time stamp flag: 0 or 1.
    pub timestamp: u8,
    /// `AUXF`, the auxiliary data flag: 0 or 1.
    pub auxiliary: u8,
    /// `HDCD`, the HDCD source flag: 0 or 1.
    pub hdcd: u8,
    /// `EXT_AUDIO_ID`, the extension audio descriptor: 0 to 7. With `extension`
    /// set it is 0, 2 or 6.
    pub extension_id: u8,
    /// `EXT_AUDIO`: 1 when the frame carries the extension `extension_id` names,
    /// otherwise 0.
    pub extension: u8,
    /// `ASPF`, the audio sync word insertion flag: 0 or 1.
    pub audio_sync: u8,
    /// `LFF`, the low frequency effects flag: 0 without an LFE channel, 1 or 2
    /// with one.
    pub lfe: u8,
    /// `HFLAG`, the predictor history flag: 0 or 1.
    pub history: u8,
    /// `HCRC`, the header CRC word as coded; `None` when `cpf` is 0. It is not
    /// verified.
    pub hcrc: Option<u16>,
    /// `FILTS`, the multirate interpolator switch: 0 or 1.
    pub filter: u8,
    /// `VERNUM`, the encoder software revision: 0 to 7.
    pub version: u8,
    /// `CHIST`, the copy history: 0 to 3.
    pub copy_history: u8,
    /// `PCMR`, the source PCM resolution code: 0, 1, 2, 3, 5 or 6.
    pub pcm_resolution: u8,
    /// `SUMF`, the front sum/difference flag: 0 or 1.
    pub sum_front: u8,
    /// `SUMS`, the surround sum/difference flag: 0 or 1.
    pub sum_surround: u8,
    /// `DIALNORM`, the last four header bits as coded: 0 to 15. They are a dialog
    /// normalization only for `version` 6 and 7; see `dialog_gain_db`.
    pub dialog_word: u8,
    /// Length of the header in bits, sync word included: 104, or 120 with `HCRC`.
    pub header_bits: u16,
    /// Size of the whole frame in bytes, sync word included: `FSIZE + 1`, 96 to
    /// 16384.
    pub frame_bytes: usize,
    /// Core sampling frequency in Hz, from `SFREQ`: 8000, 11025, 12000, 16000,
    /// 22050, 24000, 32000, 44100 or 48000.
    pub core_rate_hz: u32,
    /// Samples per channel the frame codes at `core_rate_hz`:
    /// `32 * (NBLKS + 1)`, which is 256, 512, 1024, 2048 or 4096.
    pub coding_window_samples: u32,
    /// Number of full-bandwidth channels, 1 to 8, for `AMODE` 0 to 15; `None`
    /// for the arrangement codes above 15. The LFE channel is not counted.
    pub full_bandwidth_channels: Option<u8>,
    /// Bit rate in bits per second for `RATE` 0 to 24, 32000 to 1536000; `None`
    /// for an open rate.
    pub nominal_bitrate: Option<u32>,
    /// Whether `RATE` is 29, the open bit rate.
    pub open_bitrate: bool,
    /// Dialog normalization gain in dB: `-(16 + DIALNORM)` for `VERNUM` 6,
    /// `-DIALNORM` for `VERNUM` 7, `None` for every other version.
    pub dialog_gain_db: Option<i8>,
    /// Duration of the frame's core coding window, exactly
    /// `coding_window_samples / core_rate_hz` seconds. It is not a container
    /// PTS/DTS, does not use the higher output rate of an extension, and is not
    /// the presentation duration of a termination frame.
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

/// Reads the header of a normal core frame from the start of `input`, which
/// begins at the sync word `0x7FFE8001` in canonical big-endian byte order.
/// The probe allocates nothing and inspects at most fifteen bytes (120 bits),
/// regardless of the size of the caller's slice. Success does not certify a
/// complete body, coded audio or decoder readiness.
///
/// # Errors
///
/// `Truncated` when `input` ends before the header does: 13 bytes, or 15 with
/// `HCRC`. `Invalid` when `input` begins with neither the core nor the extension
/// substream sync word, and for a field the specification rules out: an `SFREQ`
/// or `RATE` code without a value, `FSIZE` below 95, `NBLKS` below 5, a set
/// reserved bit, `LFF` 3, `PCMR` 4 or 7, a `SHORT` other than 31 or an `NBLKS`
/// other than 7, 15, 31, 63 and 127. `Unsupported` for the extension substream
/// sync word `0x64582025`, a termination frame, `VERNUM` above 7, and
/// `EXT_AUDIO` set with an `EXT_AUDIO_ID` other than 0, 2 and 6.
pub fn probe_core_header(input: &[u8]) -> Result<DtsCoreHeader> {
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

/// The bounds a [`DtsCoreFrameReader`] enforces. None may be zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreFrameLimits {
    /// Largest frame accepted, in bytes (`FSIZE + 1`).
    pub maximum_frame_bytes: usize,
    /// Most frames the reader returns. Input that continues after that many
    /// is refused.
    pub maximum_frames: usize,
    /// Most bytes one input window may carry.
    pub maximum_input_window: usize,
}

/// Where a [`DtsCoreFrame`] lies in its source, and what its header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreFrameFacts {
    /// The source the frame was read from.
    pub source: SourceId,
    /// The generation of the reader's input when the frame was returned; see
    /// [`DtsCoreFrameReader::generation`].
    pub generation: u64,
    /// Offset of the frame's sync word in the source, in bytes.
    pub offset: u64,
    /// The header read from the frame's first bytes.
    pub header: DtsCoreHeader,
}

/// One complete core frame: an owned copy of the `FSIZE + 1` bytes its header
/// declares, exactly as the source has them, with its header. Only a
/// [`DtsCoreFrameReader`] creates one.
#[derive(Clone, Debug)]
pub struct DtsCoreFrame {
    bytes: Bytes,
    facts: DtsCoreFrameFacts,
}

impl DtsCoreFrame {
    /// The frame's bytes from the sync word on: `facts().header.frame_bytes` of
    /// them.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// The frame's place in its source and its header.
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

/// Reads the core frames of an aligned stream: canonical big-endian normal
/// frames back to back, the first at offset zero. Each frame is returned
/// complete: the `FSIZE + 1` bytes its header declares, copied byte for byte.
/// The reader never searches for a sync word: bytes that are not a frame header
/// where one is due are an error. It does not convert word packings, and refuses
/// termination frames and extension substreams.
///
/// Reading input and copying into the frame are each charged one unit of work per
/// byte; the sync check, reading `CPF`, the header probe, the allocation,
/// completing the frame and returning it cost one unit each. An error from
/// [`step`](Self::step) ends the reader, and every later call returns it again,
/// except `Backpressure` and `Allocation`, after which `step` may be called
/// again.
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
    /// A reader at offset zero of `source`. Input windows must be charged to
    /// `pool`, and the frames are charged to it.
    ///
    /// # Errors
    ///
    /// `Invalid` when a limit is zero.
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
    /// [`InputToken::generation`] of the token the reader's requests carry.
    /// After an error, that of the last request.
    pub fn generation(&self) -> u64 {
        self.cursor.as_ref().map_or(
            self.last_request.token.generation(),
            InputCursor::generation,
        )
    }
    /// Offset in the source of the next byte the reader consumes. After an
    /// error, the offset of the last request.
    pub fn position(&self) -> u64 {
        self.cursor
            .as_ref()
            .map_or(self.last_request.offset, InputCursor::position)
    }
    /// The request for the bytes at [`position`](Self::position), to be answered
    /// with [`provide`](Self::provide), [`finish`](Self::finish) or
    /// [`fail`](Self::fail). After an error, the last request made.
    pub fn request(&self) -> InputRequest {
        self.cursor
            .as_ref()
            .map_or_else(|| self.last_request.clone(), InputCursor::request)
    }
    /// Answers the current request with `window`, whose bytes the reader shares.
    /// An empty window means "none yet", never the end of the source.
    ///
    /// # Errors
    ///
    /// The error that ended the reader, if any. Otherwise a window is refused,
    /// leaving the reader as it was, when it carries another token or source
    /// than the request, is charged to another pool, exceeds
    /// `maximum_input_window`, does not start at `position`, or arrives while
    /// earlier bytes are unread or after the end of the source.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.cursor_mut()?.provide(window)
    }
    /// Answers the current request with the end of the source, which lies after
    /// the bytes already provided. [`step`](Self::step) then returns `End` once
    /// it has read them, or `Truncated` when they end inside a frame.
    ///
    /// # Errors
    ///
    /// The error that ended the reader, if any; a token or source other than
    /// the request's; and `Truncated`, which ends the reader, when the source
    /// ends before the length its capabilities declared.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let result = self.cursor_mut()?.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.record_failure(Error::Truncated);
        }
        result
    }
    /// Answers the current request with a failure of the source. This ends the
    /// reader: every later call returns `Transport` or `Cancelled`, as `failure`
    /// says. Frames returned earlier stay valid.
    ///
    /// # Errors
    ///
    /// The error that ended the reader, if any, and a token or source other
    /// than the request's.
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

    /// Advances the reader by at most the work left in `work`. `Ready` carries
    /// the next complete frame. `NeedInput` carries the request to answer before
    /// the next call, `Yield` means the work is spent, and `End` that the source
    /// ended on a frame boundary, at the location given.
    ///
    /// # Errors
    ///
    /// `Cancelled` when `cancel` is set. `Truncated` when the source ends inside
    /// a frame. A header [`probe_core_header`] refuses, with its error. `Limit`
    /// when a frame exceeds `maximum_frame_bytes` or what the pool can hold, and
    /// when input continues after `maximum_frames` frames. `Backpressure` or
    /// `Allocation` when the frame's storage cannot be allocated: after these
    /// two, and only these, `step` may be called again.
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
                    let header = probe_core_header(&self.prefix[..self.prefix_len])?;
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
                            self.buffer = Some(failure.value);
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
