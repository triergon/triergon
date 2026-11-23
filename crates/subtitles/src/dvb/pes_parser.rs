//! DVB PES data fields cut into the codec packets FFmpeg's demuxer hands its
//! `dvbsub` decoder, as observed.
use crate::cancelled;
use crate::data_field::DataField;
use std::ops::Range;
use std::sync::atomic::AtomicBool;
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
    /// `None` until the first step looks at the field.
    scan: Option<Scan>,
}

/// A field scanned for complete segments.
#[derive(Debug)]
struct Scan {
    /// Where the body starts: after the two bytes that open a group.
    start: usize,
    /// The part of a segment earlier fields left, which starts the body.
    join: Option<Join>,
    at: usize,
    end: Option<ScanEnd>,
}

/// The part of a segment earlier fields left, copied with the body to be
/// scanned as one.
#[derive(Debug)]
struct Join {
    prior_len: usize,
    /// The part as an earlier field holds it, to be copied first; `None` when
    /// `copy` starts with it.
    shared: Option<Bytes>,
    copy: BudgetVec<u8>,
}

/// A join's copy once its segments are known to be complete.
#[derive(Debug)]
struct Joined {
    bytes: Bytes,
    prior_len: usize,
    shared: Option<Bytes>,
}

#[derive(Debug, Clone, Copy)]
struct ScanEnd {
    active: bool,
    complete: usize,
    len: usize,
}

/// How far a step took the pending field.
#[derive(Debug)]
enum Cut {
    Yield,
    Done {
        end: ScanEnd,
        joined: Option<Joined>,
    },
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
    // Out of line, so that a demuxer's per-PES loop, which the other streams'
    // packets run through, does not carry the step's registers and spills.
    #[inline(never)]
    pub fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<PesProgress> {
        if let Err(error) = cancelled(cancel) {
            self.reset();
            return Err(error);
        }
        // The field is held here while it is cut, and kept again when the
        // step stops short of a packet.
        let Some(Pending { pes, pts, scan }) = self.input.take() else {
            return Ok(PesProgress::NeedPacket);
        };
        let mut scan = match scan {
            _ if work.remaining() == 0 => {
                self.input = Some(Pending { pes, pts, scan });
                return Ok(PesProgress::Yield);
            }
            Some(scan) => scan,
            None => match self.open(&pes, pts) {
                Some(scan) => scan,
                None => {
                    work.take(1);
                    return Ok(self.passthrough(pes, pts));
                }
            },
        };
        match scan.cut(&pes, work, cancel) {
            Ok(Cut::Done { end, joined }) => self.complete(pes, pts, scan, end, joined),
            Err(Error::Cancelled) => {
                self.reset();
                Err(Error::Cancelled)
            }
            // A yield, or a pool error: the field stays pending.
            kept => {
                self.input = Some(Pending {
                    pes,
                    pts,
                    scan: Some(scan),
                });
                kept.map(|_| PesProgress::Yield)
            }
        }
    }

    /// Where a field the parser takes up starts its scan, or `None` when it
    /// passes through: a new PTS that does not open a group, a field outside
    /// a group, or a group past the ceiling. A new PTS drops the bytes earlier
    /// fields left.
    fn open(&mut self, pes: &Bytes, pts: Option<i128>) -> Option<Scan> {
        let fresh = pts.is_some() && pts != self.last_pts;
        if fresh {
            self.tail = None;
        }
        let new_group = fresh && DataField::new(pes).is_some_and(|field| field.is_subtitling());
        let start = usize::from(new_group) * 2;
        let prior_len = self.tail.as_ref().map_or(0, Tail::len);
        if new_group {
            self.active = true;
        }
        let body_len = pes.len() - start;
        if (fresh && !new_group) || !self.active || body_len > MAX_GROUP_BYTES - prior_len {
            return None;
        }
        let join = self.tail.take().map(|tail| match tail {
            Tail::Shared(bytes) => Join {
                prior_len,
                shared: Some(bytes),
                copy: BudgetVec::new(&self.pool),
            },
            Tail::Buffered(copy) => Join {
                prior_len,
                shared: None,
                copy,
            },
        });
        Some(Scan {
            start,
            join,
            at: 0,
            end: None,
        })
    }

    fn passthrough(&mut self, pes: Bytes, pts: Option<i128>) -> PesProgress {
        self.last_pts = pts.or(self.last_pts);
        PesProgress::Packet(PesPacket {
            codec: pes.clone(),
            pes,
            framing: PesFraming::Passthrough,
            source_pts: pts,
            codec_pts: pts,
            prior_tail: None,
        })
    }

    fn complete(
        &mut self,
        pes: Bytes,
        pts: Option<i128>,
        scan: Scan,
        end: ScanEnd,
        mut joined: Option<Joined>,
    ) -> Result<PesProgress> {
        let (codec, framing, prior_tail) = if end.complete != 0 {
            let codec = view(&pes, scan.start, joined.as_ref(), 0..end.complete)?;
            let prior_tail = match &mut joined {
                None => None,
                Some(joined) => match joined.shared.take() {
                    Some(bytes) => Some(bytes),
                    None => Some(codec.slice(0..joined.prior_len)?),
                },
            };
            (codec, PesFraming::CompleteSegments, prior_tail)
        } else {
            (pes.clone(), PesFraming::Passthrough, None)
        };
        self.tail = if end.active && end.complete < end.len {
            Some(match scan.join {
                Some(join) => Tail::Buffered(join.copy),
                None => Tail::Shared(view(
                    &pes,
                    scan.start,
                    joined.as_ref(),
                    end.complete..end.len,
                )?),
            })
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
    /// Joins the field to the part earlier fields left, scans the segments
    /// and freezes the join once they are complete, each as `work` allows.
    fn cut(&mut self, pes: &Bytes, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Cut> {
        let end = if let Some(end) = self.end {
            end
        } else {
            let field = &pes[self.start..];
            let body = match &mut self.join {
                Some(join) => {
                    if !join.collect(field, work)? {
                        return Ok(Cut::Yield);
                    }
                    join.copy.as_slice()
                }
                None => field,
            };
            let mut active = true;
            while self.at < body.len() {
                cancelled(cancel)?;
                if !work.try_charge(1) {
                    return Ok(Cut::Yield);
                }
                let rest = &body[self.at..];
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
                self.at += size;
            }
            *self.end.insert(ScanEnd {
                active,
                complete: self.at,
                len: body.len(),
            })
        };
        let joined = match self.join.take_if(|_| end.complete != 0) {
            Some(join) => {
                if !work.try_charge(1) {
                    self.join = Some(join);
                    return Ok(Cut::Yield);
                }
                let Join {
                    prior_len,
                    shared,
                    copy,
                } = join;
                match copy.freeze() {
                    Ok(bytes) => Some(Joined {
                        bytes,
                        prior_len,
                        shared,
                    }),
                    Err(error) => {
                        self.join = Some(Join {
                            prior_len,
                            shared,
                            copy: error.value,
                        });
                        return Err(error.error);
                    }
                }
            }
            None => None,
        };
        Ok(Cut::Done { end, joined })
    }
}

impl Join {
    /// Copies the part earlier fields left, then `body`, as `work` allows;
    /// `true` once all of it is in.
    fn collect(&mut self, body: &[u8], work: &mut WorkBudget) -> Result<bool> {
        let total = self.prior_len + body.len();
        let copy = &mut self.copy;
        if copy.capacity() < total {
            if !work.try_charge(1) {
                return Ok(false);
            }
            // Grow at most logarithmically over a fragmented segment. Each
            // allocation stays within the parser's admitted group ceiling.
            copy.try_reserve_exact(total.next_power_of_two() - copy.len())?;
        }
        if let Some(shared) = &self.shared {
            let offset = copy.len();
            if offset < self.prior_len {
                let n = work.take((self.prior_len - offset) as u64) as usize;
                copy.try_extend_from_slice(&shared[offset..offset + n])?;
                if copy.len() < self.prior_len {
                    return Ok(false);
                }
            }
        }
        let offset = copy.len() - self.prior_len;
        let n = work.take((body.len() - offset) as u64) as usize;
        copy.try_extend_from_slice(&body[offset..offset + n])?;
        Ok(copy.len() == total)
    }
}

/// `range` of the segments' bytes: the frozen join, else the field's body
/// from `start`.
fn view(pes: &Bytes, start: usize, joined: Option<&Joined>, range: Range<usize>) -> Result<Bytes> {
    match joined {
        Some(joined) => joined.bytes.slice(range),
        None => pes.slice(start + range.start..start + range.end),
    }
}
