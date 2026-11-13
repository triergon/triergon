//! Native receiver configuration, separate from strict source ConfigJob.
use super::{
    Funding, Limits, Options,
    write::{Fixed, Writer},
};
use crate::text::unicode::cancelled;
use std::{ffi::CStr, fmt::Write, ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Style {
    pub font: u16,
    pub face: u8,
    pub size: u8,
    pub color: u32,
    pub alpha: u8,
}
impl Style {
    pub(super) fn read(bytes: &[u8]) -> Self {
        Self {
            font: u16::from_be_bytes([bytes[0], bytes[1]]),
            face: bytes[2] & 7,
            size: bytes[3],
            color: u32::from(bytes[4]) | (u32::from(bytes[5]) << 8) | (u32::from(bytes[6]) << 16),
            alpha: bytes[7],
        }
    }
}
#[derive(Debug)]
pub(super) struct Font {
    pub id: u16,
    pub name: Range<usize>,
}
#[derive(Debug, Clone, Copy)]
enum Phase {
    Fixed,
    Fonts,
    Header,
    Copy,
    Freeze,
    Ready,
}

#[derive(Debug)]
pub(super) struct Configuration {
    pub raw: Option<Bytes>,
    pub style: Style,
    pub fonts: BudgetVec<Font>,
    pub header: Option<Bytes>,
    pub fallback: bool,
    options: Options,
    alignment: u8,
    background: u32,
    default_font: Option<Range<usize>>,
    phase: Phase,
    at: usize,
    count: usize,
    read: usize,
    writer: Writer,
}
impl Configuration {
    pub(super) fn new(
        pool: &Budget,
        raw: Option<&Bytes>,
        options: Options,
        limits: Limits,
    ) -> Result<Self> {
        if let Some(bytes) = raw {
            if !bytes.belongs_to(pool) {
                return Err(Error::ForeignPool);
            }
            if bytes.len() > limits.maximum_config_bytes {
                return Err(Error::Limit {
                    requested: bytes.len(),
                    maximum: limits.maximum_config_bytes,
                });
            }
        }
        Ok(Self {
            raw: raw.cloned(),
            style: Style::default(),
            fonts: BudgetVec::new(pool),
            header: None,
            fallback: false,
            options,
            alignment: 0,
            background: 0,
            default_font: None,
            phase: Phase::Fixed,
            at: 40,
            count: 0,
            read: 0,
            writer: Writer::new(pool),
        })
    }
    /// The index of the first font table entry with font ID `id` or a higher
    /// one: the table is sorted by ID once read, so a style's entries follow
    /// it, in table order.
    pub(super) fn first_font(&self, id: u16) -> usize {
        self.fonts.as_slice().partition_point(|font| font.id < id)
    }
    /// The name of font table entry `index` up to its first NUL.
    pub(super) fn font_name(&self, index: usize) -> Result<&[u8]> {
        let font = self
            .fonts
            .as_slice()
            .get(index)
            .ok_or(Error::Invalid("native font index"))?;
        let raw = self
            .raw
            .as_ref()
            .ok_or(Error::Invalid("native font source"))?;
        let name = raw
            .as_ref()
            .get(font.name.clone())
            .ok_or(Error::Truncated)?;
        Ok(until_nul(name))
    }
    fn fallback(&mut self) {
        self.fallback = true;
        self.fonts.clear();
        self.default_font = None;
        self.phase = Phase::Header;
    }
    pub(super) fn poll(
        &mut self,
        _pool: &Budget,
        limits: Limits,
        funding: &mut Funding,
        caller: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            cancelled(cancel)?;
            match self.phase {
                Phase::Ready => return Ok(true),
                Phase::Fixed => {
                    let bytes = self.raw.as_ref().map_or(&[][..], AsRef::as_ref);
                    if !funding.pay(bytes.len().clamp(1, 40), caller)? {
                        return Ok(false);
                    }
                    if bytes.len() < 40 {
                        funding.commit();
                        self.fallback();
                        continue;
                    }
                    self.style = Style::read(&bytes[22..30]);
                    self.background = (u32::from(255 - bytes[9]) << 24)
                        | u32::from(bytes[6])
                        | (u32::from(bytes[7]) << 8)
                        | (u32::from(bytes[8]) << 16);
                    self.alignment = match (bytes[4] as i8, bytes[5] as i8) {
                        (0, 0) => 7,
                        (1, 0) => 8,
                        (-1, 0) => 9,
                        (0, 1) => 4,
                        (1, 1) => 5,
                        (-1, 1) => 6,
                        (0, -1) => 1,
                        (1, -1) => 2,
                        (-1, -1) => 3,
                        _ => 0,
                    };
                    self.count = usize::from(u16::from_be_bytes([bytes[38], bytes[39]]));
                    if self.count > limits.maximum_fonts {
                        return Err(Error::Limit {
                            requested: self.count,
                            maximum: limits.maximum_fonts,
                        });
                    }
                    let enough = self
                        .count
                        .checked_mul(3)
                        .is_some_and(|n| n <= bytes.len() - 40);
                    funding.commit();
                    if !enough {
                        self.fallback();
                    } else {
                        self.phase = Phase::Fonts;
                    }
                }
                Phase::Fonts => {
                    if self.read == self.count {
                        // Sorted by ID, and by position within one ID, so
                        // a style's entries are found by binary search; a
                        // unit per entry and comparison level.
                        let n = self.fonts.len();
                        let levels = usize::try_from(usize::BITS - n.leading_zeros())
                            .map_err(|_| Error::Overflow)?;
                        if !funding.pay(1 + n * levels, caller)? {
                            return Ok(false);
                        }
                        self.fonts
                            .as_mut_slice()
                            .sort_unstable_by_key(|font| (font.id, font.name.start));
                        funding.commit();
                        self.phase = Phase::Header;
                        continue;
                    }
                    let raw = self
                        .raw
                        .as_ref()
                        .ok_or(Error::Invalid("native configuration owner"))?;
                    let bytes = raw.as_ref();
                    let Some(header) = bytes.get(self.at..self.at.saturating_add(3)) else {
                        self.fallback();
                        continue;
                    };
                    let id = u16::from_be_bytes([header[0], header[1]]);
                    let end = self
                        .at
                        .checked_add(3 + usize::from(header[2]))
                        .ok_or(Error::Overflow)?;
                    if end > bytes.len() {
                        self.fallback();
                        continue;
                    }
                    let moving = if self.fonts.len() == self.fonts.capacity() {
                        self.fonts
                            .len()
                            .checked_mul(size_of::<Font>())
                            .ok_or(Error::Overflow)?
                    } else {
                        0
                    };
                    if !funding.pay(
                        (end - self.at).checked_add(moving).ok_or(Error::Overflow)?,
                        caller,
                    )? {
                        return Ok(false);
                    }
                    let range = self.at + 3..end;
                    self.fonts
                        .try_push(Font {
                            id,
                            name: range.clone(),
                        })
                        .map_err(|e| e.error)?;
                    if id == self.style.font {
                        self.default_font = Some(range);
                    }
                    self.read += 1;
                    self.at = end;
                    funding.commit();
                }
                Phase::Header => {
                    // A fixed bounded header construction is fully funded before
                    // formatting; its later charged byte copies remain resumable.
                    if !funding.pay(2048, caller)? {
                        return Ok(false);
                    }
                    self.writer.pending = self.make_header()?;
                    funding.commit();
                    self.phase = Phase::Copy;
                }
                Phase::Copy => {
                    if !self.writer.drain(2048, funding, caller)? {
                        return Ok(false);
                    }
                    self.phase = Phase::Freeze;
                }
                Phase::Freeze => {
                    if !self.writer.freeze(funding, caller)? {
                        return Ok(false);
                    }
                    self.header = self.writer.frozen.take();
                    self.phase = Phase::Ready;
                }
            }
        }
    }
    fn make_header(&self) -> Result<Fixed<2048>> {
        let mut h = Fixed::new();
        let defaults = self.fallback;
        let width = if defaults || self.options.frame_width == 0 || self.options.frame_height == 0 {
            384
        } else {
            self.options.frame_width
        };
        let height = if defaults || self.options.frame_width == 0 || self.options.frame_height == 0
        {
            288
        } else {
            self.options.frame_height
        };
        let style = if defaults {
            Style {
                font: 0,
                face: 0,
                size: 16,
                color: 0xffffff,
                alpha: 255,
            }
        } else {
            self.style
        };
        let color = (u32::from(255 - style.alpha) << 24) | style.color;
        let background = if defaults { 0 } else { self.background };
        let align = if defaults { 2 } else { self.alignment };
        write!(h,"[Script Info]\n; Script generated by FFmpeg/Lavc63.1.100\nScriptType: v4.00+\nPlayResX: {width}\nPlayResY: {height}\nScaledBorderAndShadow: yes\nYCbCr Matrix: None\n\n[V4+ Styles]\n").map_err(|_|Error::Overflow)?;
        h.append(b"Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,")?;
        if let Some(range) = self.default_font.as_ref().filter(|_| !defaults) {
            let raw = self
                .raw
                .as_ref()
                .ok_or(Error::Invalid("native default font owner"))?;
            let bytes = &raw.as_ref()[range.clone()];
            h.append(until_nul(bytes))?;
        } else {
            h.append(b"Arial")?;
        }
        write!(h,",{},&H{:x},&H{:x},&H{:x},&H{:x},{},{},{},0,100,100,0,0,1,1,0,{},10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",style.size,color,color,background,background,
               -i32::from(style.face&1!=0),-i32::from(style.face&2!=0),-i32::from(style.face&4!=0),align).map_err(|_|Error::Overflow)?;
        Ok(h)
    }
}
/// The bytes before the first NUL, or all of them.
pub(super) fn until_nul(bytes: &[u8]) -> &[u8] {
    CStr::from_bytes_until_nul(bytes).map_or(bytes, CStr::to_bytes)
}
