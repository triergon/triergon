//! Original bounded tx3g envelope and Matroska WebVTT addition carriage.
//! Modifier bodies remain original opaque ranges, without style interpretation.
use super::{
    Encoding, Field, FieldOwner, Limits, Modifier,
    emit::Fields,
    unicode::{Cursor, Poll, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

#[derive(Debug)]
pub(crate) struct Tx3g {
    length: [u8; 2],
    length_read: usize,
    at: usize,
    header: [u8; 16],
    header_length: usize,
    iso_boxes: bool,
    header_read: usize,
    modifiers: BudgetVec<Modifier>,
}

impl Tx3g {
    pub(super) fn new(pool: &Budget, iso_boxes: bool) -> Self {
        Self {
            length: [0; 2],
            length_read: 0,
            at: 0,
            header: [0; 16],
            header_length: 8,
            iso_boxes,
            header_read: 0,
            modifiers: BudgetVec::new(pool),
        }
    }

    pub(super) fn text(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Range<usize>>> {
        while self.length_read != 2 {
            cancelled(cancel)?;
            let Some(byte) = bytes.get(self.length_read) else {
                return Err(Error::Truncated);
            };
            if !work.try_charge(1) {
                return Ok(Poll::Yield);
            }
            self.length[self.length_read] = *byte;
            self.length_read += 1;
        }
        let end = 2_usize
            .checked_add(usize::from(u16::from_be_bytes(self.length)))
            .ok_or(Error::Overflow)?;
        if end > bytes.len() {
            return Err(Error::Truncated);
        }
        self.at = end;
        Ok(Poll::Ready(2..end))
    }

    pub(super) fn boxes(
        &mut self,
        bytes: &[u8],
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<()>> {
        loop {
            cancelled(cancel)?;
            if self.at == bytes.len() && self.header_read == 0 {
                return Ok(Poll::Ready(()));
            }
            while self.header_read < self.header_length {
                let offset = self
                    .at
                    .checked_add(self.header_read)
                    .ok_or(Error::Overflow)?;
                let Some(byte) = bytes.get(offset) else {
                    return Err(Error::Truncated);
                };
                if !work.try_charge(1) {
                    return Ok(Poll::Yield);
                }
                self.header[self.header_read] = *byte;
                self.header_read += 1;
            }
            if self.header_length == 8 && self.iso_boxes && self.header[..4] == [0, 0, 0, 1] {
                self.header_length = 16;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(Poll::Yield);
            }
            let size = u32::from_be_bytes([
                self.header[0],
                self.header[1],
                self.header[2],
                self.header[3],
            ]);
            let size = match size {
                0 if self.iso_boxes => bytes.len() - self.at,
                1 if self.iso_boxes => usize::try_from(u64::from_be_bytes(
                    self.header[8..16]
                        .try_into()
                        .map_err(|_| Error::Truncated)?,
                ))
                .map_err(|_| Error::Overflow)?,
                0 => return Err(Error::Unsupported("tx3g box-to-end size")),
                1 => return Err(Error::Unsupported("tx3g extended box size")),
                value => usize::try_from(value).map_err(|_| Error::Overflow)?,
            };
            let uuid = self.iso_boxes && self.header[4..8] == *b"uuid";
            let minimum = self.header_length + if uuid { 16 } else { 0 };
            if size < minimum {
                return Err(Error::Invalid("tx3g modifier box smaller than header"));
            }
            let end = self.at.checked_add(size).ok_or(Error::Overflow)?;
            if end > bytes.len() {
                return Err(Error::Truncated);
            }
            let requested = self.modifiers.len().checked_add(1).ok_or(Error::Overflow)?;
            if requested > limits.max_modifiers {
                return Err(Error::Limit {
                    requested,
                    maximum: limits.max_modifiers,
                });
            }
            let modifier = Modifier {
                kind: [
                    self.header[4],
                    self.header[5],
                    self.header[6],
                    self.header[7],
                ],
                range: self.at..end,
            };
            // On pressure the exact completed header remains pending. A retry
            // never rereads the prefix or increments the modifier count twice.
            self.modifiers
                .try_push(modifier)
                .map_err(|failure| failure.error)?;
            self.at = end;
            self.header_read = 0;
            self.header_length = 8;
            // The complete body is retained by the immutable input owner.
            // Skipping its opaque range is fixed work, not a synthetic byte loop.
        }
    }

    pub(super) fn take_modifiers(&mut self, pool: &Budget) -> BudgetVec<Modifier> {
        std::mem::replace(&mut self.modifiers, BudgetVec::new(pool))
    }
}

#[derive(Debug)]
pub(crate) struct Addition {
    cursor: Cursor,
    first: Option<usize>,
    second: Option<usize>,
    end: usize,
}

impl Addition {
    pub(super) fn new(length: usize) -> Self {
        Self {
            cursor: Cursor::new(0..length, Encoding::Utf8),
            first: None,
            second: None,
            end: length,
        }
    }

    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Fields>> {
        loop {
            cancelled(cancel)?;
            match self.cursor.poll(bytes, work, cancel)? {
                Poll::Yield => return Ok(Poll::Yield),
                Poll::Ready(scalar) => {
                    if scalar.value == '\n' {
                        if self.first.is_none() {
                            self.first = Some(scalar.start);
                        } else if self.second.is_none() {
                            self.second = Some(scalar.start);
                        }
                    }
                }
                Poll::End => {
                    let first = self.first.ok_or(Error::Truncated)?;
                    let second = self.second.ok_or(Error::Truncated)?;
                    return Ok(Poll::Ready(Fields {
                        settings: (first != 0).then_some(Field {
                            owner: FieldOwner::Addition,
                            range: 0..first,
                        }),
                        identifier: (second != first + 1).then(|| Field {
                            owner: FieldOwner::Addition,
                            range: first + 1..second,
                        }),
                        comments: (second + 1 != self.end).then(|| Field {
                            owner: FieldOwner::Addition,
                            range: second + 1..self.end,
                        }),
                    }));
                }
            }
        }
    }
}
