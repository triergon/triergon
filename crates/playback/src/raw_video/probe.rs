//! Bounded playback format selection over one source and retained prefix.

use super::RawVideoSource;
use crate::{
    Error, Format, Result,
    input::{Source, SourceReply},
    io::{CONTAINER_SIGNATURE_BYTES, SelectedInput, SourceOwner, detect_container, provide},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error as CoreError, InputCursor, InputRequest, InputWindow,
    SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::{Codec, NalHeader, RawAnnexBLimits, RawAnnexBReader};

const MAXIMUM_PROBE_WINDOW: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub struct RawVideoProbeLimits {
    pub maximum_bytes: usize,
    pub maximum_nals: usize,
}

impl Default for RawVideoProbeLimits {
    fn default() -> Self {
        Self {
            maximum_bytes: 64 * 1024,
            maximum_nals: 64,
        }
    }
}

#[derive(Debug)]
pub enum RawVideoProbeStep {
    NeedInput(InputRequest),
    Yield,
    Detected(Codec),
}

/// Selects one codec from ordered parameter sets and a compatible slice
/// prefix. The actual read prefix is replayed to the selected raw reader.
pub struct RawVideoProbe<S: Source> {
    driver: ProbeDriver<S>,
    reader_limits: RawAnnexBLimits,
}

impl<S: Source> RawVideoProbe<S> {
    pub fn new(
        pool: &Budget,
        id: SourceId,
        caps: SourceCapabilities,
        source: S,
        limits: RawVideoProbeLimits,
        reader_limits: RawAnnexBLimits,
    ) -> Result<Self> {
        Ok(Self {
            driver: ProbeDriver::new(
                pool,
                id,
                caps,
                reader_limits.nal.maximum_input_window,
                source,
                limits,
                ProbeMode::Raw,
            )?,
            reader_limits,
        })
    }

    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<RawVideoProbeStep> {
        Ok(match self.driver.advance(work, cancel)? {
            PlaybackProbeStep::NeedInput(request) => RawVideoProbeStep::NeedInput(request),
            PlaybackProbeStep::Yield => RawVideoProbeStep::Yield,
            PlaybackProbeStep::Detected(PlaybackInputKind::RawVideo(codec)) => {
                RawVideoProbeStep::Detected(codec)
            }
            PlaybackProbeStep::Detected(PlaybackInputKind::Container(_)) => {
                return Err(CoreError::Invalid("raw probe selected a container").into());
            }
        })
    }

    /// Allocation pressure leaves the retained prefix and source retryable.
    pub fn finish(&mut self) -> Result<RawVideoSource<RawVideoReplay<S>>> {
        let Some(PlaybackInputKind::RawVideo(codec)) = self.driver.detected else {
            return Err(CoreError::Invalid("raw video probe not ready").into());
        };
        self.driver.finish_raw(codec, self.reader_limits)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackInputKind {
    Container(Format),
    RawVideo(Codec),
}

#[derive(Debug)]
pub enum PlaybackProbeStep {
    NeedInput(InputRequest),
    Yield,
    Detected(PlaybackInputKind),
}

#[allow(
    clippy::large_enum_variant,
    reason = "one inline selected reader avoids a separate uncharged allocation"
)]
pub enum PlaybackInput<S: Source> {
    Container(SelectedInput<ProbeReplay<S>>),
    RawVideo {
        codec: Codec,
        source: RawVideoSource<ProbeReplay<S>>,
    },
}

/// Common selector for the existing container signatures and the bounded raw
/// Annex B probe. It keeps the actual prefix when the transport cannot rewind.
pub struct PlaybackProbe<S: Source> {
    driver: ProbeDriver<S>,
}

impl<S: Source> PlaybackProbe<S> {
    pub fn new(
        pool: &Budget,
        id: SourceId,
        caps: SourceCapabilities,
        window: usize,
        source: S,
        limits: RawVideoProbeLimits,
    ) -> Result<Self> {
        Ok(Self {
            driver: ProbeDriver::new(pool, id, caps, window, source, limits, ProbeMode::Playback)?,
        })
    }

    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<PlaybackProbeStep> {
        self.driver.advance(work, cancel)
    }

    pub fn finish(&mut self, raw_limits: RawAnnexBLimits) -> Result<PlaybackInput<S>> {
        let selection = self
            .driver
            .detected
            .ok_or(CoreError::Invalid("playback probe not ready"))?;
        match selection {
            PlaybackInputKind::Container(format) => {
                let driver = &mut self.driver;
                let input = InputCursor::new(
                    driver.input.budget(),
                    driver.input.source(),
                    driver.input.capabilities(),
                    driver.window,
                )?;
                let source = SourceOwner::new(driver.take_replay()?);
                Ok(PlaybackInput::Container(SelectedInput {
                    input,
                    source,
                    format,
                }))
            }
            PlaybackInputKind::RawVideo(codec) => Ok(PlaybackInput::RawVideo {
                codec,
                source: self.driver.finish_raw(codec, raw_limits)?,
            }),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeMode {
    Raw,
    Playback,
}

struct ProbeDriver<S: Source> {
    source: Option<SourceOwner<S>>,
    input: InputCursor,
    prefix: BudgetVec<u8>,
    scan: Signatures,
    scanned: usize,
    detected: Option<PlaybackInputKind>,
    limits: RawVideoProbeLimits,
    window: usize,
    container_pending: bool,
    end: Option<u64>,
    failed: bool,
}

impl<S: Source> ProbeDriver<S> {
    fn new(
        pool: &Budget,
        id: SourceId,
        caps: SourceCapabilities,
        window: usize,
        source: S,
        limits: RawVideoProbeLimits,
        mode: ProbeMode,
    ) -> Result<Self> {
        let source = SourceOwner::new(source);
        if limits.maximum_bytes == 0 || limits.maximum_nals == 0 {
            return Err(CoreError::Invalid("zero raw video probe limit").into());
        }
        let container_pending = mode == ProbeMode::Playback;
        if container_pending && limits.maximum_bytes < CONTAINER_SIGNATURE_BYTES {
            return Err(
                CoreError::Invalid("playback probe byte bound below container signature").into(),
            );
        }
        Ok(Self {
            source: Some(source),
            input: InputCursor::new(pool, id, caps, window.min(MAXIMUM_PROBE_WINDOW))?,
            prefix: BudgetVec::new(pool),
            scan: Signatures::default(),
            scanned: 0,
            detected: None,
            limits,
            window,
            container_pending,
            end: None,
            failed: false,
        })
    }

    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<PlaybackProbeStep> {
        if self.failed || self.source.is_none() {
            return Err(Error::Closed);
        }
        let result = self.read(work, cancel);
        if result.is_err()
            && !matches!(
                result,
                Err(Error::Core(
                    CoreError::Backpressure { .. } | CoreError::Allocation
                ))
            )
        {
            self.failed = true;
            self.prefix = BudgetVec::new(self.input.budget());
            self.source.take();
        }
        result
    }

    fn read(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<PlaybackProbeStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CoreError::Cancelled.into());
            }
            if let Some(kind) = self.detected {
                return Ok(PlaybackProbeStep::Detected(kind));
            }
            // Drain the entire accepted window before choosing a codec. A
            // forward-only source already supplied its suffix as well.
            if work.remaining() == 0 {
                return Ok(PlaybackProbeStep::Yield);
            }
            let buffered = self.input.buffered().len();
            if buffered != 0 {
                self.reserve_window(buffered)?;
                match self
                    .input
                    .read_append(&mut self.prefix, buffered, work, cancel)?
                {
                    Step::Ready(_) if self.input.buffered().is_empty() => continue,
                    Step::Ready(_) | Step::Yield => return Ok(PlaybackProbeStep::Yield),
                    Step::NeedInput(_) | Step::End(_) => {
                        return Err(
                            CoreError::Invalid("raw video probe lost buffered input").into()
                        );
                    }
                }
            }
            if self.container_pending {
                if let Some(format) = detect_container(self.prefix.as_slice())? {
                    self.detected = Some(PlaybackInputKind::Container(format));
                    continue;
                }
                self.container_pending =
                    self.prefix.len() < CONTAINER_SIGNATURE_BYTES && self.end.is_none();
            }
            if !self.container_pending
                && let Some(&byte) = self.prefix.as_slice().get(self.scanned)
            {
                if !work.try_charge(1) {
                    return Ok(PlaybackProbeStep::Yield);
                }
                self.scanned += 1;
                self.detected = self
                    .scan
                    .push(byte, self.limits.maximum_nals)?
                    .map(PlaybackInputKind::RawVideo);
                continue;
            }
            if self.end.is_some() {
                return Err(CoreError::Unsupported("raw video signature absent at EOF").into());
            }
            let left = self.limits.maximum_bytes - self.prefix.len();
            if left == 0 {
                return Err(CoreError::Unsupported(
                    "raw video signature absent within probe bound",
                )
                .into());
            }
            let mut request = self.input.request();
            request.maximum = request.maximum.min(left);
            if self.container_pending {
                let next = match self.prefix.len() {
                    0..=3 => 4,
                    4..=7 => 8,
                    _ => CONTAINER_SIGNATURE_BYTES,
                };
                request.maximum = request.maximum.min(next - self.prefix.len());
            }
            let reply = self
                .source
                .as_mut()
                .ok_or(Error::Closed)?
                .poll(&request, work, cancel)?;
            if let SourceReply::Data(window) = &reply
                && window.bytes().len() > request.maximum
            {
                return Err(CoreError::Invalid("raw video probe reply exceeds request").into());
            }
            let ended = matches!(reply, SourceReply::End { .. });
            if !provide(&mut self.input, &reply)? {
                return Ok(PlaybackProbeStep::NeedInput(request));
            }
            if ended {
                if !self.container_pending {
                    return Err(CoreError::Unsupported("raw video signature absent at EOF").into());
                }
                self.end = Some(self.input.position());
            }
        }
    }

    fn reserve_window(&mut self, count: usize) -> triergon_core::Result<()> {
        let needed = self
            .prefix
            .len()
            .checked_add(count)
            .ok_or(CoreError::Overflow)?;
        if needed <= self.prefix.capacity() {
            return Ok(());
        }
        let target = self
            .prefix
            .capacity()
            .saturating_mul(2)
            .max(needed)
            .min(self.limits.maximum_bytes);
        let len = self.prefix.len();
        self.prefix
            .try_reserve_exact(target - len)
            .or_else(|error| {
                if target != needed
                    && matches!(
                        error,
                        CoreError::Backpressure { .. } | CoreError::Limit { .. }
                    )
                {
                    self.prefix.try_reserve_exact(needed - len)
                } else {
                    Err(error)
                }
            })
    }

    fn finish_raw(
        &mut self,
        codec: Codec,
        limits: RawAnnexBLimits,
    ) -> Result<RawVideoSource<ProbeReplay<S>>> {
        if self.source.is_none() {
            return Err(Error::Closed);
        }
        let reader = RawAnnexBReader::new(
            self.input.budget(),
            codec,
            self.input.source(),
            self.input.capabilities(),
            limits,
        )?;
        Ok(RawVideoSource::new(reader, self.take_replay()?))
    }

    fn take_replay(&mut self) -> Result<ProbeReplay<S>> {
        if self.source.is_none() {
            return Err(Error::Closed);
        }
        let prefix = std::mem::replace(&mut self.prefix, BudgetVec::new(self.input.budget()));
        let prefix = match prefix.freeze() {
            Ok(prefix) => prefix,
            Err(rejected) => {
                self.prefix = rejected.buffer;
                return Err(rejected.error.into());
            }
        };
        Ok(ProbeReplay {
            id: self.input.source(),
            prefix: Some(prefix),
            end: self.end,
            source: self.source.take().ok_or(Error::Closed)?,
        })
    }
}

/// A one-pass replay of accepted probe bytes and any confirmed source EOF.
pub struct ProbeReplay<S: Source> {
    id: SourceId,
    prefix: Option<Bytes>,
    end: Option<u64>,
    source: SourceOwner<S>,
}

pub type RawVideoReplay<S> = ProbeReplay<S>;

impl<S: Source> Source for ProbeReplay<S> {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if cancel.load(Ordering::Relaxed) {
            return Err(CoreError::Cancelled);
        }
        if request.source != self.id {
            return Err(CoreError::SourceMismatch);
        }
        if let Some(prefix) = &self.prefix
            && request.offset < prefix.len() as u64
        {
            if !work.try_charge(1) {
                return Ok(SourceReply::Pending);
            }
            let start = request.offset as usize;
            let end = start + request.maximum.min(prefix.len() - start);
            return Ok(SourceReply::Data(InputWindow::new(
                request.source,
                request.token.clone(),
                request.offset,
                prefix.slice(start..end)?,
            )?));
        }
        self.prefix = None;
        if self.end == Some(request.offset) {
            if !work.try_charge(1) {
                return Ok(SourceReply::Pending);
            }
            return Ok(SourceReply::End {
                source: request.source,
                token: request.token.clone(),
            });
        }
        self.source
            .poll(request, work, cancel)
            .map_err(|error| match error {
                Error::Core(error) => error,
                _ => CoreError::Invalid("raw video probe upstream error"),
            })
    }

    fn invalidate(&mut self) {
        self.source.invalidate();
    }

    fn abort(&mut self) {
        self.source.abort();
    }

    fn close(&mut self) {
        self.source.close();
    }
}

#[derive(Default)]
struct Head {
    prefix: [u8; 5],
    len: usize,
}

impl Head {
    fn push(&mut self, byte: u8) -> triergon_core::Result<()> {
        if let Some(next) = self.prefix.get_mut(self.len) {
            *next = byte;
        }
        self.len = self.len.checked_add(1).ok_or(CoreError::Overflow)?;
        Ok(())
    }

    fn header(&self, codec: Codec) -> Option<NalHeader> {
        NalHeader::parse(codec, &self.prefix[..self.len.min(self.prefix.len())]).ok()
    }

    fn hevc_base_header(&self) -> Option<(u8, u8)> {
        let NalHeader::Hevc {
            nal_unit_type,
            layer_id: 0,
            temporal_id_plus_one,
        } = self.header(Codec::Hevc)?
        else {
            return None;
        };
        Some((nal_unit_type, temporal_id_plus_one))
    }
}

#[derive(Default)]
struct Signatures {
    zeros: usize,
    count: usize,
    head: Option<Head>,
    avc_sps: bool,
    avc_pps: bool,
    hevc_vps: bool,
    hevc_sps: bool,
    hevc_pps: bool,
}

impl Signatures {
    fn push(&mut self, byte: u8, maximum_nals: usize) -> triergon_core::Result<Option<Codec>> {
        if byte == 0 {
            self.zeros = self.zeros.checked_add(1).ok_or(CoreError::Overflow)?;
            return Ok(None);
        }
        let zeros = std::mem::take(&mut self.zeros);
        if byte == 1 && zeros >= 2 {
            if let Some(head) = self.head.take() {
                self.complete(&head)?;
            }
            self.count = self.count.checked_add(1).ok_or(CoreError::Overflow)?;
            if self.count > maximum_nals {
                return Err(CoreError::Limit {
                    requested: self.count,
                    maximum: maximum_nals,
                });
            }
            self.head = Some(Head::default());
            return Ok(None);
        }
        let Some(head) = &mut self.head else {
            return Err(CoreError::Unsupported(
                "raw video probe needs a leading Annex B start code",
            ));
        };
        if zeros >= 3 {
            return Err(CoreError::Invalid(
                "nonzero raw probe byte after Annex B padding",
            ));
        }
        for _ in 0..zeros {
            head.push(0)?;
        }
        head.push(byte)?;
        if head.len <= 5 {
            if self.avc_pps
                && head.len >= 2
                && matches!(
                    head.header(Codec::Avc),
                    Some(NalHeader::Avc {
                        nal_unit_type: 1 | 5,
                        ..
                    })
                )
            {
                return Ok(Some(Codec::Avc));
            }
            if self.hevc_pps
                && head.len >= 3
                && matches!(head.hevc_base_header(), Some((0..=9, _) | (16..=21, 1)))
                && head.prefix[2] & 0x80 != 0
            {
                return Ok(Some(Codec::Hevc));
            }
        }
        Ok(None)
    }

    fn complete(&mut self, head: &Head) -> triergon_core::Result<()> {
        if head.len == 0 {
            return Err(CoreError::Invalid("empty raw probe NAL"));
        }
        if let Some(NalHeader::Avc {
            nal_ref_idc: 1..=3,
            nal_unit_type,
        }) = head.header(Codec::Avc)
        {
            match nal_unit_type {
                7 if head.len >= 5 && head.prefix[1] != 0 && head.prefix[2] & 3 == 0 => {
                    self.avc_sps = true;
                }
                8 if self.avc_sps && head.len >= 2 => self.avc_pps = true,
                _ => {}
            }
        }
        // H.265 7.4.2.2 requires temporal id zero on VPS/SPS. PPS
        // can carry a greater temporal id than the following picture.
        match head.hevc_base_header() {
            Some((32, 1)) if head.len >= 3 => self.hevc_vps = true,
            Some((33, 1)) if self.hevc_vps && head.len >= 4 => self.hevc_sps = true,
            Some((34, _)) if self.hevc_sps && head.len >= 3 => self.hevc_pps = true,
            _ => {}
        }
        if self.avc_pps && self.hevc_pps {
            return Err(CoreError::Invalid(
                "ambiguous raw Annex B parameter-set signatures",
            ));
        }
        Ok(())
    }
}
