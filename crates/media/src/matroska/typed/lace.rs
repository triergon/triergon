//! RFC 9559 §10.3 descriptor state; every range is relative to the same owner.
use super::syntax::check;
use crate::{Error, Result, ebml, incremental::ParseStep, storage::SharedBytes};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;

#[derive(Debug)]
enum Phase {
    Count,
    Sizes,
    Last,
    PrepareRanges,
    Done,
}
#[derive(Debug)]
pub(crate) struct LaceJob {
    source: SharedBytes,
    range: Range<usize>,
    mode: u8,
    phase: Phase,
    at: usize,
    count: usize,
    frame: usize,
    xiph: usize,
    // Sum of descriptor lengths during validation and payload offset during replay.
    payload_at: usize,
    previous_size: usize,
    data_start: usize,
    fixed_size: usize,
    ready: Option<Range<usize>>,
    failed: bool,
}
impl LaceJob {
    pub(crate) fn new(source: SharedBytes, range: Range<usize>, mode: u8) -> Result<Self> {
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
            count: 0,
            frame: 0,
            xiph: 0,
            payload_at: 0,
            previous_size: 0,
            data_start: 0,
            fixed_size: 0,
            ready: None,
            failed: false,
        })
    }
    pub(crate) fn step_descriptors(
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
    pub(crate) fn frame_count(&self) -> Option<usize> {
        matches!(self.phase, Phase::Done).then_some(self.count)
    }

    /// Validate every descriptor before replaying the immutable header under the
    /// same work budget. The cached range survives frame-allocation pressure.
    pub(crate) fn frame_range(
        &mut self,
        index: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Range<usize>>> {
        if self.failed || !matches!(self.phase, Phase::Done) || index >= self.count {
            return Err(Error::Invalid("missing frame range"));
        }
        let result = self.advance_range(index, work, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn advance_range(
        &mut self,
        index: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Range<usize>>> {
        check(cancel)?;
        if let Some(range) = &self.ready {
            if self.frame == index {
                return Ok(Some(range.clone()));
            }
            if index != self.frame + 1 {
                return Err(Error::Invalid("lace frame order"));
            }
            self.frame = index;
            self.ready = None;
        }
        if self.frame != index {
            return Err(Error::Invalid("lace frame order"));
        }
        loop {
            check(cancel)?;
            if !work.try_charge(1) {
                return Ok(None);
            }
            let size = if self.frame == self.count - 1 {
                self.range
                    .end
                    .checked_sub(self.payload_at)
                    .ok_or(Error::Overflow)?
            } else if self.mode == 2 {
                self.fixed_size
            } else {
                let Some(size) = self.read_size(self.data_start)? else {
                    continue;
                };
                size
            };
            let end = self.payload_at.checked_add(size).ok_or(Error::Overflow)?;
            if end > self.range.end {
                return Err(Error::Truncated);
            }
            let range = self.payload_at..end;
            self.ready = Some(range.clone());
            self.payload_at = end;
            return Ok(Some(range));
        }
    }

    fn finish_descriptors(&mut self) {
        self.data_start = self.at;
        self.phase = Phase::PrepareRanges;
    }

    fn read_size(&mut self, end: usize) -> Result<Option<usize>> {
        let size = if self.mode == 1 {
            let byte = *self
                .source
                .get(self.at)
                .filter(|_| self.at < end)
                .ok_or(Error::Truncated)?;
            self.at += 1;
            self.xiph = self
                .xiph
                .checked_add(usize::from(byte))
                .ok_or(Error::Overflow)?;
            if byte == 255 {
                return Ok(None);
            }
            std::mem::take(&mut self.xiph)
        } else {
            let (value, width, _) = ebml::vint(&self.source[self.at..end], false)?;
            self.at += width;
            if self.frame == 0 {
                usize::try_from(value).map_err(|_| Error::Overflow)?
            } else {
                let delta = i128::from(value) - ((1_i128 << (7 * width - 1)) - 1);
                usize::try_from(self.previous_size as i128 + delta)
                    .map_err(|_| Error::Invalid("negative/overflow lace size"))?
            }
        };
        self.previous_size = size;
        Ok(Some(size))
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
                        self.count = 1;
                        self.finish_descriptors();
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
                    self.count = count;
                    if self.mode == 2 {
                        let bytes = self.range.end - self.at;
                        if !bytes.is_multiple_of(count) {
                            return Err(Error::Invalid("fixed lace remainder"));
                        }
                        self.fixed_size = bytes / count;
                        self.finish_descriptors();
                    } else if matches!(self.mode, 1 | 3) {
                        self.phase = Phase::Sizes;
                    } else {
                        return Err(Error::Invalid("lace mode"));
                    }
                }
                Phase::Sizes => {
                    if self.frame == self.count - 1 {
                        self.phase = Phase::Last;
                        continue;
                    }
                    let Some(size) = self.read_size(self.range.end)? else {
                        continue;
                    };
                    self.payload_at = self.payload_at.checked_add(size).ok_or(Error::Overflow)?;
                    self.frame += 1;
                }
                Phase::Last => {
                    self.range
                        .end
                        .checked_sub(self.at)
                        .and_then(|remaining| remaining.checked_sub(self.payload_at))
                        .ok_or(Error::Invalid("lace exceeds block"))?;
                    self.finish_descriptors();
                }
                Phase::PrepareRanges => {
                    self.at = self.range.start + usize::from(self.mode != 0);
                    self.frame = 0;
                    self.xiph = 0;
                    self.payload_at = self.data_start;
                    self.previous_size = 0;
                    self.phase = Phase::Done;
                }
                Phase::Done => unreachable!(),
            }
        }
    }
}

super::usage::storage!(LaceJob, |s| [s.source]);

#[cfg(test)]
mod tests {
    use super::*;

    fn lace(data: &[u8], mode: u8) -> Result<Vec<Vec<u8>>> {
        let pool = triergon_core::Budget::new(1 << 16)?;
        let source = SharedBytes::try_from_vec(&pool, data.to_vec())?;
        let mut job = LaceJob::new(source, 0..data.len(), mode)?;
        loop {
            match job.step_descriptors(&mut WorkBudget::new(u64::MAX), &AtomicBool::new(false))? {
                ParseStep::Event(()) => break,
                ParseStep::Yield => continue,
                other => panic!("unexpected lace step {other:?}"),
            }
        }
        let mut frames = Vec::new();
        for index in 0..job.frame_count().unwrap() {
            let Some(range) = job.frame_range(
                index,
                &mut WorkBudget::new(u64::MAX),
                &AtomicBool::new(false),
            )?
            else {
                panic!("unlimited lace work yielded");
            };
            frames.push(data[range].to_vec());
        }
        Ok(frames)
    }

    #[test]
    fn all_lacing_schemes_and_malformed_lengths() {
        assert_eq!(lace(&[1, 1, 4, 5, 6], 1).unwrap(), [&[4][..], &[5, 6]]);
        assert_eq!(lace(&[1, 4, 5, 6, 7], 2).unwrap(), [&[4, 5][..], &[6, 7]]);
        assert_eq!(
            lace(&[2, 0x81, 0xc0, 4, 5, 6, 7, 8, 9], 3).unwrap(),
            [&[4][..], &[5, 6], &[7, 8, 9]]
        );
        for b in [&[][..], &[0][..], &[1, 255][..], &[2, 0x81, 0x80][..]] {
            assert!(lace(b, 3).is_err(), "{b:?}");
        }
        assert!(lace(&[1, 0, 1, 2], 2).is_err());
    }

    #[test]
    fn xiph_range_resumes_each_octet_and_checks_cancellation_when_cached() {
        let mut data = vec![1, 255, 255, 255, 0];
        data.resize(771, b'A');
        let pool = triergon_core::Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::try_from_vec(&pool, data).unwrap();
        let mut job = LaceJob::new(source, 0..771, 1).unwrap();
        let running = AtomicBool::new(false);
        assert!(matches!(
            job.step_descriptors(&mut WorkBudget::new(32), &running)
                .unwrap(),
            ParseStep::Event(())
        ));
        for _ in 0..3 {
            assert!(
                job.frame_range(0, &mut WorkBudget::new(1), &running)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(matches!(
            job.frame_range(0, &mut WorkBudget::new(1), &running).unwrap(),
            Some(range) if range == (5..770)
        ));
        assert_eq!(
            job.frame_range(0, &mut WorkBudget::new(0), &AtomicBool::new(true))
                .unwrap_err(),
            Error::Cancelled
        );
        assert!(
            job.frame_range(0, &mut WorkBudget::new(1), &running)
                .is_err()
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn ebml_size_delta_uses_full_width_until_the_lace_is_rejected() {
        let size = 1_u64 << 40;
        let delta = (1_u64 << 55) - 1 - size;
        let marker = 1_u64 << 56;
        let mut data = vec![2];
        data.extend_from_slice(&(marker | size).to_be_bytes());
        data.extend_from_slice(&(marker | delta).to_be_bytes());
        assert_eq!(lace(&data, 3), Err(Error::Invalid("lace exceeds block")));
    }
}
