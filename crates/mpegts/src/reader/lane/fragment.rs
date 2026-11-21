//! Bytes no framer accepted and unfinished tails after logical EOF.
use super::*;

pub(super) const LEADING_VIDEO_BYTES: Error = Error::Invalid("Annex B leading bytes");

#[derive(Default)]
pub(super) struct VideoResidueScan {
    piece: usize,
    span: usize,
    offset: usize,
    zeroes: u8,
}

impl VideoResidueScan {
    pub(super) fn step(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(piece) = pieces.get(self.piece) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let Some(span) = piece.source.pes().spans().get(self.span) else {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.piece += 1;
                self.span = 0;
                continue;
            };
            let bytes = span.as_ref();
            if self.offset == bytes.len() {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.span += 1;
                self.offset = 0;
                continue;
            }
            let count = work.take((bytes.len() - self.offset).min(4096) as u64) as usize;
            if count == 0 {
                return Ok(false);
            }
            for byte in &bytes[self.offset..self.offset + count] {
                if *byte == 0 {
                    self.zeroes = (self.zeroes + 1).min(2);
                } else {
                    if *byte == 1 && self.zeroes == 2 {
                        return Err(LEADING_VIDEO_BYTES);
                    }
                    self.zeroes = 0;
                }
            }
            self.offset += count;
        }
        Ok(true)
    }
}

pub(super) enum ResidueScan {
    Video(VideoResidueScan),
    Audio(unframed_audio::Scan),
}

#[derive(Clone, Copy)]
pub(super) enum UnframedKind {
    Video,
    Audio(PrivateAudioCarriage),
}

#[derive(Clone, Copy)]
pub(super) enum FragmentKind {
    Audio,
    Unframed(UnframedKind),
}

impl FragmentKind {
    fn publication_bytes(self) -> Result<usize> {
        match self {
            Self::Audio => Shared::<PacketData>::allocation_bytes(),
            Self::Unframed(_) => Shared::<UnframedData>::allocation_bytes(),
        }
    }
}

// Only validated PES spans enter this copy, after logical EOF. Keeping the
// copy separate makes every allocation and yield retryable.
pub(super) struct Incomplete {
    pub(super) kind: FragmentKind,
    pub(super) range: Range<u64>,
    pub(super) buffer: Option<BudgetVec<u8>>,
    pub(super) bytes: Option<Bytes>,
    pub(super) prepared: bool,
    pub(super) piece: usize,
    pub(super) span: Option<usize>,
    pub(super) span_start: usize,
    pub(super) copied: usize,
    pub(super) covered: usize,
    pub(super) origins: usize,
    pub(super) reader_storage: Option<usize>,
}

impl Incomplete {
    pub(super) fn new(kind: FragmentKind, range: Range<u64>, pool: &Budget) -> Self {
        Self {
            kind,
            range,
            buffer: Some(BudgetVec::new(pool)),
            bytes: None,
            prepared: false,
            piece: 0,
            span: None,
            span_start: 0,
            copied: 0,
            covered: 0,
            origins: 0,
            reader_storage: None,
        }
    }

    fn scan(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(piece) = pieces.get(self.piece) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let pes = piece.source.pes();
            if let Some(index) = self.span {
                if let Some(span) = pes.spans().get(index) {
                    let length = span.as_ref().len();
                    let begin = piece
                        .range
                        .start
                        .checked_add(self.span_start as u64)
                        .ok_or(Error::Overflow)?;
                    let end = begin.checked_add(length as u64).ok_or(Error::Overflow)?;
                    let count = self
                        .range
                        .end
                        .min(end)
                        .saturating_sub(self.range.start.max(begin));
                    self.covered = self
                        .covered
                        .checked_add(usize::try_from(count).map_err(|_| Error::Overflow)?)
                        .ok_or(Error::Overflow)?;
                    self.span_start = self.span_start.checked_add(length).ok_or(Error::Overflow)?;
                    self.span = Some(index + 1);
                } else {
                    self.piece += 1;
                    self.span = None;
                    self.span_start = 0;
                }
            } else {
                if piece.range.start < self.range.end && self.range.start < piece.range.end {
                    self.origins += 1;
                }
                self.span = Some(0);
            }
        }
        Ok(true)
    }

    #[expect(
        clippy::expect_used,
        reason = "typing the fragment copy measured +0.04 to +0.26% more instructions (lane notes)"
    )]
    fn prepare(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.prepared {
            return Ok(true);
        }
        if !self.scan(pieces, work, cancel)? || !work.try_charge(1) {
            return Ok(false);
        }
        let size =
            usize::try_from(self.range.end - self.range.start).map_err(|_| Error::Overflow)?;
        if self.covered != size {
            return Err(Error::Invalid("MPEG-TS fragment source extent"));
        }
        // The EOF audit counted all remaining reader owners, without any
        // caller-only reservations. Add the complete publication peak here.
        let publication = self.kind.publication_bytes()?;
        let minimum = self
            .origins
            .checked_mul(size_of::<PayloadSource>())
            .and_then(|n| n.checked_add(self.reader_storage.expect("audited reader storage")))
            .and_then(|n| n.checked_add(size))
            .and_then(|n| n.checked_add(Bytes::owner_overhead_bytes()))
            .and_then(|n| n.checked_add(publication))
            .ok_or(Error::Overflow)?;
        let buffer = self.buffer.as_mut().expect("incomplete fragment buffer");
        if minimum > buffer.budget().limit() {
            return Err(Error::Limit {
                requested: minimum,
                maximum: buffer.budget().limit(),
            });
        }
        buffer.try_reserve_exact(size)?;
        self.prepared = true;
        self.piece = 0;
        self.span = Some(0);
        self.span_start = 0;
        Ok(true)
    }

    #[expect(
        clippy::expect_used,
        reason = "typing the fragment copy measured +0.04 to +0.26% more instructions (lane notes)"
    )]
    pub(super) fn copy(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.bytes.is_some() {
            return Ok(true);
        }
        if !self.prepare(pieces, work, cancel)? {
            return Ok(false);
        }
        while let Some(piece) = pieces.get(self.piece) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let index = self.span.expect("prepared span cursor");
            let Some(span) = piece.source.pes().spans().get(index) else {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.piece += 1;
                self.span = Some(0);
                self.span_start = 0;
                continue;
            };
            let bytes = span.as_ref();
            let begin = piece
                .range
                .start
                .checked_add(self.span_start as u64)
                .ok_or(Error::Overflow)?;
            let end = begin
                .checked_add(bytes.len() as u64)
                .ok_or(Error::Overflow)?;
            let start = self.range.start.max(begin);
            let length = usize::try_from(self.range.end.min(end).saturating_sub(start))
                .map_err(|_| Error::Overflow)?;
            if self.copied == length {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.span = Some(index + 1);
                self.span_start = self
                    .span_start
                    .checked_add(bytes.len())
                    .ok_or(Error::Overflow)?;
                self.copied = 0;
                continue;
            }
            let count = usize::try_from(work.take((length - self.copied).min(4096) as u64))
                .map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(false);
            }
            let at = usize::try_from(start - begin).map_err(|_| Error::Overflow)? + self.copied;
            self.buffer
                .as_mut()
                .expect("incomplete buffer")
                .try_extend_from_slice(&bytes[at..at + count])?;
            self.copied += count;
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match self.buffer.take().expect("incomplete freeze").freeze() {
            Ok(bytes) => self.bytes = Some(bytes),
            Err(error) => {
                self.buffer = Some(error.value);
                return Err(error.error);
            }
        }
        Ok(true)
    }
}
