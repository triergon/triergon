//! Parser for a tx3g sample description (the ETSI TS 126 245 text sample
//! entry): display defaults, default style, font table and child boxes. The
//! caller names the layout; nothing is inferred and no inherited style is
//! guessed.
use super::{
    Encoding, Modifier,
    unicode::{Cursor, Poll, Probe, cancelled},
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// How the description bytes are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigLayout {
    /// The sample entry body after its box header: 6 reserved zero bytes and
    /// the data reference index, then the defaults record and child boxes.
    SampleEntryBody,
    /// The 30-byte defaults record, from the display flags on, then child
    /// boxes.
    DefaultsRecord,
}
/// Resource bounds of one [`ConfigJob`].
#[derive(Clone, Copy, Debug)]
pub struct ConfigLimits {
    /// Description bytes; more is `Error::Limit` from [`ConfigJob::new`].
    /// Default 1 MiB.
    pub maximum_bytes: usize,
    /// `ftab` entries; more is `Error::Limit`. Default 4096.
    pub maximum_fonts: usize,
    /// Child boxes, `ftab` included; more is `Error::Limit`. Default 4096.
    pub maximum_boxes: usize,
}
impl Default for ConfigLimits {
    fn default() -> Self {
        Self {
            maximum_bytes: 1024 * 1024,
            maximum_fonts: 4096,
            maximum_boxes: 4096,
        }
    }
}
/// A style record: the style of a character range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleRecord {
    /// Start offset in characters.
    pub start: u16,
    /// Exclusive end offset in characters.
    pub end: u16,
    /// Font identifier, an entry of the `ftab`.
    pub font_id: u16,
    /// Face style flags: bit 0 bold, bit 1 italic, bit 2 underline.
    pub face_flags: u8,
    /// Font size in pixels.
    pub font_size: u8,
    /// Text colour as red, green, blue and alpha bytes.
    pub rgba: [u8; 4],
}
/// The fixed fields of the sample description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Defaults {
    /// The sample entry's data reference index; `None` for
    /// [`ConfigLayout::DefaultsRecord`].
    pub data_reference_index: Option<u16>,
    /// Display flags as written.
    pub display_flags: u32,
    /// Horizontal justification, −1 to 1: 0 left, 1 centred, −1 right.
    pub horizontal: i8,
    /// Vertical justification, −1 to 1: 0 top, 1 centred, −1 bottom.
    pub vertical: i8,
    /// Background colour as red, green, blue and alpha bytes.
    pub background: [u8; 4],
    /// Default text box: top, left, bottom and right as written.
    pub text_box: [i16; 4],
    /// Default style; its character range is always `0..0`.
    pub style: StyleRecord,
}
/// One `ftab` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontRecord {
    /// Font identifier.
    pub id: u16,
    /// Byte range of the font name in `Configuration::raw`.
    pub name_range: Range<usize>,
}
/// A parsed sample description.
#[derive(Debug)]
pub struct Configuration {
    /// The description bytes; every range points into them.
    pub raw: Bytes,
    /// The layout the bytes were read in.
    pub layout: ConfigLayout,
    /// The fixed fields.
    pub defaults: Defaults,
    /// `ftab` entries in order.
    pub fonts: BudgetVec<FontRecord>,
    /// Child boxes after the fixed fields in order, `ftab` and unknown boxes
    /// included, each with its whole byte range in `raw`.
    pub boxes: BudgetVec<Modifier>,
}
/// What a [`ConfigJob::poll`] reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigStatus {
    /// The work budget ran out; poll again.
    Pending,
    /// The description is parsed; take it with [`ConfigJob::take_output`].
    Ready,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Fixed,
    Header,
    BoxCommit,
    FontCount,
    FontHeader,
    FontName,
    FontCommit,
    Ready,
    Taken,
}

/// Parses one tx3g sample description in a layout the caller names.
#[derive(Debug)]
pub struct ConfigJob {
    raw: Bytes,
    layout: ConfigLayout,
    budget: Budget,
    limits: ConfigLimits,
    fixed: [u8; 38],
    fixed_read: usize,
    defaults: Option<Defaults>,
    at: usize,
    header: [u8; 16],
    header_read: usize,
    header_size: usize,
    box_start: usize,
    box_end: usize,
    boxes: BudgetVec<Modifier>,
    fonts: BudgetVec<FontRecord>,
    ftab_seen: bool,
    font_count: usize,
    font_index: usize,
    font_header: [u8; 3],
    font_read: usize,
    font_pending: Option<FontRecord>,
    name_cursor: Option<Cursor>,
    name_probe: Option<Probe>,
    unicode_font_names: bool,
    paid: bool,
    phase: Phase,
    failed: Option<Error>,
}
impl ConfigJob {
    /// Like [`Self::new`], but a font name may also be UTF-16BE after a FEFF
    /// byte-order mark, as TS 126 245 strings may be.
    pub fn new_unicode_fonts(
        raw: &Bytes,
        layout: ConfigLayout,
        budget: &Budget,
        limits: ConfigLimits,
    ) -> Result<Self> {
        let mut job = Self::new(raw, layout, budget, limits)?;
        job.unicode_font_names = true;
        Ok(job)
    }
    /// Starts on `raw` in `layout`; font names must be UTF-8. Fails with
    /// `Error::ForeignPool` when `raw` is not charged to `budget` and
    /// `Error::Limit` when it is longer than `limits.maximum_bytes`.
    pub fn new(
        raw: &Bytes,
        layout: ConfigLayout,
        budget: &Budget,
        limits: ConfigLimits,
    ) -> Result<Self> {
        if !raw.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        if raw.len() > limits.maximum_bytes {
            return Err(Error::Limit {
                requested: raw.len(),
                maximum: limits.maximum_bytes,
            });
        }
        Ok(Self {
            raw: raw.clone(),
            layout,
            budget: budget.clone(),
            limits,
            fixed: [0; 38],
            fixed_read: 0,
            defaults: None,
            at: 0,
            header: [0; 16],
            header_read: 0,
            header_size: 8,
            box_start: 0,
            box_end: 0,
            boxes: BudgetVec::new(budget),
            fonts: BudgetVec::new(budget),
            ftab_seen: false,
            font_count: 0,
            font_index: 0,
            font_header: [0; 3],
            font_read: 0,
            font_pending: None,
            name_cursor: None,
            name_probe: None,
            unicode_font_names: false,
            paid: false,
            phase: Phase::Fixed,
            failed: None,
        })
    }
    fn u16(bytes: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([bytes[at], bytes[at + 1]])
    }
    fn defaults(&self) -> Result<Defaults> {
        let offset = if self.layout == ConfigLayout::SampleEntryBody {
            8
        } else {
            0
        };
        if offset == 8 && self.fixed[..6] != [0; 6] {
            return Err(Error::Invalid("tx3g SampleEntry reserved bytes"));
        }
        let b = &self.fixed[offset..offset + 30];
        let style = StyleRecord {
            start: Self::u16(b, 18),
            end: Self::u16(b, 20),
            font_id: Self::u16(b, 22),
            face_flags: b[24],
            font_size: b[25],
            rgba: b[26..30].try_into().map_err(|_| Error::Truncated)?,
        };
        if style.start != 0 || style.end != 0 {
            return Err(Error::Invalid("tx3g default character range"));
        }
        let horizontal = b[4] as i8;
        let vertical = b[5] as i8;
        if !(-1..=1).contains(&horizontal) || !(-1..=1).contains(&vertical) {
            return Err(Error::Invalid("tx3g justification"));
        }
        Ok(Defaults {
            data_reference_index: (offset == 8).then(|| Self::u16(&self.fixed, 6)),
            display_flags: u32::from_be_bytes(b[..4].try_into().map_err(|_| Error::Truncated)?),
            horizontal,
            vertical,
            background: b[6..10].try_into().map_err(|_| Error::Truncated)?,
            text_box: [
                Self::u16(b, 10) as i16,
                Self::u16(b, 12) as i16,
                Self::u16(b, 14) as i16,
                Self::u16(b, 16) as i16,
            ],
            style,
        })
    }
    /// Advances by at most `work` units: one per byte read and one per box or
    /// font stored.
    ///
    /// Fails with `Error::Truncated` when the bytes end inside a field, box or
    /// font name, `Error::Invalid` when a fixed field is out of range, a box is
    /// smaller than its header, a font name does not decode, or the `ftab` is
    /// missing, repeated or has bytes after its entries, and `Error::Limit` past
    /// `limits`. Every later call returns such an error; cancellation and
    /// backpressure are not kept.
    /// After [`Self::take_output`] the call fails with `Error::Invalid`.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ConfigStatus> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Cancelled | Error::Backpressure { .. })
        {
            self.fonts = BudgetVec::new(&self.budget);
            self.boxes = BudgetVec::new(&self.budget);
            self.failed = Some(error);
        }
        result
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ConfigStatus> {
        loop {
            cancelled(cancel)?;
            if self.phase == Phase::Ready {
                return Ok(ConfigStatus::Ready);
            }
            if self.phase == Phase::Taken {
                return Err(Error::Invalid("tx3g config already taken"));
            }
            if work.remaining() == 0 {
                return Ok(ConfigStatus::Pending);
            }
            match self.phase {
                Phase::Fixed => {
                    let count = if self.layout == ConfigLayout::SampleEntryBody {
                        38
                    } else {
                        30
                    };
                    if self.fixed_read < count {
                        let byte = *self
                            .raw
                            .as_ref()
                            .get(self.fixed_read)
                            .ok_or(Error::Truncated)?;
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        self.fixed[self.fixed_read] = byte;
                        self.fixed_read += 1;
                    } else {
                        self.defaults = Some(self.defaults()?);
                        self.at = count;
                        self.phase = Phase::Header;
                    }
                }
                Phase::Header => {
                    if self.at == self.raw.len() && self.header_read == 0 {
                        if !self.ftab_seen {
                            return Err(Error::Invalid("tx3g font table absent"));
                        }
                        self.phase = Phase::Ready;
                        continue;
                    }
                    if self.header_read == 0 {
                        self.box_start = self.at;
                    }
                    if self.header_read < self.header_size {
                        let offset = self
                            .box_start
                            .checked_add(self.header_read)
                            .ok_or(Error::Overflow)?;
                        let byte = *self.raw.as_ref().get(offset).ok_or(Error::Truncated)?;
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        self.header[self.header_read] = byte;
                        self.header_read += 1;
                    } else if self.header_size == 8 && self.header[..4] == [0, 0, 0, 1] {
                        self.header_size = 16;
                    } else {
                        let short = u32::from_be_bytes(
                            self.header[..4].try_into().map_err(|_| Error::Truncated)?,
                        );
                        let size = match short {
                            0 => self.raw.len() - self.box_start,
                            1 => usize::try_from(u64::from_be_bytes(
                                self.header[8..16]
                                    .try_into()
                                    .map_err(|_| Error::Truncated)?,
                            ))
                            .map_err(|_| Error::Overflow)?,
                            n => n as usize,
                        };
                        let minimum =
                            self.header_size + if self.header[4..8] == *b"uuid" { 16 } else { 0 };
                        if size < minimum {
                            return Err(Error::Invalid("tx3g config box size"));
                        }
                        self.box_end = self.box_start.checked_add(size).ok_or(Error::Overflow)?;
                        if self.box_end > self.raw.len() {
                            return Err(Error::Truncated);
                        }
                        if self.boxes.len() >= self.limits.maximum_boxes {
                            return Err(Error::Limit {
                                requested: self.boxes.len() + 1,
                                maximum: self.limits.maximum_boxes,
                            });
                        }
                        self.phase = Phase::BoxCommit;
                        self.paid = false;
                    }
                }
                Phase::BoxCommit => {
                    if !self.paid {
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        self.paid = true;
                    }
                    self.boxes
                        .try_push(Modifier {
                            kind: self.header[4..8].try_into().map_err(|_| Error::Truncated)?,
                            range: self.box_start..self.box_end,
                        })
                        .map_err(|e| e.error)?;
                    self.at = self.box_start + self.header_size;
                    let font = self.header[4..8] == *b"ftab";
                    self.header_read = 0;
                    self.header_size = 8;
                    self.paid = false;
                    if font {
                        if self.ftab_seen {
                            return Err(Error::Invalid("duplicate tx3g font table"));
                        }
                        self.ftab_seen = true;
                        self.font_read = 0;
                        self.phase = Phase::FontCount;
                    } else {
                        self.at = self.box_end;
                        self.phase = Phase::Header;
                    }
                }
                Phase::FontCount => {
                    if self.font_read < 2 {
                        if self.at >= self.box_end {
                            return Err(Error::Truncated);
                        }
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        self.font_header[self.font_read] = self.raw.as_ref()[self.at];
                        self.at += 1;
                        self.font_read += 1;
                    } else {
                        self.font_count = usize::from(Self::u16(&self.font_header, 0));
                        self.font_index = 0;
                        self.font_read = 0;
                        if self.font_count > self.limits.maximum_fonts {
                            return Err(Error::Limit {
                                requested: self.font_count,
                                maximum: self.limits.maximum_fonts,
                            });
                        }
                        self.phase = Phase::FontHeader;
                    }
                }
                Phase::FontHeader => {
                    if self.font_index == self.font_count {
                        if self.at != self.box_end {
                            return Err(Error::Invalid("tx3g font table trailing bytes"));
                        }
                        self.phase = Phase::Header;
                        continue;
                    }
                    if self.font_read < 3 {
                        if self.at >= self.box_end {
                            return Err(Error::Truncated);
                        }
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        self.font_header[self.font_read] = self.raw.as_ref()[self.at];
                        self.at += 1;
                        self.font_read += 1;
                    } else {
                        let end = self
                            .at
                            .checked_add(usize::from(self.font_header[2]))
                            .ok_or(Error::Overflow)?;
                        if end > self.box_end {
                            return Err(Error::Truncated);
                        }
                        self.font_pending = Some(FontRecord {
                            id: Self::u16(&self.font_header, 0),
                            name_range: self.at..end,
                        });
                        if self.unicode_font_names {
                            self.name_probe = Some(Probe::new(self.at..end, Encoding::Tx3gInBand));
                        } else {
                            self.name_cursor = Some(Cursor::new(self.at..end, Encoding::Utf8));
                        }
                        self.at = end;
                        self.phase = Phase::FontName;
                    }
                }
                Phase::FontName => {
                    if let Some(probe) = self.name_probe.as_mut() {
                        match probe.poll(self.raw.as_ref(), work, cancel)? {
                            Poll::Yield => return Ok(ConfigStatus::Pending),
                            Poll::Ready((encoding, range)) => {
                                self.name_cursor = Some(Cursor::new(range, encoding));
                                self.name_probe = None;
                                continue;
                            }
                            Poll::End => return Err(Error::Truncated),
                        }
                    }
                    match self
                        .name_cursor
                        .as_mut()
                        .ok_or(Error::Invalid("tx3g font cursor absent"))?
                        .poll(self.raw.as_ref(), work, cancel)?
                    {
                        Poll::Yield => return Ok(ConfigStatus::Pending),
                        Poll::Ready(_) => {}
                        Poll::End => {
                            self.phase = Phase::FontCommit;
                            self.paid = false;
                        }
                    }
                }
                Phase::FontCommit => {
                    if !self.paid {
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        self.paid = true;
                    }
                    let font = self
                        .font_pending
                        .as_ref()
                        .ok_or(Error::Invalid("tx3g font absent"))?
                        .clone();
                    self.fonts.try_push(font).map_err(|e| e.error)?;
                    self.font_pending = None;
                    self.name_cursor = None;
                    self.font_read = 0;
                    self.font_index += 1;
                    self.paid = false;
                    self.phase = Phase::FontHeader;
                }
                // The loop's head returns in these phases.
                Phase::Ready | Phase::Taken => continue,
            }
        }
    }
    /// Hands over the description once [`Self::poll`] returned
    /// [`ConfigStatus::Ready`]; `Error::Invalid` before that and on a second
    /// call, and the job's error after a failure.
    pub fn take_output(&mut self, cancel: &AtomicBool) -> Result<Configuration> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("tx3g config not ready"));
        }
        let result = Configuration {
            raw: self.raw.clone(),
            layout: self.layout,
            defaults: self
                .defaults
                .ok_or(Error::Invalid("tx3g defaults absent"))?,
            fonts: std::mem::replace(&mut self.fonts, BudgetVec::new(&self.budget)),
            boxes: std::mem::replace(&mut self.boxes, BudgetVec::new(&self.budget)),
        };
        self.phase = Phase::Taken;
        Ok(result)
    }
}
