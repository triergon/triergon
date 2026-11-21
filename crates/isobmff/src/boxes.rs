//! Box framing. [`BoxHeader`] reads one box header, [`BoxView`] holds one complete box in
//! memory and walks its children with [`BoxChildren`], and [`BoxReader`] walks the root
//! boxes of a source through the caller's input cursor. A `uuid` box is framed like any
//! other; its extended type stays at the start of its payload.
//!
//! Specification: Adobe F4V 10.1 §1.3; Apple QuickTime File Format (atoms).
use crate::{Limits, Next, fields, policy};
use std::ops::Range;
use triergon_core::{Bytes, Error, Result, WorkBudget};

mod reader;
pub use reader::{BoxEvent, BoxReader, MediaDataPolicy};

/// The header of one box: its type, where it starts and how long it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    /// The four-character box type.
    pub kind: [u8; 4],
    /// Absolute file offset of the box's first byte.
    pub start: u64,
    /// Total size in bytes, header included; `None` for a size-zero box whose end is not
    /// known yet.
    pub size: Option<u64>,
    /// Header length in bytes: 8, or 16 with a 64-bit size.
    pub header_size: u8,
}
/// The result of [`BoxHeader::parse`] on a prefix that may be too short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRead {
    /// The prefix must hold at least this many bytes (8 or 16).
    Need(usize),
    /// The header is complete.
    Ready(BoxHeader),
}

impl BoxHeader {
    /// Reads the header at absolute offset `start` from `prefix`, the bytes from there on.
    /// `parent_end` is the end of the enclosing box or source, if known; a box crossing it
    /// is `Error::Truncated`. A size-zero box runs to `parent_end` and is accepted only as
    /// a `top_level` `mdat`; elsewhere it is `Error::Unsupported`.
    pub fn parse(
        prefix: &[u8],
        start: u64,
        parent_end: Option<u64>,
        top_level: bool,
    ) -> Result<HeaderRead> {
        let remaining = parent_end
            .map(|end| {
                end.checked_sub(start)
                    .ok_or(Error::Invalid("box starts beyond parent"))
            })
            .transpose()?;
        if remaining.is_some_and(|v| v < 8) {
            return Err(Error::Truncated);
        }
        if prefix.len() < 8 {
            return Ok(HeaderRead::Need(8));
        }
        let small = fields::u32_at(prefix, 0)?;
        let kind = fields::fourcc(prefix, 4)?;
        let header_size = if small == 1 { 16 } else { 8 };
        if remaining.is_some_and(|v| v < u64::from(header_size)) {
            return Err(Error::Truncated);
        }
        if prefix.len() < usize::from(header_size) {
            return Ok(HeaderRead::Need(usize::from(header_size)));
        }
        let size = match small {
            0 => {
                if !top_level || kind != *b"mdat" {
                    return Err(Error::Unsupported(
                        "zero-size box outside terminal top-level mdat profile",
                    ));
                }
                remaining
            }
            1 => Some(fields::u64_at(prefix, 8)?),
            n => Some(u64::from(n)),
        };
        start
            .checked_add(u64::from(header_size))
            .ok_or(Error::Overflow)?;
        if let Some(size) = size {
            if size < u64::from(header_size) {
                return Err(Error::Invalid("box shorter than header"));
            }
            let end = start.checked_add(size).ok_or(Error::Overflow)?;
            if parent_end.is_some_and(|bound| end > bound) {
                return Err(Error::Truncated);
            }
        }
        Ok(HeaderRead::Ready(Self {
            kind,
            start,
            size,
            header_size,
        }))
    }
    /// Absolute end offset, exclusive; `None` while the size is unknown.
    pub fn end(&self) -> Result<Option<u64>> {
        self.size
            .map(|size| self.start.checked_add(size).ok_or(Error::Overflow))
            .transpose()
    }
    /// Absolute byte range of the payload after the header; `Error::Unsupported` while
    /// the end is unknown.
    pub fn payload_range(&self) -> Result<Range<u64>> {
        let start = self
            .start
            .checked_add(u64::from(self.header_size))
            .ok_or(Error::Overflow)?;
        let end = self
            .end()?
            .ok_or(Error::Unsupported("source final endpoint is not known"))?;
        Ok(start..end)
    }
}

/// One complete box held in memory charged to the shared budget, with its parsed header.
/// Media data is never held this way: an `mdat` is known only by its [`BoxHeader`].
#[derive(Clone, Debug)]
pub struct BoxView {
    bytes: Bytes,
    header: BoxHeader,
}
impl BoxView {
    /// The box stored in `bytes`, which starts at absolute offset `start`; fails unless
    /// `bytes` hold exactly one complete box.
    pub fn new(bytes: Bytes, start: u64) -> Result<Self> {
        let end = start
            .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let HeaderRead::Ready(header) = BoxHeader::parse(bytes.as_ref(), start, Some(end), true)?
        else {
            return Err(Error::Truncated);
        };
        Self::checked(bytes, header)
    }
    fn checked(bytes: Bytes, header: BoxHeader) -> Result<Self> {
        if header.size != Some(u64::try_from(bytes.len()).map_err(|_| Error::Overflow)?) {
            return Err(Error::Invalid("box view must match one complete box"));
        }
        Ok(Self { bytes, header })
    }
    /// The whole box, header included.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// The parsed header.
    pub fn header(&self) -> &BoxHeader {
        &self.header
    }
    /// The bytes after the header.
    pub fn payload(&self) -> &[u8] {
        &self.bytes[usize::from(self.header.header_size)..]
    }
    /// Walks the child boxes that follow the header and `skip` more bytes (the fixed
    /// fields of a full box or sample entry), up to `limits.max_boxes` of them.
    /// `Error::Truncated` when `skip` passes the end of the box.
    pub fn children(&self, skip: usize, limits: Limits) -> Result<BoxChildren> {
        let position = usize::from(self.header.header_size)
            .checked_add(skip)
            .ok_or(Error::Overflow)?;
        if position > self.bytes.len() {
            return Err(Error::Truncated);
        }
        Ok(BoxChildren {
            parent: self.clone(),
            position,
            count: 0,
            limits,
        })
    }
}

/// A walk over the child boxes of one [`BoxView`], from [`BoxView::children`]. Each
/// child is a [`BoxView`] sharing the parent's storage.
#[derive(Debug)]
pub struct BoxChildren {
    parent: BoxView,
    position: usize,
    count: u32,
    limits: Limits,
}
impl BoxChildren {
    /// The next child, for one work unit. `Error::Limit` past `max_boxes` children and
    /// `Error::Truncated` for a child crossing the parent's end; a failed poll leaves the
    /// walk where it was.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<Next<BoxView>> {
        if self.position == self.parent.bytes.len() {
            return Ok(Next::End);
        }
        if !work.try_charge(1) {
            return Ok(Next::Yield);
        }
        let next_count = self.count.checked_add(1).ok_or(Error::Overflow)?;
        policy(next_count, self.limits.max_boxes)?;
        let start = self
            .parent
            .header
            .start
            .checked_add(u64::try_from(self.position).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let HeaderRead::Ready(header) = BoxHeader::parse(
            &self.parent.bytes[self.position..],
            start,
            self.parent.header.end()?,
            false,
        )?
        else {
            return Err(Error::Truncated);
        };
        let size =
            usize::try_from(header.size.ok_or(Error::Truncated)?).map_err(|_| Error::Overflow)?;
        let end = self.position.checked_add(size).ok_or(Error::Overflow)?;
        let view = BoxView::checked(self.parent.bytes.slice(self.position..end)?, header)?;
        self.position = end;
        self.count = next_count;
        Ok(Next::Item(view))
    }
}
