//! Intended child of zlib, so the pinned header/Adler implementation is reused.
//! One charged compressed frame, persistent transform state, no I/O adapter.
use super::{InflateLimits, Inflater, adler32, header};
use crate::matroska::typed::syntax::Required;
use crate::{
    Error, Limits, Result,
    content_encoding::{DecodeStep, Decoder},
    incremental::ParseStep,
    storage::SharedBytes,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, BudgetVec, WorkBudget};

#[derive(Debug, Clone)]
pub(crate) struct RestorePlan {
    steps: [Option<DecodeStep>; 8],
    count: usize,
    borrow_stored: bool,
}
impl RestorePlan {
    pub(crate) fn from_decoder(decoder: &Decoder) -> Self {
        Self {
            steps: decoder.steps.clone(),
            count: decoder.count,
            borrow_stored: false,
        }
    }
    pub(crate) fn header(prefix: SharedBytes) -> Self {
        let mut steps = std::array::from_fn(|_| None);
        steps[0] = Some(DecodeStep::Header(prefix));
        Self {
            steps,
            count: 1,
            borrow_stored: false,
        }
    }
    #[cfg(test)]
    pub(crate) fn identity() -> Self {
        Self {
            steps: std::array::from_fn(|_| None),
            count: 0,
            borrow_stored: false,
        }
    }
    #[cfg(test)]
    pub(crate) fn zlib() -> Self {
        let mut steps = std::array::from_fn(|_| None);
        steps[0] = Some(DecodeStep::Zlib);
        Self {
            steps,
            count: 1,
            borrow_stored: false,
        }
    }
    pub(crate) fn borrow_single_stored(mut self, borrow: bool) -> Self {
        self.borrow_stored =
            borrow && self.count == 1 && matches!(self.steps[0], Some(DecodeStep::Zlib));
        self
    }
}
#[derive(Debug)]
enum CopyPhase {
    Reserve,
    Prefix,
    Input,
    Freeze,
    Done,
}
#[derive(Debug)]
pub(crate) struct CopyJob {
    prefix: SharedBytes,
    input: SharedBytes,
    buffer: Option<BudgetVec<u8>>,
    result: Option<SharedBytes>,
    size: usize,
    at: usize,
    phase: CopyPhase,
}
impl CopyJob {
    #[inline(always)]
    pub(crate) fn new(
        prefix: SharedBytes,
        input: SharedBytes,
        limit: usize,
        pool: &Budget,
    ) -> Result<Self> {
        if !prefix.belongs_to(pool) || !input.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let size = prefix
            .len()
            .checked_add(input.len())
            .ok_or(Error::Overflow)?;
        if size > limit {
            return Err(Error::Policy("restored content bytes"));
        }
        Ok(Self {
            prefix,
            input,
            buffer: Some(BudgetVec::new(pool)),
            result: None,
            size,
            at: 0,
            phase: CopyPhase::Reserve,
        })
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        loop {
            cancelled(cancel)?;
            if matches!(self.phase, CopyPhase::Done) {
                return Ok(ParseStep::Event(
                    self.result
                        .as_ref()
                        .required("missing restored result")?
                        .clone(),
                ));
            }
            match self.phase {
                CopyPhase::Reserve => {
                    if self.prefix.is_empty()
                        && self.size <= 8192
                        && work.remaining() >= self.size as u64 + 2
                        && let Some(buffer) = &self.buffer
                        && let Ok(bytes) =
                            triergon_core::Bytes::copy_from_slice(buffer.budget(), &self.input)
                    {
                        // Charge reserve, the copy and publication as in the
                        // resumable path. Each frame keeps its own exact span.
                        work.take(self.size as u64 + 2);
                        self.result = Some(SharedBytes::from_core(bytes));
                        self.buffer = None;
                        self.phase = CopyPhase::Done;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.buffer
                        .as_mut()
                        .required("missing restore buffer")?
                        .try_reserve_exact(self.size)?;
                    self.phase = CopyPhase::Prefix;
                }
                CopyPhase::Prefix | CopyPhase::Input => {
                    let bytes = if matches!(self.phase, CopyPhase::Prefix) {
                        self.prefix.as_ref()
                    } else {
                        self.input.as_ref()
                    };
                    if self.at == bytes.len() {
                        self.at = 0;
                        self.phase = if matches!(self.phase, CopyPhase::Prefix) {
                            CopyPhase::Input
                        } else {
                            CopyPhase::Freeze
                        };
                        continue;
                    }
                    let count = work.take((bytes.len() - self.at).min(8192) as u64) as usize;
                    if count == 0 {
                        return Ok(ParseStep::Yield);
                    }
                    self.buffer
                        .as_mut()
                        .required("missing restore buffer")?
                        .try_extend_from_slice(&bytes[self.at..self.at + count])?;
                    self.at += count;
                }
                CopyPhase::Freeze => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let buffer = self.buffer.take().required("missing restore buffer")?;
                    // Even an empty header restoration keeps the API's
                    // owned empty allocation; pure empty zlib is separate below.
                    match buffer.freeze() {
                        Ok(bytes) => {
                            self.result = Some(SharedBytes::from_core(bytes));
                            self.phase = CopyPhase::Done;
                        }
                        Err(error) => {
                            self.buffer = Some(error.buffer);
                            return Err(error.error);
                        }
                    }
                }
                CopyPhase::Done => unreachable!(),
            }
        }
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
enum StoredPhase {
    Layout,
    Checksum,
    Copy,
    Done,
}
#[derive(Debug)]
struct StoredJob {
    source: SharedBytes,
    pool: Budget,
    limits: InflateLimits,
    range: Range<usize>,
    at: usize,
    a: u32,
    b: u32,
    phase: StoredPhase,
    copy: Option<CopyJob>,
    result: Option<SharedBytes>,
    borrow: bool,
}
impl StoredJob {
    #[inline(always)]
    fn new(source: SharedBytes, limits: InflateLimits, pool: &Budget, borrow: bool) -> Self {
        Self {
            source,
            pool: pool.clone(),
            limits,
            range: 0..0,
            at: 0,
            a: 1,
            b: 0,
            phase: StoredPhase::Layout,
            copy: None,
            result: None,
            borrow,
        }
    }
    fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        loop {
            cancelled(cancel)?;
            match self.phase {
                StoredPhase::Layout => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.source.len() < 11 {
                        return Err(Error::Truncated);
                    }
                    let length = usize::from(u16::from_le_bytes([self.source[3], self.source[4]]));
                    if u16::from_le_bytes([self.source[5], self.source[6]]) != !(length as u16) {
                        return Err(Error::Invalid("stored block complement"));
                    }
                    if self.source.len() != length + 11 {
                        return Err(if self.source.len() < length + 11 {
                            Error::Truncated
                        } else {
                            Error::Invalid("trailing zlib bytes")
                        });
                    }
                    if length
                        > self.limits.output_bytes.min(
                            self.source
                                .len()
                                .saturating_mul(self.limits.expansion_ratio),
                        )
                    {
                        return Err(Error::Policy("inflated output bytes or expansion ratio"));
                    }
                    if length as u64 + 1 > self.limits.work {
                        return Err(Error::Policy("inflate work"));
                    }
                    self.range = 7..7 + length;
                    self.at = 7;
                    self.phase = StoredPhase::Checksum;
                }
                StoredPhase::Checksum => {
                    if self.at == self.range.end {
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        let expected = u32::from_be_bytes(
                            self.source[self.range.end..]
                                .try_into()
                                .map_err(|_| Error::Truncated)?,
                        );
                        if ((self.b << 16) | self.a) != expected {
                            return Err(Error::Invalid("Adler-32 mismatch"));
                        }
                        if self.borrow {
                            self.result = Some(self.source.slice(self.range.clone())?);
                            self.phase = StoredPhase::Done;
                            continue;
                        }
                        if self.range.is_empty() {
                            self.result = Some(SharedBytes::default());
                            self.phase = StoredPhase::Done;
                            continue;
                        }
                        self.copy = Some(CopyJob::new(
                            SharedBytes::default(),
                            self.source.slice(self.range.clone())?,
                            self.range.len(),
                            &self.pool,
                        )?);
                        self.phase = StoredPhase::Copy;
                        continue;
                    }
                    let count = work.take((self.range.end - self.at).min(4096) as u64) as usize;
                    if count == 0 {
                        return Ok(ParseStep::Yield);
                    }
                    let chunk = adler32(&self.source[self.at..self.at + count], cancel)?;
                    let old_a = self.a;
                    self.a = (self.a + (chunk & 0xffff) + 65520) % 65521;
                    self.b = ((u64::from(self.b)
                        + u64::from(chunk >> 16)
                        + count as u64 * ((u64::from(old_a) + 65520) % 65521))
                        % 65521) as u32;
                    self.at += count;
                }
                StoredPhase::Copy => {
                    match self
                        .copy
                        .as_mut()
                        .required("missing stored copy")?
                        .step(work, cancel)?
                    {
                        ParseStep::Event(bytes) => {
                            self.result = Some(bytes);
                            self.copy = None;
                            self.phase = StoredPhase::Done;
                        }
                        other => return Ok(other),
                    }
                }
                StoredPhase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing stored result")?
                            .clone(),
                    ));
                }
            }
        }
    }
}
// Compressed work owns one charged slot. Copy, stored and identity work do
// not carry the inflater's maximum fixed state through their return values.
#[derive(Debug)]
struct InflateSlot {
    jobs: BudgetVec<Inflater>,
}
impl InflateSlot {
    fn new(source: SharedBytes, limits: InflateLimits, pool: &Budget) -> Result<Self> {
        let mut jobs = BudgetVec::new(pool);
        jobs.try_reserve_exact(1)?;
        jobs.try_push(Inflater::new(source, limits, pool)?)
            .map_err(|error| error.error)?;
        Ok(Self { jobs })
    }
    fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        self.jobs
            .as_mut_slice()
            .first_mut()
            .required("missing retained inflater")?
            .step(work, cancel)
    }
}
#[derive(Debug)]
enum Transform {
    Copy(CopyJob),
    Stored(StoredJob),
    Inflate(InflateSlot),
}
impl Transform {
    fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        match self {
            Self::Copy(job) => job.step(work, cancel),
            Self::Stored(job) => job.step(work, cancel),
            Self::Inflate(job) => job.step(work, cancel),
        }
    }
}
#[derive(Debug)]
pub(crate) struct RestoreJob {
    current: SharedBytes,
    plan: RestorePlan,
    at: usize,
    job: Option<Transform>,
    limits: Limits,
    output: usize,
    pool: Budget,
    failed: bool,
}
impl RestoreJob {
    #[inline(always)]
    pub(crate) fn new(
        source: SharedBytes,
        plan: RestorePlan,
        limits: Limits,
        output: usize,
        pool: &Budget,
    ) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if plan.count > plan.steps.len() {
            return Err(Error::Policy("content encoding count"));
        }
        Ok(Self {
            current: source,
            plan,
            at: 0,
            job: None,
            limits,
            output,
            pool: pool.clone(),
            failed: false,
        })
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        if self.failed {
            return Err(Error::Invalid("restore requires replacement after error"));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        loop {
            cancelled(cancel)?;
            if let Some(job) = &mut self.job {
                match job.step(work, cancel)? {
                    ParseStep::Event(bytes) => {
                        self.current = bytes;
                        self.job = None;
                        self.at += 1;
                    }
                    other => return Ok(other),
                }
                continue;
            }
            if self.at == self.plan.count {
                return Ok(ParseStep::Event(self.current.clone()));
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            match self.plan.steps[self.at]
                .as_ref()
                .required("empty decoder step")?
            {
                DecodeStep::Header(prefix) => {
                    self.job = Some(Transform::Copy(CopyJob::new(
                        prefix.clone(),
                        self.current.clone(),
                        self.output,
                        &self.pool,
                    )?));
                }
                DecodeStep::Zlib => {
                    let limits = InflateLimits {
                        output_bytes: self.output,
                        expansion_ratio: self.limits.decompression_ratio,
                        work: self.limits.decompression_work,
                    };
                    header(&self.current)?;
                    if self.current[2] & 7 == 1 {
                        self.job = Some(Transform::Stored(StoredJob::new(
                            self.current.clone(),
                            limits,
                            &self.pool,
                            self.plan.borrow_stored,
                        )));
                    } else {
                        self.job = Some(Transform::Inflate(InflateSlot::new(
                            self.current.clone(),
                            limits,
                            &self.pool,
                        )?));
                    }
                }
            }
        }
    }
}

crate::matroska::typed::usage::storage!(RestorePlan, |s| [PlanHeaders(&s.steps)]);
crate::matroska::typed::usage::storage!(CopyJob, |s| [s.prefix, s.input, s.buffer, s.result]);
crate::matroska::typed::usage::storage!(StoredJob, |s| [s.source, s.copy, s.result]);
crate::matroska::typed::usage::storage!(RestoreJob, |s| [s.current, s.plan, s.job]);
// The allocation itself and every source/output/growth/result owner inside the
// live inflater are separate terms in the existing deduplicated cold proof.
crate::matroska::typed::usage::storage!(InflateSlot, |s| [s.jobs, s.jobs.as_slice().first()]);

struct PlanHeaders<'a>(&'a [Option<DecodeStep>; 8]);
impl crate::matroska::typed::usage::Storage for PlanHeaders<'_> {
    fn storage_slots(&self) -> Result<usize> {
        Ok(8)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        match self.0.get(index).and_then(Option::as_ref) {
            Some(DecodeStep::Header(bytes)) => {
                crate::matroska::typed::usage::Storage::storage_owner(bytes, 0)
            }
            _ => Ok(None),
        }
    }
}
impl crate::matroska::typed::usage::Storage for Transform {
    fn storage_slots(&self) -> Result<usize> {
        match self {
            Self::Copy(job) => crate::matroska::typed::usage::Storage::storage_slots(job),
            Self::Stored(job) => crate::matroska::typed::usage::Storage::storage_slots(job),
            Self::Inflate(job) => crate::matroska::typed::usage::Storage::storage_slots(job),
        }
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        match self {
            Self::Copy(job) => crate::matroska::typed::usage::Storage::storage_owner(job, index),
            Self::Stored(job) => crate::matroska::typed::usage::Storage::storage_owner(job, index),
            Self::Inflate(job) => crate::matroska::typed::usage::Storage::storage_owner(job, index),
        }
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_tests {
    use super::*;
    #[test]
    fn inline_transform_workspace_stays_within_the_observed_host_ceiling() {
        let bytes = size_of::<Transform>();
        println!(
            "Transform={bytes} Inflater={} Stored={} Copy={}",
            size_of::<Inflater>(),
            size_of::<StoredJob>(),
            size_of::<CopyJob>()
        );
        assert!(
            bytes <= 3104,
            "the observed inline transform ceiling must not grow"
        );
    }
}

#[cfg(test)]
mod lazy_workspace_tests;
