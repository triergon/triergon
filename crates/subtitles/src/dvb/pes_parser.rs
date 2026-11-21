//! DVB PES data fields cut into the codec packets FFmpeg's demuxer hands its
//! `dvbsub` decoder, as observed.
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

// Observed parser accumulation ceiling. The complete source PES is retained
// even when its segment data exceeds this ceiling and passes through.
const MAX_GROUP_BYTES: usize = 65_536;

/// How a codec packet was cut from its PES data field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PesFraming {
    /// The codec packet is the whole field, unchanged.
    Passthrough,
    /// The codec packet is the complete segments the field holds or
    /// completes, without the two bytes that open a group.
    CompleteSegments,
}

/// One codec packet cut from a PES data field.
#[derive(Clone, Debug)]
pub struct PesPacket {
    pes: Bytes,
    codec: Bytes,
    framing: PesFraming,
    /// PTS on admission and the separately projected codec timestamp.
    source_pts: Option<i128>,
    codec_pts: Option<i128>,
    /// Bytes buffered from earlier fields that start the emitted complete
    /// segments.
    prior_tail: Option<Bytes>,
}

impl PesPacket {
    /// The PES data field as provided.
    pub fn pes(&self) -> &Bytes {
        &self.pes
    }

    /// The codec packet for the decoder. It shares storage with the field
    /// unless it joins a segment split across fields.
    pub fn codec(&self) -> &Bytes {
        &self.codec
    }

    /// How [`codec`](Self::codec) was cut.
    pub fn framing(&self) -> PesFraming {
        self.framing
    }

    /// The PTS provided with the field, in the caller's ticks.
    pub fn source_pts(&self) -> Option<i128> {
        self.source_pts
    }

    /// The PTS handed to the decoder: the field's own or, for a field without
    /// one that the parser scans as part of a group, the last known PTS.
    pub fn codec_pts(&self) -> Option<i128> {
        self.codec_pts
    }

    /// The part of a split segment earlier fields left, which starts
    /// [`codec`](Self::codec); `None` when there was none.
    pub fn prior_tail(&self) -> Option<&Bytes> {
        self.prior_tail.as_ref()
    }
}

/// What a [`PesParser::step`] call did.
#[derive(Debug)]
pub enum PesProgress {
    /// No field is pending; [`provide`](PesParser::provide) the next one.
    NeedPacket,
    /// The work budget ran out; step again.
    Yield,
    /// The codec packet cut from the pending field, which is consumed.
    Packet(PesPacket),
}

/// Cuts the PES data fields of one selected service and clock epoch into
/// codec packets. The caller recreates or resets the parser on a seek or
/// discontinuity. Within an epoch, a changed known PTS opens a new group. An
/// incomplete segment can continue across PES calls.
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
    /// A parser that charges the segments it joins to `pool`.
    pub fn new(pool: &Budget) -> Self {
        Self {
            pool: pool.clone(),
            last_pts: None,
            active: false,
            tail: None,
            input: None,
        }
    }

    /// Drops the pending field, the buffered part of a segment and the group,
    /// as after a seek.
    pub fn reset(&mut self) {
        self.last_pts = None;
        self.active = false;
        self.tail = None;
        self.input = None;
    }

    /// Hands over the next PES data field with its PTS in the caller's ticks.
    /// `Error::Invalid` while a field is pending or for an empty one,
    /// `Error::ForeignPool` for bytes of another pool.
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

    /// Cuts the pending field, at one work unit per segment scanned and per
    /// byte copied to join a split segment. `Error::Cancelled` when `cancel`
    /// is set, which also [`reset`](Self::reset)s the parser; a pool error
    /// keeps the field pending for the next step.
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

        let scan = match &mut pending.scan {
            Some(scan) => scan,
            slot @ None => {
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
                let copy = match self.tail.take() {
                    Some(Tail::Buffered(buffer)) => Some(buffer),
                    shared => {
                        self.tail = shared;
                        None
                    }
                };
                slot.insert(Scan {
                    start,
                    prior_len,
                    at: 0,
                    copy,
                    end: None,
                })
            }
        };

        let end = if let Some(end) = scan.end {
            end
        } else {
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
            *scan.end.insert(ScanEnd {
                active,
                complete: scan.at,
                len: body.len(),
            })
        };
        let joined = match scan.copy.take_if(|_| end.complete != 0) {
            Some(copy) => {
                if !work.try_charge(1) {
                    scan.copy = Some(copy);
                    return Ok(PesProgress::Yield);
                }
                match copy.freeze() {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        scan.copy = Some(error.value);
                        return Err(error.error);
                    }
                }
            }
            None => None,
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
        let Some(Pending {
            pes,
            pts,
            scan: Some(scan),
        }) = self.input.take()
        else {
            return Err(Error::Invalid("DVB PES parser input absent"));
        };
        let prior = self.tail.take();
        let (codec, framing, prior_tail) = if end.complete != 0 {
            let codec = view(&pes, scan.start, joined.as_ref(), 0..end.complete)?;
            let prior_tail = if scan.prior_len == 0 {
                None
            } else if let Some(Tail::Shared(bytes)) = prior {
                Some(bytes)
            } else {
                Some(codec.slice(0..scan.prior_len)?)
            };
            (codec, PesFraming::CompleteSegments, prior_tail)
        } else {
            (pes.clone(), PesFraming::Passthrough, None)
        };
        self.tail = if end.active && end.complete < end.len {
            if let Some(copy) = scan.copy {
                Some(Tail::Buffered(copy))
            } else {
                Some(Tail::Shared(view(
                    &pes,
                    scan.start,
                    joined.as_ref(),
                    end.complete..end.len,
                )?))
            }
        } else {
            None
        };
        self.active = end.active;
        self.last_pts = pts.or(self.last_pts);
        Ok(PesProgress::Packet(PesPacket {
            pes,
            codec,
            framing,
            source_pts: pts,
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

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
