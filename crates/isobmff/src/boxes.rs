//! Adobe F4V 10.1 §1.3 / Apple QTFF atom bounds. UUID syntax stays opaque (G01).
use crate::{Limits, fields, policy};
use std::ops::Range;
use triergon_core::{Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    pub start: u64,
    pub size: Option<u64>,
    pub header_size: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRead {
    Need(usize),
    Ready(BoxHeader),
}

impl BoxHeader {
    /// Only inspects a bounded prefix. Unknown final length stays unknown. A zero
    /// size is admitted only for a top-level terminal mdat in this selected seam.
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
    pub fn end(&self) -> Result<Option<u64>> {
        self.size
            .map(|size| self.start.checked_add(size).ok_or(Error::Overflow))
            .transpose()
    }
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

/// An immutable, core-charged, complete bounded metadata box. Large mdat headers
/// use BoxHeader directly; this type never buffers a file or fetches a range.
#[derive(Clone, Debug)]
pub struct BoxView {
    bytes: Bytes,
    header: BoxHeader,
}
impl BoxView {
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
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn header(&self) -> &BoxHeader {
        &self.header
    }
    pub fn payload(&self) -> &[u8] {
        &self.bytes[usize::from(self.header.header_size)..]
    }
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

#[derive(Debug)]
pub enum BoxStep {
    Box(BoxView),
    Yield,
    End,
}
#[derive(Debug)]
pub struct BoxChildren {
    parent: BoxView,
    position: usize,
    count: u32,
    limits: Limits,
}
impl BoxChildren {
    /// One unit bounds one header/slice operation. Position changes only on success.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<BoxStep> {
        if self.position == self.parent.bytes.len() {
            return Ok(BoxStep::End);
        }
        if !work.try_charge(1) {
            return Ok(BoxStep::Yield);
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
        Ok(BoxStep::Box(view))
    }
}
