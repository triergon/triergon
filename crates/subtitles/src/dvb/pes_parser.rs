//! Stateful codec-packet projection for the observed native DVB parser.
use super::bits::cancelled;
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

// Observed parser accumulation ceiling. The complete source PES is retained
// even when its segment data exceeds this ceiling and passes through.
const MAX_GROUP_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PesFraming {
    Passthrough,
    CompleteSegments,
}

#[derive(Clone, Debug)]
pub struct PesPacket {
    pes: Bytes,
    codec: Bytes,
    framing: PesFraming,
    /// PTS on admission and the separately projected codec timestamp.
    source_pts: Option<i128>,
    codec_pts: Option<i128>,
    /// Previously buffered bytes included in the emitted complete segments.
    prior_tail: Option<Bytes>,
}

impl PesPacket {
    pub fn pes(&self) -> &Bytes {
        &self.pes
    }

    pub fn codec(&self) -> &Bytes {
        &self.codec
    }

    pub fn framing(&self) -> PesFraming {
        self.framing
    }

    pub fn source_pts(&self) -> Option<i128> {
        self.source_pts
    }

    pub fn codec_pts(&self) -> Option<i128> {
        self.codec_pts
    }

    pub fn prior_tail(&self) -> Option<&Bytes> {
        self.prior_tail.as_ref()
    }
}

#[derive(Debug)]
pub enum PesProgress {
    NeedPacket,
    Yield,
    Packet(PesPacket),
}

/// One selected service and clock epoch. The caller recreates or resets the
/// parser on a seek or discontinuity. Within an epoch, a changed known PTS
/// opens a new group. An incomplete segment can continue across PES calls.
#[derive(Debug)]
pub struct PesParser {
    pool: Budget,
    last_pts: Option<i128>,
    active: bool,
    tail: Option<Tail>,
    input: Option<Pending>,
}

#[derive(Debug)]
enum Tail {
    Shared(Bytes),
    Buffered(BudgetVec<u8>),
}

impl Tail {
    fn len(&self) -> usize {
        match self {
            Self::Shared(bytes) => bytes.len(),
            Self::Buffered(bytes) => bytes.len(),
        }
    }
}

#[derive(Debug)]
struct Pending {
    pes: Bytes,
    pts: Option<i128>,
    scan: Option<Scan>,
}

#[derive(Debug)]
struct Scan {
    start: usize,
    prior_len: usize,
    at: usize,
    copy: Option<BudgetVec<u8>>,
    end: Option<ScanEnd>,
}

#[derive(Debug, Clone, Copy)]
struct ScanEnd {
    active: bool,
    complete: usize,
    len: usize,
}

impl PesParser {
    pub fn new(pool: &Budget) -> Self {
        Self {
            pool: pool.clone(),
            last_pts: None,
            active: false,
            tail: None,
            input: None,
        }
    }

    pub fn reset(&mut self) {
        self.last_pts = None;
        self.active = false;
        self.tail = None;
        self.input = None;
    }

    pub fn provide(&mut self, pes: &Bytes, pts: Option<i128>) -> Result<()> {
        if self.input.is_some() || pes.is_empty() {
            return Err(Error::Invalid("DVB PES parser input"));
        }
        if !pes.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        self.input = Some(Pending {
            pes: pes.clone(),
            pts,
            scan: None,
        });
        Ok(())
    }

    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<PesProgress> {
        if let Err(error) = cancelled(cancel) {
            self.reset();
            return Err(error);
        }
        let Some(pending) = self.input.as_mut() else {
            return Ok(PesProgress::NeedPacket);
        };
        if work.remaining() == 0 {
            return Ok(PesProgress::Yield);
        }

        if pending.scan.is_none() {
            let fresh = pending.pts.is_some() && pending.pts != self.last_pts;
            if fresh {
                self.tail = None;
            }
            let new_group = fresh && pending.pes.starts_with(&[0x20, 0]);
            let start = usize::from(new_group) * 2;
            let prior_len = self.tail.as_ref().map_or(0, Tail::len);
            if new_group {
                self.active = true;
            }
            let body_len = pending.pes.len() - start;
            if (fresh && !new_group) || !self.active || body_len > MAX_GROUP_BYTES - prior_len {
                work.take(1);
                return self.passthrough();
            }
            let copy = if matches!(self.tail, Some(Tail::Buffered(_))) {
                match self.tail.take() {
                    Some(Tail::Buffered(buffer)) => Some(buffer),
                    _ => unreachable!("selected buffered DVB tail"),
                }
            } else {
                None
            };
            pending.scan = Some(Scan {
                start,
                prior_len,
                at: 0,
                copy,
                end: None,
            });
        }

        let scan = pending.scan.as_mut().expect("admitted DVB scan");
        if scan.end.is_none() {
            if scan.prior_len != 0 {
                let prior = match &self.tail {
                    Some(Tail::Shared(bytes)) => Some(bytes),
                    _ => None,
                };
                if !scan.collect(&self.pool, prior, &pending.pes[scan.start..], work)? {
                    return Ok(PesProgress::Yield);
                }
            }
            let body = scan
                .copy
                .as_ref()
                .map_or(&pending.pes[scan.start..], BudgetVec::as_slice);
            let mut active = true;
            while scan.at < body.len() {
                if let Err(error) = cancelled(cancel) {
                    self.reset();
                    return Err(error);
                }
                if !work.try_charge(1) {
                    return Ok(PesProgress::Yield);
                }
                let rest = &body[scan.at..];
                if rest[0] != 0x0f {
                    active = false;
                    break;
                }
                let Some(header) = rest.first_chunk::<6>() else {
                    break;
                };
                let size = 6 + usize::from(u16::from_be_bytes([header[4], header[5]]));
                if size > rest.len() {
                    break;
                }
                scan.at += size;
            }
            scan.end = Some(ScanEnd {
                active,
                complete: scan.at,
                len: body.len(),
            });
        }
        let end = scan.end.expect("completed DVB scan");
        let joined = if end.complete != 0 && scan.copy.is_some() {
            if !work.try_charge(1) {
                return Ok(PesProgress::Yield);
            }
            let copy = scan.copy.take().expect("assembled DVB segments");
            match copy.freeze() {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    scan.copy = Some(error.buffer);
                    return Err(error.error);
                }
            }
        } else {
            None
        };
        self.complete(end, joined)
    }

    fn passthrough(&mut self) -> Result<PesProgress> {
        let pending = self
            .input
            .take()
            .ok_or(Error::Invalid("DVB PES parser input absent"))?;
        self.last_pts = pending.pts.or(self.last_pts);
        Ok(PesProgress::Packet(PesPacket {
            codec: pending.pes.clone(),
            pes: pending.pes,
            framing: PesFraming::Passthrough,
            source_pts: pending.pts,
            codec_pts: pending.pts,
            prior_tail: None,
        }))
    }

    fn complete(&mut self, end: ScanEnd, joined: Option<Bytes>) -> Result<PesProgress> {
        let pending = self
            .input
            .take()
            .ok_or(Error::Invalid("DVB PES parser input absent"))?;
        let scan = pending.scan.expect("completed DVB scan");
        let prior = self.tail.take();
        let (codec, framing, prior_tail) = if end.complete != 0 {
            let codec = view(&pending.pes, scan.start, joined.as_ref(), 0..end.complete)?;
            let prior_tail = if scan.prior_len == 0 {
                None
            } else if let Some(Tail::Shared(bytes)) = prior {
                Some(bytes)
            } else {
                Some(codec.slice(0..scan.prior_len)?)
            };
            (codec, PesFraming::CompleteSegments, prior_tail)
        } else {
            (pending.pes.clone(), PesFraming::Passthrough, None)
        };
        self.tail = if end.active && end.complete < end.len {
            if let Some(copy) = scan.copy {
                Some(Tail::Buffered(copy))
            } else {
                Some(Tail::Shared(view(
                    &pending.pes,
                    scan.start,
                    joined.as_ref(),
                    end.complete..end.len,
                )?))
            }
        } else {
            None
        };
        self.active = end.active;
        self.last_pts = pending.pts.or(self.last_pts);
        Ok(PesProgress::Packet(PesPacket {
            pes: pending.pes,
            codec,
            framing,
            source_pts: pending.pts,
            codec_pts: self.last_pts,
            prior_tail,
        }))
    }
}

impl Scan {
    fn collect(
        &mut self,
        pool: &Budget,
        prior: Option<&Bytes>,
        body: &[u8],
        work: &mut WorkBudget,
    ) -> Result<bool> {
        let total = self.prior_len + body.len();
        let copy = self.copy.get_or_insert_with(|| BudgetVec::new(pool));
        if copy.capacity() < total {
            if !work.try_charge(1) {
                return Ok(false);
            }
            // Grow at most logarithmically over a fragmented segment. Each
            // allocation stays within the parser's admitted group ceiling.
            copy.try_reserve_exact(total.next_power_of_two() - copy.len())?;
        }
        let offset = copy.len();
        if offset < self.prior_len {
            let source = prior.ok_or(Error::Invalid("DVB PES shared tail absent"))?;
            let n = work.take((self.prior_len - offset) as u64) as usize;
            copy.try_extend_from_slice(&source[offset..offset + n])?;
            if copy.len() < self.prior_len {
                return Ok(false);
            }
        }
        let offset = copy.len() - self.prior_len;
        let n = work.take((body.len() - offset) as u64) as usize;
        copy.try_extend_from_slice(&body[offset..offset + n])?;
        Ok(copy.len() == total)
    }
}

fn view(pes: &Bytes, start: usize, joined: Option<&Bytes>, range: Range<usize>) -> Result<Bytes> {
    if let Some(joined) = joined {
        joined.slice(range)
    } else {
        pes.slice(start + range.start..start + range.end)
    }
}
