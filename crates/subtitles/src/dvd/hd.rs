//! Original extended-SPU wire facts and bounded eight-bit source raster.
//! Inferred, pinned implementation evidence is not a normative DVD claim.
//! Header width, field width, pixel mode and source colour are independent.
use super::{DvdArea, DvdBitmap, DvdDate};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};
use triergon_pixel::{ChannelRounding, YcbcrMatrix};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdSpan {
    pub start: usize,
    pub end: usize,
}
impl HdSpan {
    pub fn get(self, raw: &[u8]) -> Result<&[u8]> {
        raw.get(self.start..self.end).ok_or(Error::Truncated)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdFields {
    pub top: u32,
    pub bottom: u32,
    pub operand_bits: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct HdPacket<'a> {
    raw: &'a [u8],
    header: usize,
    control: usize,
    wide: bool,
}
fn number(raw: &[u8], at: usize, width: usize) -> Result<u32> {
    let bytes = raw
        .get(at..at.checked_add(width).ok_or(Error::Overflow)?)
        .ok_or(Error::Truncated)?;
    Ok(bytes
        .iter()
        .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte)))
}
impl<'a> HdPacket<'a> {
    pub fn parse(raw: &'a [u8]) -> Result<Self> {
        let wide = number(raw, 0, 2)? == 0;
        let width = if wide { 4 } else { 2 };
        let header = if wide { 10 } else { 4 };
        let size = usize::try_from(number(raw, if wide { 2 } else { 0 }, width)?)
            .map_err(|_| Error::Overflow)?;
        let control =
            usize::try_from(number(raw, header - width, width)?).map_err(|_| Error::Overflow)?;
        if raw.len() < size {
            return Err(Error::Truncated);
        }
        if raw.len() > size {
            return Err(Error::Invalid("HD SPU trailing bytes"));
        }
        if control < header
            || control
                .checked_add(3 + width)
                .is_none_or(|end| end > size.min(raw.len()))
        {
            return Err(Error::Invalid("HD SPU control extent"));
        }
        Ok(Self {
            raw,
            header,
            control,
            wide,
        })
    }
    pub fn raw(self) -> &'a [u8] {
        self.raw
    }
    pub fn header_bytes(self) -> usize {
        self.header
    }
    pub fn wide_header(self) -> bool {
        self.wide
    }
    pub fn control_offset(self) -> usize {
        self.control
    }
    pub fn controls(self, max_controls: usize, max_commands: usize) -> HdControls<'a> {
        HdControls {
            packet: self,
            at: Some(self.control),
            count: 0,
            max_controls,
            max_commands,
        }
    }
    fn block(self, at: usize) -> Result<HdControl<'a>> {
        let width = if self.wide { 4 } else { 2 };
        if at < self.control {
            return Err(Error::Invalid("HD control before control area"));
        }
        let date = DvdDate(u16::try_from(number(self.raw, at, 2)?).map_err(|_| Error::Overflow)?);
        let next_offset = number(self.raw, at.checked_add(2).ok_or(Error::Overflow)?, width)?;
        let next = usize::try_from(next_offset).map_err(|_| Error::Overflow)?;
        let start = at.checked_add(2 + width).ok_or(Error::Overflow)?;
        let end = if next == at {
            self.raw.len()
        } else {
            // An end that does not fit `usize` lies past the packet on every target.
            if next <= start
                || next
                    .checked_add(3 + width)
                    .is_none_or(|end| end > self.raw.len())
            {
                return Err(Error::Invalid(
                    "HD backward, overlapping or out-of-range link",
                ));
            }
            next
        };
        if start >= end {
            return Err(Error::Truncated);
        }
        Ok(HdControl {
            date,
            offset: at,
            next_offset,
            span: HdSpan { start: at, end },
            commands: HdSpan { start, end },
            raw: self.raw,
            maximum: usize::MAX,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HdControl<'a> {
    pub date: DvdDate,
    pub offset: usize,
    pub next_offset: u32,
    pub span: HdSpan,
    commands: HdSpan,
    raw: &'a [u8],
    maximum: usize,
}
impl<'a> HdControl<'a> {
    pub fn raw(self) -> Result<&'a [u8]> {
        self.span.get(self.raw)
    }
    pub fn commands(self) -> HdCommands<'a> {
        HdCommands {
            raw: self.raw,
            at: self.commands.start,
            end: self.commands.end,
            count: 0,
            maximum: self.maximum,
            done: false,
        }
    }
}
#[derive(Debug)]
pub struct HdControls<'a> {
    packet: HdPacket<'a>,
    at: Option<usize>,
    count: usize,
    max_controls: usize,
    max_commands: usize,
}
impl<'a> Iterator for HdControls<'a> {
    type Item = Result<HdControl<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        let at = self.at.take()?;
        if self.count == self.max_controls {
            return Some(Err(Error::Limit {
                requested: self.count.saturating_add(1),
                maximum: self.max_controls,
            }));
        }
        let result = self.packet.block(at).map(|mut block| {
            self.count += 1;
            if usize::try_from(block.next_offset).ok() != Some(at) {
                self.at = Some(block.next_offset as usize);
            }
            block.maximum = self.max_commands;
            block
        });
        Some(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdCommand<'a> {
    Forced,
    Start,
    Stop,
    Palette4(u16),
    Alpha4(u16),
    YCrCb(&'a [u8]),
    Contrast(&'a [u8]),
    Area {
        value: DvdArea,
        eight_bit_signal: bool,
    },
    Fields(HdFields),
    End,
    Opaque {
        opcode: u8,
        remainder: &'a [u8],
    },
}
fn operand_len(opcode: u8) -> Option<usize> {
    Some(match opcode {
        0..=2 | 0xff => 0,
        3 | 4 => 2,
        5 | 0x85 => 6,
        6 => 4,
        0x86 => 8,
        0x83 => 768,
        0x84 => 256,
        _ => return None,
    })
}
fn decode_command(opcode: u8, data: &[u8]) -> Result<HdCommand<'_>> {
    Ok(match opcode {
        0 => HdCommand::Forced,
        1 => HdCommand::Start,
        2 => HdCommand::Stop,
        3 => HdCommand::Palette4(number(data, 0, 2)? as u16),
        4 => HdCommand::Alpha4(number(data, 0, 2)? as u16),
        0x83 => HdCommand::YCrCb(data),
        0x84 => HdCommand::Contrast(data),
        5 | 0x85 => HdCommand::Area {
            value: DvdArea {
                left: u16::from(data[0]) * 16 + u16::from(data[1] >> 4),
                right: u16::from(data[1] & 15) * 256 + u16::from(data[2]),
                top: u16::from(data[3]) * 16 + u16::from(data[4] >> 4),
                bottom: u16::from(data[4] & 15) * 256 + u16::from(data[5]),
            },
            eight_bit_signal: opcode == 0x85,
        },
        6 | 0x86 => {
            let width = if opcode == 6 { 2 } else { 4 };
            HdCommand::Fields(HdFields {
                top: number(data, 0, width)?,
                bottom: number(data, width, width)?,
                operand_bits: (8 * width) as u8,
            })
        }
        0xff => HdCommand::End,
        _ => HdCommand::Opaque {
            opcode,
            remainder: data,
        },
    })
}
#[derive(Debug)]
pub struct HdCommands<'a> {
    raw: &'a [u8],
    at: usize,
    end: usize,
    count: usize,
    maximum: usize,
    done: bool,
}
impl<'a> Iterator for HdCommands<'a> {
    type Item = Result<HdCommand<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = (|| {
            if self.count == self.maximum {
                return Err(Error::Limit {
                    requested: self.count.saturating_add(1),
                    maximum: self.maximum,
                });
            }
            self.count += 1;
            let opcode = *self
                .raw
                .get(self.at)
                .filter(|_| self.at < self.end)
                .ok_or(Error::Truncated)?;
            self.at += 1;
            let end = match operand_len(opcode) {
                Some(n) => self.at.checked_add(n).ok_or(Error::Overflow)?,
                None => self.end,
            };
            if end > self.end {
                return Err(Error::Truncated);
            }
            let data = &self.raw[self.at..end];
            self.at = end;
            self.done = opcode == 0xff || operand_len(opcode).is_none();
            decode_command(opcode, data)
        })();
        if result.is_err() {
            self.done = true;
        }
        Some(result)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HdFacts {
    pub ycrcb: Option<HdSpan>,
    pub contrast: Option<HdSpan>,
    pub area: Option<DvdArea>,
    pub area_wire: Option<HdSpan>,
    pub fields: Option<HdFields>,
    pub fields_wire: Option<HdSpan>,
    pub eight_bit: bool,
    pub forced: bool,
    pub menu: bool,
    pub start_date: Option<DvdDate>,
    pub stop_date: Option<DvdDate>,
    /// A forced command's source date is distinct from an ordinary start.
    pub forced_date: Option<DvdDate>,
    pub palette4_word: Option<u16>,
    pub palette4_wire: Option<HdSpan>,
    pub alpha4_word: Option<u16>,
    pub alpha4_wire: Option<HdSpan>,
    pub selected_control: Option<usize>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HdPolicy {
    /// Default constructor profile, used by `new`.
    #[default]
    StrictEightBit,
    /// Strict source decoding with either the four-entry or eight-bit grammar.
    Strict,
}
#[derive(Clone, Copy, Debug)]
pub struct HdLimits {
    pub max_packet: usize,
    pub max_controls: usize,
    pub max_commands: usize,
    pub max_axis: u16,
    pub max_pixels: usize,
    pub max_work: usize,
}
impl Default for HdLimits {
    fn default() -> Self {
        Self {
            max_packet: 16 * 1024 * 1024,
            max_controls: 4096,
            max_commands: 16384,
            max_axis: 4096,
            max_pixels: 8 * 1024 * 1024,
            max_work: 128 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdStatus {
    Yield,
    Ready,
}
#[derive(Debug)]
pub struct HdSubpicture {
    packet: Bytes,
    bitmap: DvdBitmap,
    facts: HdFacts,
}
impl HdSubpicture {
    pub fn packet(&self) -> &Bytes {
        &self.packet
    }
    pub fn bitmap(&self) -> &DvdBitmap {
        &self.bitmap
    }
    pub fn facts(&self) -> HdFacts {
        self.facts
    }
    pub fn ycrcb(&self) -> Option<&[u8]> {
        self.facts.ycrcb.and_then(|s| s.get(&self.packet).ok())
    }
    pub fn contrast(&self) -> Option<&[u8]> {
        self.facts.contrast.and_then(|s| s.get(&self.packet).ok())
    }
    pub fn into_parts(self) -> (Bytes, DvdBitmap, HdFacts) {
        (self.packet, self.bitmap, self.facts)
    }
    pub fn palette_job(&self) -> Result<HdPaletteJob<'_>> {
        Ok(HdPaletteJob {
            ycrcb: self
                .ycrcb()
                .ok_or(Error::Unsupported("HD source YCrCb absent"))?,
            contrast: self
                .contrast()
                .ok_or(Error::Unsupported("HD source contrast absent"))?,
            values: [0; 256],
            at: 0,
            failed: None,
            finished: false,
            alpha4_override: self.facts.alpha4_word.filter(|_| {
                self.facts
                    .alpha4_wire
                    .zip(self.facts.contrast)
                    .is_some_and(|(a, c)| a.start > c.start)
            }),
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ScanBlock {
    offset: usize,
    next: usize,
    end: usize,
    at: usize,
    date: DvdDate,
    local_area: bool,
    local_fields: bool,
    forced: bool,
}
#[derive(Clone, Copy, Debug)]
struct Operand {
    opcode: u8,
    start: usize,
    end: usize,
    paid: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Scan,
    Reserve,
    Allocate,
    Raster,
    Ready,
    Finished,
}
/// Owned strict source decoding. `new` uses the strict eight-bit profile;
/// [`HdPolicy::Strict`] accepts either pixel grammar. Original source bytes
/// and errors survive.
#[derive(Debug)]
pub struct HdSpuJob {
    packet: Option<Bytes>,
    budget: Budget,
    pixels: Option<BudgetVec<u8>>,
    limits: HdLimits,
    policy: HdPolicy,
    control: usize,
    header: usize,
    wide: bool,
    next: Option<usize>,
    header_paid: usize,
    block: Option<ScanBlock>,
    operand: Option<Operand>,
    controls: usize,
    commands: usize,
    facts: HdFacts,
    selected: Option<HdFacts>,
    work: usize,
    phase: Phase,
    failed: Option<Error>,
    width: u16,
    height: u16,
    row: u16,
    column: u16,
    bit_at: [usize; 2],
    token_stage: u8,
    bits_needed: u8,
    bits_read: u8,
    value: u16,
    has_run: bool,
    index: u8,
    length_bits: u8,
    run_left: u16,
    nibble_word: u16,
    nibbles: u8,
}
impl HdSpuJob {
    pub fn new(packet: Bytes, budget: &Budget, limits: HdLimits) -> Result<Self> {
        Self::with_policy(packet, budget, limits, HdPolicy::StrictEightBit)
    }
    pub fn with_policy(
        packet: Bytes,
        budget: &Budget,
        limits: HdLimits,
        policy: HdPolicy,
    ) -> Result<Self> {
        if !packet.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        if packet.len() > limits.max_packet {
            return Err(Error::Limit {
                requested: packet.len(),
                maximum: limits.max_packet,
            });
        }
        let p = HdPacket::parse(&packet)?;
        let (control, header, wide) = (p.control, p.header, p.wide);
        Ok(Self {
            packet: Some(packet),
            budget: budget.clone(),
            pixels: None,
            limits,
            policy,
            control,
            header,
            wide,
            next: Some(control),
            header_paid: 0,
            block: None,
            operand: None,
            controls: 0,
            commands: 0,
            facts: HdFacts::default(),
            selected: None,
            work: 0,
            phase: Phase::Scan,
            failed: None,
            width: 0,
            height: 0,
            row: 0,
            column: 0,
            bit_at: [0; 2],
            token_stage: 0,
            bits_needed: 1,
            bits_read: 0,
            value: 0,
            has_run: false,
            index: 0,
            length_bits: 0,
            run_left: 0,
            nibble_word: 0,
            nibbles: 0,
        })
    }
    pub fn work_spent(&self) -> usize {
        self.work
    }
    pub fn source(&self) -> Option<&Bytes> {
        self.packet.as_ref()
    }
    pub fn source_facts(&self) -> HdFacts {
        if self.failed.is_some() {
            self.facts
        } else {
            self.selected.unwrap_or(self.facts)
        }
    }
    fn raw(&self) -> Result<&[u8]> {
        self.packet
            .as_deref()
            .ok_or(Error::Invalid("HD source absent"))
    }
    fn poison(&mut self, error: Error) -> Error {
        self.pixels = None;
        if error == Error::Cancelled {
            self.packet = None;
        }
        self.failed = Some(error);
        error
    }
    fn pay(&mut self, n: usize, work: &mut WorkBudget) -> Result<usize> {
        if work.remaining() == 0 {
            return Ok(0);
        }
        if self.work == self.limits.max_work {
            return Err(Error::Limit {
                requested: self.work.saturating_add(1),
                maximum: self.limits.max_work,
            });
        }
        let take = n
            .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
            .min(self.limits.max_work - self.work);
        work.try_charge(take as u64);
        self.work += take;
        Ok(take)
    }
    fn apply(&mut self, operand: Operand) -> Result<()> {
        let span = HdSpan {
            start: operand.start,
            end: operand.end,
        };
        let command = decode_command(operand.opcode, span.get(self.raw()?)?)?;
        let mut b = self
            .block
            .ok_or(Error::Invalid("HD control state absent"))?;
        match command {
            HdCommand::Forced => {
                b.forced = true;
                self.facts.menu = true;
                self.facts.forced_date = Some(b.date);
            }
            HdCommand::Start => self.facts.start_date = Some(b.date),
            HdCommand::Stop => self.facts.stop_date = Some(b.date),
            HdCommand::YCrCb(_) => self.facts.ycrcb = Some(span),
            HdCommand::Contrast(_) => self.facts.contrast = Some(span),
            HdCommand::Area {
                value,
                eight_bit_signal,
            } => {
                self.facts.area = Some(value);
                self.facts.area_wire = Some(span);
                self.facts.eight_bit |= eight_bit_signal;
                b.local_area = true;
            }
            HdCommand::Fields(value) => {
                self.facts.fields = Some(value);
                self.facts.fields_wire = Some(span);
                b.local_fields = true;
            }
            HdCommand::Palette4(word) => {
                if self.policy == HdPolicy::StrictEightBit {
                    return Err(Error::Unsupported("HD mixed four-entry palette operation"));
                }
                self.facts.palette4_word = Some(word);
                self.facts.palette4_wire = Some(span);
            }
            HdCommand::Alpha4(word) => {
                if self.policy == HdPolicy::StrictEightBit {
                    return Err(Error::Unsupported("HD mixed four-entry palette operation"));
                }
                self.facts.alpha4_word = Some(word);
                self.facts.alpha4_wire = Some(span);
            }
            HdCommand::Opaque { .. } => return Err(Error::Unsupported("HD opaque control")),
            HdCommand::End => {
                if b.local_area && b.local_fields {
                    if self.selected.is_some() {
                        return Err(Error::Unsupported("HD multiple locally eligible rasters"));
                    }
                    let mut facts = self.facts;
                    facts.forced = b.forced;
                    facts.selected_control = Some(b.offset);
                    self.selected = Some(facts);
                }
                self.next = if b.next == b.offset {
                    None
                } else {
                    Some(b.next)
                };
                self.block = None;
                return Ok(());
            }
        }
        self.block = Some(b);
        Ok(())
    }
    fn prepare_raster(&mut self, f: HdFacts) -> Result<()> {
        let area = f.area.ok_or(Error::Invalid("HD source area absent"))?;
        (self.width, self.height) = area.dimensions()?;
        for axis in [self.width, self.height] {
            if axis > self.limits.max_axis {
                return Err(Error::Limit {
                    requested: axis as usize,
                    maximum: self.limits.max_axis as usize,
                });
            }
        }
        let count = usize::from(self.width)
            .checked_mul(usize::from(self.height))
            .ok_or(Error::Overflow)?;
        if count > self.limits.max_pixels {
            return Err(Error::Limit {
                requested: count,
                maximum: self.limits.max_pixels,
            });
        }
        let fields = f.fields.ok_or(Error::Invalid("HD field facts absent"))?;
        for (i, offset) in [fields.top, fields.bottom].into_iter().enumerate() {
            let offset = usize::try_from(offset).map_err(|_| Error::Overflow)?;
            let rows = if i == 0 {
                self.height.div_ceil(2)
            } else {
                self.height / 2
            };
            if offset < self.header || offset > self.control || (rows > 0 && offset == self.control)
            {
                return Err(Error::Invalid("HD field outside pixel data"));
            }
            self.bit_at[i] = offset.checked_mul(8).ok_or(Error::Overflow)?;
        }
        self.selected = Some(f);
        self.reset_token();
        self.phase = Phase::Reserve;
        self.row = 0;
        self.column = 0;
        self.run_left = 0;
        if let Some(pixels) = self.pixels.as_mut() {
            pixels.clear();
        }
        Ok(())
    }
    fn scan(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if self.block.is_none() {
            let Some(at) = self.next else {
                let mut f = self
                    .selected
                    .ok_or(Error::Unsupported("HD no locally eligible raster"))?;
                f.start_date = self.facts.start_date;
                f.stop_date = self.facts.stop_date;
                f.forced_date = self.facts.forced_date;
                f.menu = self.facts.menu;
                if !f.eight_bit && self.policy == HdPolicy::StrictEightBit {
                    return Err(Error::Unsupported("HD two-bit source raster"));
                }
                self.prepare_raster(f)?;
                return Ok(true);
            };
            let needed = if self.wide { 6 } else { 4 };
            self.header_paid += self.pay(needed - self.header_paid, work)?;
            if self.header_paid != needed {
                return Ok(false);
            }
            if self.controls == self.limits.max_controls {
                return Err(Error::Limit {
                    requested: self.controls.saturating_add(1),
                    maximum: self.limits.max_controls,
                });
            }
            let block = HdPacket::parse(self.raw()?)?.block(at)?;
            self.block = Some(ScanBlock {
                offset: at,
                next: block.next_offset as usize,
                end: block.commands.end,
                at: block.commands.start,
                date: block.date,
                local_area: false,
                local_fields: false,
                forced: false,
            });
            self.controls += 1;
            self.header_paid = 0;
        }
        if let Some(mut operand) = self.operand {
            operand.paid += self.pay(operand.end - operand.start - operand.paid, work)?;
            self.operand = Some(operand);
            if operand.paid < operand.end - operand.start {
                return Ok(false);
            }
            self.operand = None;
            self.apply(operand)?;
            return Ok(true);
        }
        if self.pay(1, work)? == 0 {
            return Ok(false);
        }
        if self.commands == self.limits.max_commands {
            return Err(Error::Limit {
                requested: self.commands.saturating_add(1),
                maximum: self.limits.max_commands,
            });
        }
        self.commands += 1;
        let b = self
            .block
            .as_mut()
            .ok_or(Error::Invalid("HD command block absent"))?;
        if b.at >= b.end {
            return Err(Error::Truncated);
        }
        let at = b.at;
        b.at += 1;
        let opcode = self.raw()?[at];
        let length = operand_len(opcode).ok_or(Error::Unsupported("HD opaque control"))?;
        let mut b = self
            .block
            .ok_or(Error::Invalid("HD command block absent"))?;
        let start = b.at;
        let end = start.checked_add(length).ok_or(Error::Overflow)?;
        if end > b.end {
            return Err(Error::Truncated);
        }
        b.at = end;
        self.block = Some(b);
        let operand = Operand {
            opcode,
            start,
            end,
            paid: 0,
        };
        if length == 0 {
            self.apply(operand)?;
        } else {
            self.operand = Some(operand);
        }
        Ok(true)
    }
    fn reset_token(&mut self) {
        let eight = self.selected.is_none_or(|f| f.eight_bit);
        self.token_stage = if eight { 0 } else { 5 };
        self.bits_needed = if eight { 1 } else { 4 };
        self.bits_read = 0;
        self.value = 0;
        self.nibble_word = 0;
        self.nibbles = 0;
    }
    fn start_run(&mut self, count: u16) -> Result<()> {
        if count == 0 || count > self.width - self.column {
            return Err(Error::Invalid("HD run exceeds source row"));
        }
        self.run_left = count;
        Ok(())
    }
    fn raster(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if self.run_left > 0 {
            let take = self.pay(self.run_left as usize, work)?;
            if take == 0 {
                return Ok(false);
            }
            let p = self
                .pixels
                .as_mut()
                .ok_or(Error::Invalid("HD private pixels absent"))?;
            p.try_resize(
                p.len().checked_add(take).ok_or(Error::Overflow)?,
                self.index,
            )?;
            self.column += take as u16;
            self.run_left -= take as u16;
            if self.run_left == 0 {
                if self.column == self.width {
                    let field = (self.row & 1) as usize;
                    self.bit_at[field] =
                        self.bit_at[field].checked_add(7).ok_or(Error::Overflow)? & !7;
                    self.column = 0;
                    self.row += 1;
                    if self.row == self.height {
                        self.phase = Phase::Ready;
                    }
                }
                self.reset_token();
            }
            return Ok(true);
        }
        if work.remaining() == 0 {
            return Ok(false);
        }
        let field = (self.row & 1) as usize;
        let at = self.bit_at[field];
        if at / 8 >= self.control {
            return Err(Error::Truncated);
        }
        let bit = (self.raw()?[at / 8] >> (7 - at % 8)) & 1;
        if self.pay(1, work)? == 0 {
            return Ok(false);
        }
        self.bit_at[field] = self.bit_at[field].checked_add(1).ok_or(Error::Overflow)?;
        self.value = (self.value << 1) | u16::from(bit);
        self.bits_read += 1;
        if self.bits_read == self.bits_needed {
            let value = self.value;
            self.value = 0;
            self.bits_read = 0;
            match self.token_stage {
                0 => {
                    self.has_run = value != 0;
                    self.token_stage = 1;
                    self.bits_needed = 1;
                }
                1 => {
                    self.token_stage = 2;
                    self.bits_needed = if value == 0 { 2 } else { 8 };
                }
                2 => {
                    self.index = value as u8;
                    if self.has_run {
                        self.token_stage = 3;
                        self.bits_needed = 1;
                    } else {
                        self.start_run(1)?;
                    }
                }
                3 => {
                    self.token_stage = 4;
                    self.bits_needed = if value == 0 { 3 } else { 7 };
                    self.length_bits = self.bits_needed;
                }
                4 => {
                    let count = if self.length_bits == 3 {
                        value + 2
                    } else if value == 0 {
                        self.width - self.column
                    } else {
                        value + 9
                    };
                    self.start_run(count)?;
                }
                5 => {
                    self.nibble_word = (self.nibble_word << 4) | value;
                    self.nibbles += 1;
                    let threshold = match self.nibbles {
                        1 => 4,
                        2 => 16,
                        3 => 64,
                        _ => 0,
                    };
                    if self.nibble_word >= threshold {
                        self.index = (self.nibble_word & 3) as u8;
                        let count = if self.nibble_word < 4 {
                            self.width - self.column
                        } else {
                            self.nibble_word >> 2
                        };
                        self.start_run(count)?;
                    }
                }
                _ => return Err(Error::Invalid("HD token state")),
            }
        }
        Ok(true)
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<HdStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        let allow_allocate = work.remaining() > 0;
        let result = (|| {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                match self.phase {
                    Phase::Ready => return Ok(HdStatus::Ready),
                    Phase::Finished => return Err(Error::Invalid("HD job already finished")),
                    Phase::Scan => {
                        if !self.scan(work)? {
                            return Ok(HdStatus::Yield);
                        }
                    }
                    Phase::Reserve => {
                        if self.pay(1, work)? == 0 {
                            return Ok(HdStatus::Yield);
                        }
                        self.phase = Phase::Allocate;
                    }
                    Phase::Allocate => {
                        if !allow_allocate {
                            return Ok(HdStatus::Yield);
                        }
                        if self.pixels.is_none() {
                            self.pixels = Some(BudgetVec::new(&self.budget));
                        }
                        self.pixels
                            .as_mut()
                            .ok_or(Error::Invalid("HD pixels absent"))?
                            .try_reserve_exact(
                                usize::from(self.width) * usize::from(self.height),
                            )?;
                        self.phase = Phase::Raster;
                    }
                    Phase::Raster => {
                        if !self.raster(work)? {
                            return Ok(HdStatus::Yield);
                        }
                    }
                }
            }
        })();
        match result {
            Err(e @ Error::Backpressure { .. }) => Err(e),
            Err(e) => Err(self.poison(e)),
            value => value,
        }
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<HdSubpicture> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("HD source raster not ready"));
        }
        let buffer = self
            .pixels
            .take()
            .ok_or(Error::Invalid("HD pixels absent"))?;
        let bytes = match buffer.freeze() {
            Ok(v) => v,
            Err(e) => {
                let reason = e.error;
                self.pixels = Some(e.buffer);
                return match reason {
                    Error::Backpressure { .. } => Err(reason),
                    _ => Err(self.poison(reason)),
                };
            }
        };
        let facts = self
            .selected
            .ok_or(Error::Invalid("HD selected facts absent"))?;
        let bitmap = DvdBitmap::from_indices(
            facts.area.ok_or(Error::Invalid("HD area absent"))?,
            bytes,
            &self.budget,
            self.limits.max_pixels,
        )?;
        let packet = self
            .packet
            .take()
            .ok_or(Error::Invalid("HD source absent"))?;
        self.phase = Phase::Finished;
        Ok(HdSubpicture {
            packet,
            bitmap,
            facts,
        })
    }
}

/// The ARGB word of one Y/Cr/Cb entry and its contrast byte.
fn argb(ycrcb: [u8; 3], contrast: u8) -> u32 {
    u32::from(255 - contrast) << 24
        | YcbcrMatrix::BT601.rgb(ycrcb[0], ycrcb[1], ycrcb[2], ChannelRounding::Nearest)
}
#[derive(Debug)]
pub struct HdPaletteJob<'a> {
    ycrcb: &'a [u8],
    contrast: &'a [u8],
    values: [u32; 256],
    at: usize,
    failed: Option<Error>,
    finished: bool,
    alpha4_override: Option<u16>,
}
impl HdPaletteJob<'_> {
    pub fn entries_done(&self) -> usize {
        self.at
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<HdStatus> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            self.failed = Some(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        if self.finished {
            return Err(Error::Invalid("HD palette already finished"));
        }
        while self.at < 256 {
            if cancel.load(Ordering::Relaxed) {
                self.failed = Some(Error::Cancelled);
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(HdStatus::Yield);
            }
            let i = self.at;
            let mut alpha = self.contrast.get(i).map_or(0, |raw| 255 - *raw);
            if i < 4
                && let Some(word) = self.alpha4_override
            {
                alpha = ((word >> (4 * i)) & 15) as u8;
            }
            self.values[i] = argb(
                [
                    self.ycrcb[3 * i],
                    self.ycrcb[3 * i + 1],
                    self.ycrcb[3 * i + 2],
                ],
                255 - alpha,
            );
            self.at += 1;
        }
        Ok(HdStatus::Ready)
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<[u32; 256]> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        if cancel.load(Ordering::Relaxed) {
            self.failed = Some(Error::Cancelled);
            return Err(Error::Cancelled);
        }
        if self.at != 256 || self.finished {
            return Err(Error::Invalid("HD palette not ready"));
        }
        self.finished = true;
        Ok(self.values)
    }
}
