//! One-pass, cooperative views of selected VobSub CodecPrivate fields.
//! All original bytes remain owned; unrelated settings and timestamps are not
//! applied to presentation. RGB words, dimensions and duplicate counts are facts.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct ConfigLimits {
    pub max_bytes: usize,
    pub max_work: usize,
    pub max_canvas_axis: u32,
}

impl Default for ConfigLimits {
    fn default() -> Self {
        Self {
            max_bytes: 65536,
            max_work: 65537,
            max_canvas_axis: 65535,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigStatus {
    Yield,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct VobSubConfig {
    source: Bytes,
    pub canvas: Option<Canvas>,
    pub palette: Option<[u32; 16]>,
    pub custom_colours: Option<bool>,
    pub size_assignments: usize,
    pub palette_assignments: usize,
    pub custom_assignments: usize,
}

impl VobSubConfig {
    pub fn raw(&self) -> &Bytes {
        &self.source
    }

    /// An explicit LSB-indexed interpretation. High bits of the original RGB
    /// words and packed control words are not destroyed. Transparent RGB stays
    /// intact. No palette is guessed when CodecPrivate supplies none.
    pub fn rgba_lsb(&self, palette_word: u16, alpha_word: u16) -> Option<[[u8; 4]; 4]> {
        let palette = self.palette.as_ref()?;
        Some(std::array::from_fn(|index| {
            let slot = usize::from((palette_word >> (index * 4)) & 15);
            let rgb = palette[slot];
            let alpha = (((alpha_word >> (index * 4)) & 15) * 17) as u8;
            [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, alpha]
        }))
    }
}

#[derive(Clone, Copy, Debug)]
enum Key {
    Size,
    Palette,
    Custom,
}

impl Key {
    fn text(self) -> &'static [u8] {
        match self {
            Self::Size => b"size:",
            Self::Palette => b"palette:",
            Self::Custom => b"custom colors:",
        }
    }
}

#[derive(Debug, Default)]
struct SizeState {
    values: [u32; 2],
    has_digits: [bool; 2],
    part: usize,
    ended: bool,
}

impl SizeState {
    fn byte(&mut self, byte: u8, maximum: u32) -> Result<()> {
        match byte {
            b'0'..=b'9' => {
                if self.ended {
                    return Err(Error::Invalid("VobSub split canvas number"));
                }
                let value = self.values[self.part]
                    .checked_mul(10)
                    .and_then(|n| n.checked_add(u32::from(byte - b'0')))
                    .ok_or(Error::Overflow)?;
                if value > maximum {
                    return Err(Error::Limit {
                        requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
                        maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
                    });
                }
                self.values[self.part] = value;
                self.has_digits[self.part] = true;
            }
            b'x' if self.part == 0 && self.has_digits[0] => {
                self.part = 1;
                self.ended = false;
            }
            b' ' | b'\t' | b'\r' => {
                self.ended |= self.has_digits[self.part];
            }
            _ => return Err(Error::Invalid("VobSub canvas syntax")),
        }
        Ok(())
    }
    fn finish(&self) -> Result<Canvas> {
        if self.part != 1 || self.has_digits.contains(&false) {
            return Err(Error::Invalid("VobSub incomplete canvas"));
        }
        Ok(Canvas {
            width: self.values[0],
            height: self.values[1],
        })
    }
}

#[derive(Debug, Default)]
struct PaletteState {
    values: [u32; 16],
    count: usize,
    value: u32,
    digits: u8,
    prefixed: bool,
    ended: bool,
    continuation: bool,
}

impl PaletteState {
    fn commit(&mut self) -> Result<()> {
        if self.digits == 0 || self.count == 16 {
            return Err(Error::Invalid("VobSub palette entry count"));
        }
        self.values[self.count] = self.value;
        self.count += 1;
        self.value = 0;
        self.digits = 0;
        self.prefixed = false;
        self.ended = false;
        Ok(())
    }
    fn byte(&mut self, byte: u8) -> Result<()> {
        if self.continuation {
            return if matches!(byte, b' ' | b'\t' | b'\r') {
                Ok(())
            } else {
                Err(Error::Invalid("VobSub palette continuation suffix"))
            };
        }
        let nibble = match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        if let Some(nibble) = nibble {
            if self.ended || self.digits == 8 {
                return Err(Error::Invalid("VobSub palette word syntax"));
            }
            self.value = self
                .value
                .checked_mul(16)
                .and_then(|n| n.checked_add(u32::from(nibble)))
                .ok_or(Error::Overflow)?;
            self.digits += 1;
        } else {
            match byte {
                b'x' | b'X'
                    if self.digits == 1 && self.value == 0 && !self.prefixed && !self.ended =>
                {
                    self.digits = 0;
                    self.prefixed = true;
                }
                b',' => self.commit()?,
                b' ' | b'\t' | b'\r' => {
                    self.ended |= self.digits != 0;
                }
                b'\\' if self.digits == 0 && self.count != 0 => {
                    self.continuation = true;
                }
                _ => return Err(Error::Invalid("VobSub palette syntax")),
            }
        }
        Ok(())
    }
    fn finish(mut self) -> Result<[u32; 16]> {
        if self.continuation {
            return Err(Error::Truncated);
        }
        self.commit()?;
        if self.count != 16 {
            return Err(Error::Invalid("VobSub palette entry count"));
        }
        Ok(self.values)
    }
}

#[derive(Debug, Default)]
struct CustomState {
    word: [u8; 3],
    count: usize,
    ended: bool,
}

impl CustomState {
    fn byte(&mut self, byte: u8) -> Result<()> {
        if self.ended {
            return Ok(());
        } // The remainder stays in original source.
        if matches!(byte, b' ' | b'\t' | b'\r' | b',') {
            if self.count != 0 {
                self.ended = true;
            }
        } else if self.count < self.word.len() {
            self.word[self.count] = byte.to_ascii_uppercase();
            self.count += 1;
        } else {
            return Err(Error::Invalid("VobSub custom-colour setting"));
        }
        Ok(())
    }
    fn finish(&self) -> Result<bool> {
        match &self.word[..self.count] {
            b"ON" => Ok(true),
            b"OFF" => Ok(false),
            _ => Err(Error::Invalid("VobSub custom-colour setting")),
        }
    }
}

#[derive(Debug, Default)]
enum Line {
    #[default]
    Start,
    Prefix {
        key: Key,
        at: usize,
    },
    Ignore,
    Size(SizeState),
    Palette(PaletteState),
    Custom(CustomState),
}

/// Reads each byte once under caller work. Known fields are committed only at
/// a complete line; later assignments are reported explicitly by their counts.
/// Unknown settings, comments and timestamps remain raw and are never applied.
#[derive(Debug)]
pub struct ConfigJob {
    source: Option<Bytes>,
    at: usize,
    work: usize,
    limits: ConfigLimits,
    line: Line,
    canvas: Option<Canvas>,
    palette: Option<[u32; 16]>,
    custom: Option<bool>,
    counts: [usize; 3],
    ready: bool,
    finished: bool,
    failed: Option<Error>,
}

impl ConfigJob {
    pub fn new(source: Bytes, budget: &Budget, limits: ConfigLimits) -> Result<Self> {
        if !source.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        if source.len() > limits.max_bytes {
            return Err(Error::Limit {
                requested: source.len(),
                maximum: limits.max_bytes,
            });
        }
        Ok(Self {
            source: Some(source),
            at: 0,
            work: 0,
            limits,
            line: Line::Start,
            canvas: None,
            palette: None,
            custom: None,
            counts: [0; 3],
            ready: false,
            finished: false,
            failed: None,
        })
    }
    pub fn work_spent(&self) -> usize {
        self.work
    }
    pub fn bytes_consumed(&self) -> usize {
        self.at
    }
    fn poison(&mut self, error: Error) -> Error {
        self.source = None;
        self.failed = Some(error);
        error
    }

    fn charge(&mut self, work: &mut WorkBudget) -> Result<bool> {
        if work.remaining() == 0 {
            return Ok(false);
        }
        let next = self.work.checked_add(1).ok_or(Error::Overflow)?;
        if next > self.limits.max_work {
            return Err(Error::Limit {
                requested: next,
                maximum: self.limits.max_work,
            });
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        self.work = next;
        Ok(true)
    }

    fn line_end(&mut self, eof: bool) -> Result<()> {
        if let Line::Palette(state) = &mut self.line
            && state.continuation
            && !eof
        {
            state.continuation = false;
            return Ok(());
        }
        match std::mem::take(&mut self.line) {
            Line::Size(state) => {
                self.canvas = Some(state.finish()?);
                self.counts[0] = self.counts[0].checked_add(1).ok_or(Error::Overflow)?;
            }
            Line::Palette(state) => {
                self.palette = Some(state.finish()?);
                self.counts[1] = self.counts[1].checked_add(1).ok_or(Error::Overflow)?;
            }
            Line::Custom(state) => {
                self.custom = Some(state.finish()?);
                self.counts[2] = self.counts[2].checked_add(1).ok_or(Error::Overflow)?;
            }
            _ => (),
        }
        Ok(())
    }

    fn byte(&mut self, byte: u8) -> Result<()> {
        if byte == b'\n' {
            return self.line_end(false);
        }
        match &mut self.line {
            Line::Start => {
                self.line = match byte {
                    b' ' | b'\t' | b'\r' => Line::Start,
                    b's' => Line::Prefix {
                        key: Key::Size,
                        at: 1,
                    },
                    b'p' => Line::Prefix {
                        key: Key::Palette,
                        at: 1,
                    },
                    b'c' => Line::Prefix {
                        key: Key::Custom,
                        at: 1,
                    },
                    _ => Line::Ignore,
                };
            }
            Line::Prefix { key, at } => {
                let expected = key.text();
                if byte != expected[*at] {
                    self.line = Line::Ignore;
                } else {
                    *at += 1;
                    if *at == expected.len() {
                        self.line = match key {
                            Key::Size => Line::Size(SizeState::default()),
                            Key::Palette => Line::Palette(PaletteState::default()),
                            Key::Custom => Line::Custom(CustomState::default()),
                        };
                    }
                }
            }
            Line::Ignore => (),
            Line::Size(state) => state.byte(byte, self.limits.max_canvas_axis)?,
            Line::Palette(state) => state.byte(byte)?,
            Line::Custom(state) => state.byte(byte)?,
        }
        Ok(())
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ConfigStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        match self.poll_inner(work, cancel) {
            Ok(s) => Ok(s),
            Err(e) => Err(self.poison(e)),
        }
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ConfigStatus> {
        if self.ready {
            return Ok(ConfigStatus::Ready);
        }
        if self.finished {
            return Err(Error::Invalid("VobSub configuration already finished"));
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !self.charge(work)? {
                return Ok(ConfigStatus::Yield);
            }
            let input = self
                .source
                .as_ref()
                .ok_or(Error::Invalid("VobSub private source absent"))?;
            if self.at == input.len() {
                self.line_end(true)?;
                self.ready = true;
                return Ok(ConfigStatus::Ready);
            }
            let byte = input[self.at];
            self.at += 1;
            self.byte(byte)?;
        }
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<VobSubConfig> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !self.ready || self.finished {
            return Err(self.poison(Error::Invalid("VobSub configuration not ready")));
        }
        let source = self
            .source
            .take()
            .ok_or(Error::Invalid("VobSub private source absent"))?;
        self.finished = true;
        self.ready = false;
        Ok(VobSubConfig {
            source,
            canvas: self.canvas,
            palette: self.palette,
            custom_colours: self.custom,
            size_assignments: self.counts[0],
            palette_assignments: self.counts[1],
            custom_assignments: self.counts[2],
        })
    }
}
