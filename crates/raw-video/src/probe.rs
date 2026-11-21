//! Bounded playback format selection over one source and retained prefix.

use super::RawVideoSource;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, Result,
    SourceCapabilities, SourceId, Step, WorkBudget, nonzero_prefix_len,
};
use triergon_framing::nal::{Codec, NalHeader, RawAnnexBLimits, RawAnnexBReader};
use triergon_playback::{
    Format, Source, SourceOwner, SourceReply,
    probe::{CONTAINER_SIGNATURE_BYTES, SelectedInput, detect_container},
};

const MAXIMUM_PROBE_WINDOW: usize = 4096;

/// Bounds of a raw video probe.
#[derive(Clone, Copy, Debug)]
pub struct RawVideoProbeLimits {
    /// The most prefix bytes read and retained (64 KiB by default); nonzero, and for a
    /// [`PlaybackProbe`] at least `CONTAINER_SIGNATURE_BYTES`.
    pub maximum_bytes: usize,
    /// The most NAL units scanned before the probe fails with `Limit` (64 by default);
    /// nonzero.
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

/// One step of a [`RawVideoProbe`].
#[derive(Debug)]
pub enum RawVideoProbeStep {
    /// The source is pending on this request: advance again once it can reply.
    NeedInput(InputRequest),
    /// The work budget ran out: advance again with more work.
    Yield,
    /// The prefix identifies this codec: call `finish`.
    Detected(Codec),
}

/// Selects one codec from ordered parameter sets and a compatible slice
/// prefix. The actual read prefix is replayed to the selected raw reader.
#[derive(Debug)]
pub struct RawVideoProbe<S: Source> {
    driver: ProbeDriver<S>,
    reader_limits: RawAnnexBLimits,
}

impl<S: Source> RawVideoProbe<S> {
    /// A probe over `source`; the selected raw reader reads with `reader_limits`. Fails
    /// with `Invalid` when a limit is zero.
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

    /// Reads and scans the prefix within `work`. Fails with `Unsupported` when no AVC
    /// or HEVC signature appears within `maximum_bytes` or before the source ends, and
    /// with `Limit` after `maximum_nals` NAL units. A failure other than backpressure or
    /// allocation closes the probe.
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
                return Err(Error::Invalid("raw probe selected a container"));
            }
        })
    }

    /// Allocation pressure leaves the retained prefix and source retryable.
    pub fn finish(&mut self) -> Result<RawVideoSource<ProbeReplay<S>>> {
        let Some(PlaybackInputKind::RawVideo(codec)) = self.driver.detected else {
            return Err(Error::Invalid("raw video probe not ready"));
        };
        self.driver.finish_raw(codec, self.reader_limits)
    }
}

/// The kind of input a [`PlaybackProbe`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackInputKind {
    /// A Matroska, MP4 or transport stream container.
    Container(Format),
    /// Raw Annex B video of this codec.
    RawVideo(Codec),
}

/// One step of a [`PlaybackProbe`].
#[derive(Debug)]
pub enum PlaybackProbeStep {
    /// The source is pending on this request: advance again once it can reply.
    NeedInput(InputRequest),
    /// The work budget ran out: advance again with more work.
    Yield,
    /// The input kind is known: call `finish`.
    Detected(PlaybackInputKind),
}

/// The selected input. It replays the probed prefix, then reads on from the source.
#[allow(
    clippy::large_enum_variant,
    reason = "one inline selected reader avoids a separate uncharged allocation"
)]
#[derive(Debug)]
pub enum PlaybackInput<S: Source> {
    /// A container input for the playback engine.
    Container(SelectedInput<ProbeReplay<S>>),
    /// A raw Annex B input.
    RawVideo {
        /// The codec its first parameter sets and slice belong to.
        codec: Codec,
        /// The raw source, from byte zero.
        source: RawVideoSource<ProbeReplay<S>>,
    },
}

/// Common selector for the existing container signatures and the bounded raw
/// Annex B probe. It keeps the actual prefix when the transport cannot rewind.
#[derive(Debug)]
pub struct PlaybackProbe<S: Source> {
    driver: ProbeDriver<S>,
}

impl<S: Source> PlaybackProbe<S> {
    /// A probe over `source`; `window` is the input window of a selected container
    /// input. Fails with `Invalid` when a limit is zero or `limits.maximum_bytes` is
    /// below `CONTAINER_SIGNATURE_BYTES`.
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

    /// Reads and scans the prefix within `work`: container signatures first, then raw
    /// Annex B parameter sets. Fails as [`RawVideoProbe::advance`] does.
    pub fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<PlaybackProbeStep> {
        self.driver.advance(work, cancel)
    }

    /// Hands over the source with the probed prefix replayed, as a container input or
    /// as a raw source read with `raw_limits`. Fails with `Invalid` before detection
    /// and with `Closed` after a failure or an earlier `finish`.
    pub fn finish(&mut self, raw_limits: RawAnnexBLimits) -> Result<PlaybackInput<S>> {
        let selection = self
            .driver
            .detected
            .ok_or(Error::Invalid("playback probe not ready"))?;
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

#[derive(Debug)]
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
            return Err(Error::Invalid("zero raw video probe limit"));
        }
        let container_pending = mode == ProbeMode::Playback;
        if container_pending && limits.maximum_bytes < CONTAINER_SIGNATURE_BYTES {
            return Err(Error::Invalid(
                "playback probe byte bound below container signature",
            ));
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
        if result.is_err() && !matches!(result, Err(Error::Backpressure { .. } | Error::Allocation))
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
                return Err(Error::Cancelled);
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
                        return Err(Error::Invalid("raw video probe lost buffered input"));
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
            if !self.container_pending && self.scanned < self.prefix.len() {
                // One work unit per scanned byte, as many as the budget allows.
                let pending = &self.prefix.as_slice()[self.scanned..];
                let allowed = usize::try_from(work.remaining())
                    .unwrap_or(usize::MAX)
                    .min(pending.len());
                let (scanned, codec) = self
                    .scan
                    .scan(&pending[..allowed], self.limits.maximum_nals)?;
                work.take(scanned as u64);
                self.scanned += scanned;
                self.detected = codec.map(PlaybackInputKind::RawVideo);
                continue;
            }
            if self.end.is_some() {
                return Err(Error::Unsupported("raw video signature absent at EOF"));
            }
            let left = self.limits.maximum_bytes - self.prefix.len();
            if left == 0 {
                return Err(Error::Unsupported(
                    "raw video signature absent within probe bound",
                ));
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
                return Err(Error::Invalid("raw video probe reply exceeds request"));
            }
            let ended = matches!(reply, SourceReply::End { .. });
            if !reply.provide_to(&mut self.input)? {
                return Ok(PlaybackProbeStep::NeedInput(request));
            }
            if ended {
                if !self.container_pending {
                    return Err(Error::Unsupported("raw video signature absent at EOF"));
                }
                self.end = Some(self.input.position());
            }
        }
    }

    fn reserve_window(&mut self, count: usize) -> Result<()> {
        let needed = self
            .prefix
            .len()
            .checked_add(count)
            .ok_or(Error::Overflow)?;
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
                    && matches!(error, Error::Backpressure { .. } | Error::Limit { .. })
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
                self.prefix = rejected.value;
                return Err(rejected.error);
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
#[derive(Debug)]
pub struct ProbeReplay<S: Source> {
    id: SourceId,
    prefix: Option<Bytes>,
    end: Option<u64>,
    source: SourceOwner<S>,
}

impl<S: Source> Source for ProbeReplay<S> {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceReply> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if request.source != self.id {
            return Err(Error::SourceMismatch);
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
        self.source.poll(request, work, cancel)
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

#[derive(Debug, Default)]
struct Head {
    prefix: [u8; 5],
    len: usize,
}

impl Head {
    fn push(&mut self, byte: u8) -> Result<()> {
        if let Some(next) = self.prefix.get_mut(self.len) {
            *next = byte;
        }
        self.len = self.len.checked_add(1).ok_or(Error::Overflow)?;
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

#[derive(Debug, Default)]
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
    /// Push `bytes` in order until one selects a codec. Returns the number of
    /// bytes consumed, the selecting byte included, and the selection.
    ///
    /// Past the five bytes a NAL head keeps, a nonzero byte that follows a
    /// nonzero byte only lengthens the NAL, so such runs are skipped whole.
    fn scan(&mut self, bytes: &[u8], maximum_nals: usize) -> Result<(usize, Option<Codec>)> {
        let mut at = 0;
        while at < bytes.len() {
            if self.zeros == 0
                && let Some(head) = &mut self.head
                && head.len >= head.prefix.len()
            {
                let run = nonzero_prefix_len(&bytes[at..]);
                head.len = head.len.checked_add(run).ok_or(Error::Overflow)?;
                at += run;
                if at == bytes.len() {
                    break;
                }
            }
            let byte = bytes[at];
            at += 1;
            if let Some(codec) = self.push(byte, maximum_nals)? {
                return Ok((at, Some(codec)));
            }
        }
        Ok((at, None))
    }

    fn push(&mut self, byte: u8, maximum_nals: usize) -> Result<Option<Codec>> {
        if byte == 0 {
            self.zeros = self.zeros.checked_add(1).ok_or(Error::Overflow)?;
            return Ok(None);
        }
        let zeros = std::mem::take(&mut self.zeros);
        if byte == 1 && zeros >= 2 {
            if let Some(head) = self.head.take() {
                self.complete(&head)?;
            }
            self.count = self.count.checked_add(1).ok_or(Error::Overflow)?;
            if self.count > maximum_nals {
                return Err(Error::Limit {
                    requested: self.count,
                    maximum: maximum_nals,
                });
            }
            self.head = Some(Head::default());
            return Ok(None);
        }
        let Some(head) = &mut self.head else {
            return Err(Error::Unsupported(
                "raw video probe needs a leading Annex B start code",
            ));
        };
        if zeros >= 3 {
            return Err(Error::Invalid(
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

    fn complete(&mut self, head: &Head) -> Result<()> {
        if head.len == 0 {
            return Err(Error::Invalid("empty raw probe NAL"));
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
            return Err(Error::Invalid(
                "ambiguous raw Annex B parameter-set signatures",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Signatures;
    use triergon_core::Result;
    use triergon_framing::nal::Codec;

    const MAXIMUM_NALS: usize = 64;

    /// The bytes consumed and the selection of one byte at a time.
    fn pushed(bytes: &[u8]) -> Result<(usize, Option<Codec>)> {
        let mut signatures = Signatures::default();
        for (index, &byte) in bytes.iter().enumerate() {
            if let Some(codec) = signatures.push(byte, MAXIMUM_NALS)? {
                return Ok((index + 1, Some(codec)));
            }
        }
        Ok((bytes.len(), None))
    }

    /// The same through `scan`, given `chunk` bytes at a time.
    fn scanned(bytes: &[u8], chunk: usize) -> Result<(usize, Option<Codec>)> {
        let mut signatures = Signatures::default();
        let mut consumed = 0;
        for piece in bytes.chunks(chunk) {
            let (count, codec) = signatures.scan(piece, MAXIMUM_NALS)?;
            consumed += count;
            if codec.is_some() {
                return Ok((consumed, codec));
            }
            assert_eq!(count, piece.len());
        }
        Ok((consumed, None))
    }

    #[test]
    fn scanning_runs_selects_at_the_byte_one_byte_at_a_time_does() {
        let inputs: [&[u8]; 9] = [
            include_bytes!("../../framing/fixtures/raw-annex-b/hevc-aud0.hevc"),
            include_bytes!("../../framing/fixtures/raw-annex-b/hevc-aud1.hevc"),
            include_bytes!("../../framing/fixtures/raw-annex-b/avc-aud0.h264"),
            include_bytes!("../../framing/fixtures/raw-annex-b/avc-aud1.h264"),
            include_bytes!("../../playback/tests/fixtures/raw-video/avc-80x48.h264"),
            include_bytes!("../../playback/tests/fixtures/raw-video/avc-short.h264"),
            // A NAL body with emulation prevention, then padding inside it.
            &[
                0, 0, 1, 0x40, 1, 0x0c, 1, 0xff, 0, 0, 3, 1, 0x5a, 0, 0, 0, 7,
            ],
            // A body byte before any start code.
            &[0, 5, 0, 0, 1, 0x40, 1],
            // Nothing but padding.
            &[0; 40],
        ];
        for input in inputs {
            let expected = pushed(input);
            for end in (0..input.len().min(800)).chain([input.len()]) {
                let prefix = &input[..end];
                let whole = pushed(prefix);
                for chunk in [1, 2, 3, 7, 8, 64, 4096] {
                    assert_eq!(
                        scanned(prefix, chunk),
                        whole,
                        "{end} bytes, {chunk} at a time"
                    );
                }
            }
            assert_eq!(scanned(input, 4096), expected);
        }
        // The HEVC fixtures select at their first slice, after a 2.3 KB SEI.
        assert!(matches!(pushed(inputs[1]), Ok((2431, Some(Codec::Hevc)))));
    }
}
