//! The tx3g sample envelope (text length, text and modifier boxes) and the
//! fields of a Matroska WebVTT block addition. Modifier boxes are kept as byte
//! ranges; nothing in them is interpreted.
use super::{
    Encoding, Field, FieldOwner, Limits, Modifier,
    emit::Fields,
    unicode::{Cursor, Poll},
};
use crate::cancelled;
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// The tx3g text length: two big-endian bytes before the text.
#[derive(Debug, Default)]
pub(crate) struct Length {
    bytes: [u8; 2],
    read: usize,
}

impl Length {
    /// The text's byte range, after its length; `None` when `work` runs out
    /// first. One unit per length byte.
    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Range<usize>>> {
        while self.read != 2 {
            cancelled(cancel)?;
            let Some(byte) = bytes.get(self.read) else {
                return Err(Error::Truncated);
            };
            if !work.try_charge(1) {
                return Ok(None);
            }
            self.bytes[self.read] = *byte;
            self.read += 1;
        }
        let end = 2_usize
            .checked_add(usize::from(u16::from_be_bytes(self.bytes)))
            .ok_or(Error::Overflow)?;
        if end > bytes.len() {
            return Err(Error::Truncated);
        }
        Ok(Some(2..end))
    }
}

/// The tx3g modifier boxes after the text.
#[derive(Debug)]
pub(crate) struct Boxes {
    at: usize,
    header: [u8; 16],
    header_length: usize,
    iso_boxes: bool,
    header_read: usize,
    modifiers: BudgetVec<Modifier>,
}

impl Boxes {
    /// The boxes from `at`, the end of the text; with `iso_boxes` also in
    /// their ISO forms.
    pub(super) fn new(pool: &Budget, iso_boxes: bool, at: usize) -> Self {
        Self {
            at,
            header: [0; 16],
            header_length: 8,
            iso_boxes,
            header_read: 0,
            modifiers: BudgetVec::new(pool),
        }
    }

    /// Reads the boxes up to the end of the sample; `false` when `work` runs
    /// out first. One unit per header byte and one per box.
    pub(super) fn poll(
        &mut self,
        bytes: &[u8],
        limits: Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            cancelled(cancel)?;
            if self.at == bytes.len() && self.header_read == 0 {
                return Ok(true);
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
                    return Ok(false);
                }
                self.header[self.header_read] = *byte;
                self.header_read += 1;
            }
            if self.header_length == 8 && self.iso_boxes && self.header[..4] == [0, 0, 0, 1] {
                self.header_length = 16;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(false);
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

    /// The boxes read, leaving none.
    pub(super) fn take_modifiers(&mut self, pool: &Budget) -> BudgetVec<Modifier> {
        std::mem::replace(&mut self.modifiers, BudgetVec::new(pool))
    }
}

/// The fields of a WebVTT block addition: settings, identifier and comments,
/// each ended by a line feed but the last.
#[derive(Debug)]
pub(crate) struct Addition {
    bytes: Bytes,
    cursor: Cursor,
    first: Option<usize>,
    second: Option<usize>,
}

impl Addition {
    pub(super) fn new(bytes: &Bytes) -> Self {
        Self {
            bytes: bytes.clone(),
            cursor: Cursor::new(0..bytes.len(), Encoding::Utf8),
            first: None,
            second: None,
        }
    }

    /// The fields; `None` when `work` runs out first. The addition must be
    /// UTF-8 with at least two line feeds.
    pub(super) fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Fields>> {
        let end = self.bytes.len();
        loop {
            cancelled(cancel)?;
            match self.cursor.poll(self.bytes.as_ref(), work, cancel)? {
                Poll::Yield => return Ok(None),
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
                    return Ok(Some(Fields {
                        settings: (first != 0).then_some(Field {
                            owner: FieldOwner::Addition,
                            range: 0..first,
                        }),
                        identifier: (second != first + 1).then(|| Field {
                            owner: FieldOwner::Addition,
                            range: first + 1..second,
                        }),
                        comments: (second + 1 != end).then(|| Field {
                            owner: FieldOwner::Addition,
                            range: second + 1..end,
                        }),
                    }));
                }
            }
        }
    }
}
