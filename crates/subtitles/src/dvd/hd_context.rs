//! Explicit pinned-receiver state across complete, externally framed HD SPUs.
//! Raw source fields stay in their own packets. Cache entries retain their
//! charged original owner and span; no palette bytes are silently synthesized.
use super::{
    HdAlphaOrigin, HdFacts, HdFourPalette, HdLimits, HdReceiverJob, HdReceiverOutput,
    HdReceiverStatus, HdSpan, HdStatus, HdSubpicture, hd_argb,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Debug)]
struct Field {
    owner: Bytes,
    span: HdSpan,
}
impl Field {
    fn new(owner: &Bytes, span: HdSpan, length: usize) -> Result<Self> {
        if span.get(owner)?.len() != length {
            return Err(Error::Invalid("HD context source extent"));
        }
        Ok(Self {
            owner: owner.clone(),
            span,
        })
    }
    fn raw(&self) -> Result<&[u8]> {
        self.span.get(&self.owner)
    }
    fn word(&self) -> Result<u16> {
        let bytes = self.raw()?;
        let word = bytes
            .try_into()
            .map_err(|_| Error::Invalid("HD context word extent"))?;
        Ok(u16::from_be_bytes(word))
    }
}
#[derive(Clone, Copy, Debug)]
pub enum HdContextOrigin<'a> {
    ReceiverReset,
    Contrast { owner: &'a Bytes, span: HdSpan },
    Packed { owner: &'a Bytes, span: HdSpan },
}
impl<'a> HdContextOrigin<'a> {
    /// A caller may copy and edit public metadata; source access stays checked.
    pub fn raw(self) -> Result<Option<&'a [u8]>> {
        match self {
            Self::ReceiverReset => Ok(None),
            Self::Contrast { owner, span } | Self::Packed { owner, span } => {
                span.get(owner).map(Some)
            }
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct HdContextAlpha<'a> {
    pub value: u8,
    pub origin: HdContextOrigin<'a>,
}

/// At most three source owners: full contrast, a later packed alpha override,
/// and four-entry selectors. Cloning shares their existing full budget charges.
/// YCrCb, geometry, clocks and forced/menu state do not persist in this cache.
#[derive(Clone, Debug)]
pub struct HdReceiverContext {
    budget: Budget,
    contrast: Option<Field>,
    alpha4: Option<Field>,
    selectors: Option<Field>,
}
impl HdReceiverContext {
    pub fn new(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            contrast: None,
            alpha4: None,
            selectors: None,
        }
    }
    pub fn reset(&mut self) {
        self.contrast = None;
        self.alpha4 = None;
        self.selectors = None;
    }
    pub fn retained_sources(&self) -> [Option<&Bytes>; 3] {
        [
            self.contrast.as_ref().map(|v| &v.owner),
            self.alpha4.as_ref().map(|v| &v.owner),
            self.selectors.as_ref().map(|v| &v.owner),
        ]
    }
    pub fn alpha(&self, index: u8) -> Result<HdContextAlpha<'_>> {
        let i = usize::from(index);
        if i < 4
            && let Some(field) = &self.alpha4
        {
            return Ok(HdContextAlpha {
                value: ((field.word()? >> (4 * i)) & 15) as u8,
                origin: HdContextOrigin::Packed {
                    owner: &field.owner,
                    span: field.span,
                },
            });
        }
        if let Some(field) = &self.contrast {
            let raw = *field.raw()?.get(i).ok_or(Error::Truncated)?;
            return Ok(HdContextAlpha {
                value: 255 - raw,
                origin: HdContextOrigin::Contrast {
                    owner: &field.owner,
                    span: field.span,
                },
            });
        }
        Ok(HdContextAlpha {
            value: 0,
            origin: HdContextOrigin::ReceiverReset,
        })
    }
    pub fn selectors_origin(&self) -> HdContextOrigin<'_> {
        self.selectors
            .as_ref()
            .map_or(HdContextOrigin::ReceiverReset, |f| {
                HdContextOrigin::Packed {
                    owner: &f.owner,
                    span: f.span,
                }
            })
    }
    pub fn selectors(&self) -> Result<[u8; 4]> {
        let word = self.selectors.as_ref().map_or(Ok(0), Field::word)?;
        Ok(std::array::from_fn(|i| ((word >> (4 * i)) & 15) as u8))
    }
    fn with_facts(&self, packet: &Bytes, facts: HdFacts) -> Result<Self> {
        if !packet.belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        // Validate every source span before producing a modified state.
        let contrast = facts
            .contrast
            .map(|span| Field::new(packet, span, 256))
            .transpose()?;
        let alpha = facts
            .alpha4_wire
            .map(|span| Field::new(packet, span, 2))
            .transpose()?;
        let selectors = facts
            .palette4_wire
            .map(|span| Field::new(packet, span, 2))
            .transpose()?;
        let mut next = self.clone();
        if let Some(contrast) = contrast {
            next.contrast = Some(contrast);
            next.alpha4 = None;
        }
        if let Some(alpha) = alpha
            && facts.contrast.is_none_or(|c| alpha.span.start > c.start)
        {
            next.alpha4 = Some(alpha);
        }
        if let Some(selectors) = selectors {
            next.selectors = Some(selectors);
        }
        Ok(next)
    }
    pub fn palette_job<'a>(&'a self, picture: &'a HdSubpicture) -> Result<HdContextPaletteJob<'a>> {
        if !picture.packet().belongs_to(&self.budget) {
            return Err(Error::ForeignPool);
        }
        if !picture.facts().eight_bit {
            return Err(Error::Unsupported(
                "HD context eight-bit view of four-entry raster",
            ));
        }
        let yuv = picture
            .ycrcb()
            .ok_or(Error::Unsupported("HD source YCrCb absent"))?;
        Ok(HdContextPaletteJob {
            context: self,
            yuv,
            values: [0; 256],
            at: 0,
            failed: None,
            finished: false,
        })
    }
    pub fn four_palette(&self, source_rgb: Option<&[u32; 16]>) -> Result<HdFourPalette> {
        let selectors = self.selectors()?;
        let mut alpha = [0_u8; 4];
        let mut origins = [HdAlphaOrigin::ReceiverReset; 4];
        for i in 0..4 {
            let value = self.alpha(i as u8)?;
            alpha[i] = value.value;
            origins[i] = match value.origin {
                HdContextOrigin::ReceiverReset => HdAlphaOrigin::ReceiverReset,
                HdContextOrigin::Contrast { .. } => HdAlphaOrigin::Contrast {
                    index: i as u8,
                    raw: 255 - value.value,
                },
                HdContextOrigin::Packed { owner, span } => {
                    let raw = span.get(owner)?;
                    let word = u16::from_be_bytes(raw.try_into().map_err(|_| Error::Truncated)?);
                    HdAlphaOrigin::Packed {
                        word,
                        index: i as u8,
                    }
                }
            };
        }
        let mut classes = [u8::MAX; 16];
        let mut count = 0_u8;
        for i in 0..4 {
            if alpha[i] != 0 && classes[usize::from(selectors[i])] == u8::MAX {
                classes[usize::from(selectors[i])] = count;
                count += 1;
            }
        }
        let argb = std::array::from_fn(|i| {
            let rgb = if let Some(source) = source_rgb {
                source[usize::from(selectors[i])] & 0x00ff_ffff
            } else if alpha[i] == 0 {
                0
            } else {
                let rank = classes[usize::from(selectors[i])];
                let level = if count == 1 {
                    255
                } else {
                    (255 * u16::from(rank) + u16::from(count - 1) / 2) / u16::from(count - 1)
                };
                let gray = 255 * u32::from(level) / 256;
                gray * 0x0001_0101
            };
            (((u32::from(alpha[i]) * 17) & 255) << 24) | rgb
        });
        Ok(HdFourPalette {
            argb,
            selectors,
            effective_alpha: alpha,
            alpha_origins: origins,
            source_rgb: source_rgb.copied(),
        })
    }
}

#[derive(Debug)]
pub struct HdContextOutput {
    source: HdReceiverOutput,
    picture_state: HdReceiverContext,
    next_state: HdReceiverContext,
}
impl HdContextOutput {
    pub fn source(&self) -> &HdReceiverOutput {
        &self.source
    }
    pub fn picture_state(&self) -> &HdReceiverContext {
        &self.picture_state
    }
    pub fn next_state(&self) -> &HdReceiverContext {
        &self.next_state
    }
    pub fn into_parts(self) -> (HdReceiverOutput, HdReceiverContext, HdReceiverContext) {
        (self.source, self.picture_state, self.next_state)
    }
}
/// The final constant-size state commit costs one work unit. A failed or
/// cancelled job cannot mutate the caller's previous context. Native source
/// rejection can publish parsed prefix state alongside its retained error.
#[derive(Debug)]
pub struct HdContextJob {
    inner: Option<HdReceiverJob>,
    initial: Option<HdReceiverContext>,
    output: Option<HdContextOutput>,
    failed: Option<Error>,
    finished: bool,
    work: usize,
}
impl HdContextJob {
    pub fn new(
        packet: Bytes,
        budget: &Budget,
        limits: HdLimits,
        initial: &HdReceiverContext,
    ) -> Result<Self> {
        Self::with_extent(packet, budget, limits, initial, false)
    }
    pub(crate) fn with_prefix(
        packet: Bytes,
        budget: &Budget,
        limits: HdLimits,
        initial: &HdReceiverContext,
    ) -> Result<Self> {
        Self::with_extent(packet, budget, limits, initial, true)
    }
    fn with_extent(
        packet: Bytes,
        budget: &Budget,
        limits: HdLimits,
        initial: &HdReceiverContext,
        prefix: bool,
    ) -> Result<Self> {
        if !initial.budget.same_pool(budget) {
            return Err(Error::ForeignPool);
        }
        let mut source_limits = limits;
        source_limits.max_work = limits.max_work.saturating_sub(1);
        let inner = if prefix {
            HdReceiverJob::with_prefix(packet, budget, source_limits)?
        } else {
            HdReceiverJob::new(packet, budget, source_limits)?
        };
        Ok(Self {
            inner: Some(inner),
            initial: Some(initial.clone()),
            output: None,
            failed: None,
            finished: false,
            work: 0,
        })
    }
    pub fn finish_input(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.finish_input();
        }
    }
    pub fn work_spent(&self) -> usize {
        self.work
    }
    fn poison(&mut self, error: Error) -> Error {
        self.inner = None;
        self.initial = None;
        self.output = None;
        self.failed = Some(error);
        error
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<HdReceiverStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.finished {
            return Err(self.poison(Error::Invalid("HD context already finished")));
        }
        if self.output.is_some() {
            return Ok(HdReceiverStatus::Ready);
        }
        let result = self.inner_poll(work, cancel);
        match result {
            Err(e @ Error::Backpressure { .. }) => Err(e),
            Err(e) => Err(self.poison(e)),
            value => value,
        }
    }
    fn inner_poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<HdReceiverStatus> {
        let inner = self
            .inner
            .as_mut()
            .ok_or(Error::Invalid("HD context source job absent"))?;
        let status = inner.poll(work, cancel);
        self.work = inner.work_spent();
        let status = status?;
        if status != HdReceiverStatus::Ready {
            return Ok(status);
        }
        if !work.try_charge(1) {
            return Ok(HdReceiverStatus::Yield);
        }
        self.work += 1;
        let source = inner.try_finish(cancel)?;
        let initial = self
            .initial
            .as_ref()
            .ok_or(Error::Invalid("HD context initial state absent"))?;
        let picture_state = initial.with_facts(source.packet(), source.source_facts())?;
        let next_state = initial.with_facts(source.packet(), source.parsed_context_facts())?;
        self.output = Some(HdContextOutput {
            source,
            picture_state,
            next_state,
        });
        self.initial = None;
        self.inner = None;
        Ok(HdReceiverStatus::Ready)
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<HdContextOutput> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.finished {
            return Err(Error::Invalid("HD context already finished"));
        }
        let out = self
            .output
            .take()
            .ok_or(Error::Invalid("HD context not ready"))?;
        self.finished = true;
        Ok(out)
    }
}

#[derive(Debug)]
pub struct HdContextPaletteJob<'a> {
    context: &'a HdReceiverContext,
    yuv: &'a [u8],
    values: [u32; 256],
    at: usize,
    failed: Option<Error>,
    finished: bool,
}
impl HdContextPaletteJob<'_> {
    pub fn entries_done(&self) -> usize {
        self.at
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<HdStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            self.failed = Some(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        if self.finished {
            return Err(Error::Invalid("HD context palette already finished"));
        }
        while self.at < 256 {
            if cancel.load(Ordering::Relaxed) {
                self.failed = Some(Error::Cancelled);
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(HdStatus::Yield);
            }
            let i = self.at;
            let alpha = self.context.alpha(i as u8)?.value;
            self.values[i] = hd_argb(
                [self.yuv[3 * i], self.yuv[3 * i + 1], self.yuv[3 * i + 2]],
                255 - alpha,
            );
            self.at += 1;
        }
        Ok(HdStatus::Ready)
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<[u32; 256]> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            self.failed = Some(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        if self.at != 256 || self.finished {
            return Err(Error::Invalid("HD context palette not ready"));
        }
        self.finished = true;
        Ok(self.values)
    }
}
