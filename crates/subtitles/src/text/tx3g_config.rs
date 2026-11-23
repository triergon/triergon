//! Parser for a tx3g sample description (the ETSI TS 126 245 text sample
//! entry): display defaults, default style, font table and child boxes. The
//! caller names the layout; nothing is inferred and no inherited style is
//! guessed.
use super::{
    Encoding, Modifier,
    unicode::{Cursor, Poll, Probe},
};
use crate::cancelled;
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
/// The decoder of a font name: its encoding selector first, where names may
/// be UTF-16, then its characters.
#[derive(Debug)]
enum Name {
    Probe(Probe),
    Scalars(Cursor),
}

#[derive(Debug)]
enum Phase {
    /// The fixed fields read so far: 38 bytes of a sample entry body, 30 of a
    /// defaults record.
    Fixed {
        bytes: [u8; 38],
        read: usize,
    },
    /// The header of the child box at `start`, read so far: 8 bytes, or 16
    /// when its size field is 1.
    Header {
        start: usize,
        bytes: [u8; 16],
        read: usize,
    },
    /// Storing the box of `kind` over `range`, whose header is `header`
    /// bytes long; whether the store's unit is paid.
    BoxCommit {
        kind: [u8; 4],
        range: Range<usize>,
        header: usize,
        paid: bool,
    },
    /// The entry count of the `ftab` that ends at `end`.
    FontCount {
        end: usize,
        bytes: [u8; 2],
        read: usize,
    },
    /// The identifier and name length of the next of `left` entries.
    FontHeader {
        end: usize,
        left: usize,
        bytes: [u8; 3],
        read: usize,
    },
    /// Checking the name of `font`.
    FontName {
        end: usize,
        left: usize,
        font: FontRecord,
        name: Name,
    },
    /// Storing `font`; whether the store's unit is paid.
    FontCommit {
        end: usize,
        left: usize,
        font: FontRecord,
        paid: bool,
    },
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
    /// The fixed fields, once read.
    defaults: Option<Defaults>,
    /// The next byte to read.
    at: usize,
    boxes: BudgetVec<Modifier>,
    fonts: BudgetVec<FontRecord>,
    ftab_seen: bool,
    unicode_font_names: bool,
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
            defaults: None,
            at: 0,
            boxes: BudgetVec::new(budget),
            fonts: BudgetVec::new(budget),
            ftab_seen: false,
            unicode_font_names: false,
            phase: Phase::Fixed {
                bytes: [0; 38],
                read: 0,
            },
            failed: None,
        })
    }
    fn u16(bytes: &[u8], at: usize) -> u16 {
        u16::from_be_bytes([bytes[at], bytes[at + 1]])
    }
    /// The fixed fields of `layout` in `fixed`.
    fn defaults(layout: ConfigLayout, fixed: &[u8; 38]) -> Result<Defaults> {
        let offset = if layout == ConfigLayout::SampleEntryBody {
            8
        } else {
            0
        };
        if offset == 8 && fixed[..6] != [0; 6] {
            return Err(Error::Invalid("tx3g SampleEntry reserved bytes"));
        }
        let b = &fixed[offset..offset + 30];
        let style = StyleRecord {
            start: Self::u16(b, 18),
            end: Self::u16(b, 20),
            font_id: Self::u16(b, 22),
            face_flags: b[24],
            font_size: b[25],
            rgba: [b[26], b[27], b[28], b[29]],
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
            data_reference_index: (offset == 8).then(|| Self::u16(fixed, 6)),
            display_flags: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            horizontal,
            vertical,
            background: [b[6], b[7], b[8], b[9]],
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
            match self.phase {
                Phase::Ready => return Ok(ConfigStatus::Ready),
                Phase::Taken => return Err(Error::Invalid("tx3g config already taken")),
                _ => {}
            }
            if work.remaining() == 0 {
                return Ok(ConfigStatus::Pending);
            }
            let raw = self.raw.as_ref();
            match &mut self.phase {
                Phase::Fixed { bytes, read } => {
                    let count = if self.layout == ConfigLayout::SampleEntryBody {
                        38
                    } else {
                        30
                    };
                    if *read < count {
                        let byte = *raw.get(*read).ok_or(Error::Truncated)?;
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        bytes[*read] = byte;
                        *read += 1;
                    } else {
                        self.defaults = Some(Self::defaults(self.layout, bytes)?);
                        self.at = count;
                        self.phase = Phase::Header {
                            start: count,
                            bytes: [0; 16],
                            read: 0,
                        };
                    }
                }
                Phase::Header { start, bytes, read } => {
                    if self.at == raw.len() && *read == 0 {
                        if !self.ftab_seen {
                            return Err(Error::Invalid("tx3g font table absent"));
                        }
                        self.phase = Phase::Ready;
                        continue;
                    }
                    if *read == 0 {
                        *start = self.at;
                    }
                    // A size field of 1 is followed by a 64-bit size.
                    let header = if *read >= 8 && bytes[..4] == [0, 0, 0, 1] {
                        16
                    } else {
                        8
                    };
                    if *read < header {
                        let offset = start.checked_add(*read).ok_or(Error::Overflow)?;
                        let byte = *raw.get(offset).ok_or(Error::Truncated)?;
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        bytes[*read] = byte;
                        *read += 1;
                        continue;
                    }
                    let [a, b, c, d, kind @ .., e, f, g, h, i, j, k, l] = *bytes;
                    let size = match u32::from_be_bytes([a, b, c, d]) {
                        0 => raw.len() - *start,
                        1 => usize::try_from(u64::from_be_bytes([e, f, g, h, i, j, k, l]))
                            .map_err(|_| Error::Overflow)?,
                        n => n as usize,
                    };
                    let minimum = header + if kind == *b"uuid" { 16 } else { 0 };
                    if size < minimum {
                        return Err(Error::Invalid("tx3g config box size"));
                    }
                    let end = start.checked_add(size).ok_or(Error::Overflow)?;
                    if end > raw.len() {
                        return Err(Error::Truncated);
                    }
                    if self.boxes.len() >= self.limits.maximum_boxes {
                        return Err(Error::Limit {
                            requested: self.boxes.len() + 1,
                            maximum: self.limits.maximum_boxes,
                        });
                    }
                    self.phase = Phase::BoxCommit {
                        kind,
                        range: *start..end,
                        header,
                        paid: false,
                    };
                }
                Phase::BoxCommit {
                    kind,
                    range,
                    header,
                    paid,
                } => {
                    if !*paid {
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        *paid = true;
                    }
                    self.boxes
                        .try_push(Modifier {
                            kind: *kind,
                            range: range.clone(),
                        })
                        .map_err(|e| e.error)?;
                    if *kind == *b"ftab" {
                        if self.ftab_seen {
                            return Err(Error::Invalid("duplicate tx3g font table"));
                        }
                        self.ftab_seen = true;
                        self.at = range.start + *header;
                        self.phase = Phase::FontCount {
                            end: range.end,
                            bytes: [0; 2],
                            read: 0,
                        };
                    } else {
                        self.at = range.end;
                        self.phase = Phase::Header {
                            start: range.end,
                            bytes: [0; 16],
                            read: 0,
                        };
                    }
                }
                Phase::FontCount { end, bytes, read } => {
                    if *read < 2 {
                        if self.at >= *end {
                            return Err(Error::Truncated);
                        }
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        bytes[*read] = raw[self.at];
                        self.at += 1;
                        *read += 1;
                    } else {
                        let left = usize::from(u16::from_be_bytes(*bytes));
                        if left > self.limits.maximum_fonts {
                            return Err(Error::Limit {
                                requested: left,
                                maximum: self.limits.maximum_fonts,
                            });
                        }
                        self.phase = Phase::FontHeader {
                            end: *end,
                            left,
                            bytes: [0; 3],
                            read: 0,
                        };
                    }
                }
                Phase::FontHeader {
                    end,
                    left,
                    bytes,
                    read,
                } => {
                    if *left == 0 {
                        if self.at != *end {
                            return Err(Error::Invalid("tx3g font table trailing bytes"));
                        }
                        self.phase = Phase::Header {
                            start: self.at,
                            bytes: [0; 16],
                            read: 0,
                        };
                        continue;
                    }
                    if *read < 3 {
                        if self.at >= *end {
                            return Err(Error::Truncated);
                        }
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        bytes[*read] = raw[self.at];
                        self.at += 1;
                        *read += 1;
                    } else {
                        let [id_high, id_low, length] = *bytes;
                        let name_end = self
                            .at
                            .checked_add(usize::from(length))
                            .ok_or(Error::Overflow)?;
                        if name_end > *end {
                            return Err(Error::Truncated);
                        }
                        let name = self.at..name_end;
                        let decoder = if self.unicode_font_names {
                            Name::Probe(Probe::new(name.clone(), Encoding::Tx3gInBand))
                        } else {
                            Name::Scalars(Cursor::new(name.clone(), Encoding::Utf8))
                        };
                        self.at = name.end;
                        self.phase = Phase::FontName {
                            end: *end,
                            left: *left,
                            font: FontRecord {
                                id: u16::from_be_bytes([id_high, id_low]),
                                name_range: name,
                            },
                            name: decoder,
                        };
                    }
                }
                Phase::FontName {
                    end,
                    left,
                    font,
                    name,
                } => match name {
                    Name::Probe(probe) => {
                        let Some((encoding, range)) = probe.poll(raw, work, cancel)? else {
                            return Ok(ConfigStatus::Pending);
                        };
                        *name = Name::Scalars(Cursor::new(range, encoding));
                    }
                    Name::Scalars(cursor) => match cursor.poll(raw, work, cancel)? {
                        Poll::Yield => return Ok(ConfigStatus::Pending),
                        Poll::Ready(_) => {}
                        Poll::End => {
                            self.phase = Phase::FontCommit {
                                end: *end,
                                left: *left,
                                font: font.clone(),
                                paid: false,
                            };
                        }
                    },
                },
                Phase::FontCommit {
                    end,
                    left,
                    font,
                    paid,
                } => {
                    if !*paid {
                        if !work.try_charge(1) {
                            return Ok(ConfigStatus::Pending);
                        }
                        *paid = true;
                    }
                    self.fonts.try_push(font.clone()).map_err(|e| e.error)?;
                    self.phase = Phase::FontHeader {
                        end: *end,
                        left: *left - 1,
                        bytes: [0; 3],
                        read: 0,
                    };
                }
                // The loop's head returns in these phases.
                Phase::Ready | Phase::Taken => {}
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
        let (Phase::Ready, Some(defaults)) = (&self.phase, self.defaults) else {
            return Err(Error::Invalid("tx3g config not ready"));
        };
        let result = Configuration {
            raw: self.raw.clone(),
            layout: self.layout,
            defaults,
            fonts: std::mem::replace(&mut self.fonts, BudgetVec::new(&self.budget)),
            boxes: std::mem::replace(&mut self.boxes, BudgetVec::new(&self.budget)),
        };
        self.phase = Phase::Taken;
        Ok(result)
    }
}
