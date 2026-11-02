//! RFC 9559 §10.3 descriptor state; every range is relative to the same owner.
use super::syntax::check;
use crate::{Error, Result, ebml, incremental::ParseStep, storage::SharedBytes};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;

#[derive(Clone, Debug)]
pub(crate) struct Frames {
    // Before Ranges these are lengths; each charged Ranges step replaces one
    // length with its checked absolute end. A start is the preceding end, so
    // full-width offsets need one word per frame without allocation/narrowing.
    ends: [usize; 256],
    base: usize,
    count: usize,
}
impl Frames {
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn get(&self, index: usize) -> Option<Range<usize>> {
        let end = *self.ends.get(index).filter(|_| index < self.count)?;
        let start = if index == 0 {
            self.base
        } else {
            self.ends[index - 1]
        };
        Some(start..end)
    }
    #[cfg(test)]
    pub fn ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        self.ends[..self.count]
            .iter()
            .enumerate()
            .map(|(index, end)| {
                let start = if index == 0 {
                    self.base
                } else {
                    self.ends[index - 1]
                };
                start..*end
            })
    }
}
#[derive(Debug)]
enum Phase {
    Count,
    Sizes,
    Last,
    Ranges,
    Done,
}
#[derive(Debug)]
pub(crate) struct LaceJob {
    source: SharedBytes,
    range: Range<usize>,
    mode: u8,
    phase: Phase,
    at: usize,
    frames: Frames,
    n: usize,
    xiph: usize,
    used: usize,
    failed: bool,
}
impl LaceJob {
    pub fn new(source: SharedBytes, range: Range<usize>, mode: u8) -> Result<Self> {
        if range.start > range.end || range.end > source.len() {
            return Err(Error::Truncated);
        }
        let at = range.start;
        Ok(Self {
            source,
            range,
            mode,
            phase: Phase::Count,
            at,
            frames: Frames {
                ends: [0; 256],
                base: 0,
                count: 0,
            },
            n: 0,
            xiph: 0,
            used: 0,
            failed: false,
        })
    }
    pub fn step_descriptors(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<()>> {
        if self.failed {
            return Err(Error::Invalid("lace requires replacement after error"));
        }
        let result = self.advance(work, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    /// Borrow the one retained descriptor arena only after validation completes.
    /// Packet batches keep this job until all restored frames are staged.
    pub fn frames(&self) -> Option<&Frames> {
        matches!(self.phase, Phase::Done).then_some(&self.frames)
    }
    #[cfg(test)]
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Frames>> {
        match self.step_descriptors(work, cancel)? {
            ParseStep::Event(()) => {
                let Some(frames) = self.frames() else {
                    return Err(Error::Invalid("missing lace frames"));
                };
                Ok(ParseStep::Event(frames.clone()))
            }
            other => super::pending(other),
        }
    }
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep<()>> {
        loop {
            check(cancel)?;
            if matches!(self.phase, Phase::Done) {
                return Ok(ParseStep::Event(()));
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            match self.phase {
                Phase::Count => {
                    if self.mode == 0 {
                        self.frames.count = 1;
                        self.frames.ends[0] = self.range.len();
                        self.phase = Phase::Ranges;
                        continue;
                    }
                    let count = usize::from(
                        *self
                            .source
                            .get(self.at)
                            .filter(|_| self.at < self.range.end)
                            .ok_or(Error::Truncated)?,
                    ) + 1;
                    if count < 2 {
                        return Err(Error::Invalid("lacing requires multiple frames"));
                    }
                    self.at += 1;
                    self.frames.count = count;
                    if self.mode == 2 {
                        let bytes = self.range.end - self.at;
                        if bytes % count != 0 {
                            return Err(Error::Invalid("fixed lace remainder"));
                        }
                        // Fill each fixed size in its separately charged range step.
                        self.used = bytes / count;
                        self.phase = Phase::Ranges;
                    } else if matches!(self.mode, 1 | 3) {
                        self.phase = Phase::Sizes;
                    } else {
                        return Err(Error::Invalid("lace mode"));
                    }
                }
                Phase::Sizes => {
                    if self.n == self.frames.count - 1 {
                        self.phase = Phase::Last;
                        continue;
                    }
                    let size = if self.mode == 1 {
                        let b = *self
                            .source
                            .get(self.at)
                            .filter(|_| self.at < self.range.end)
                            .ok_or(Error::Truncated)?;
                        self.at += 1;
                        self.xiph = self
                            .xiph
                            .checked_add(usize::from(b))
                            .ok_or(Error::Overflow)?;
                        if b == 255 {
                            continue;
                        }
                        std::mem::take(&mut self.xiph)
                    } else {
                        let (value, width, _) =
                            ebml::vint(&self.source[self.at..self.range.end], false)?;
                        self.at += width;
                        if self.n == 0 {
                            usize::try_from(value).map_err(|_| Error::Overflow)?
                        } else {
                            let delta = i128::from(value) - ((1_i128 << (7 * width - 1)) - 1);
                            usize::try_from(self.frames.ends[self.n - 1] as i128 + delta)
                                .map_err(|_| Error::Invalid("negative/overflow lace size"))?
                        }
                    };
                    self.frames.ends[self.n] = size;
                    self.used = self.used.checked_add(size).ok_or(Error::Overflow)?;
                    self.n += 1;
                }
                Phase::Last => {
                    self.frames.ends[self.frames.count - 1] = self
                        .range
                        .end
                        .checked_sub(self.at)
                        .and_then(|n| n.checked_sub(self.used))
                        .ok_or(Error::Invalid("lace exceeds block"))?;
                    self.n = 0;
                    self.phase = Phase::Ranges;
                }
                Phase::Ranges => {
                    if self.n == 0 {
                        self.frames.base = self.at;
                    }
                    if self.n == self.frames.count {
                        if self.at != self.range.end {
                            return Err(Error::Invalid("unconsumed lace bytes"));
                        }
                        self.phase = Phase::Done;
                        continue;
                    }
                    let size = if self.mode == 2 {
                        self.used
                    } else {
                        self.frames.ends[self.n]
                    };
                    let end = self.at.checked_add(size).ok_or(Error::Overflow)?;
                    if end > self.range.end {
                        return Err(Error::Truncated);
                    }
                    self.frames.ends[self.n] = end;
                    self.n += 1;
                    self.at = end;
                }
                Phase::Done => unreachable!(),
            }
        }
    }
}

super::usage::storage!(LaceJob, |s| [s.source]);
