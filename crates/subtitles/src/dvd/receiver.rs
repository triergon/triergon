//! FFmpeg's `dvdsub` receiver: packets in, presented subpictures out.
//!
//! The decoder reassembles subpicture units across packets, runs the display
//! control sequence table, expands the interlaced 2-bit or 8-bit pixels,
//! resolves the colours and removes transparent borders, with the reference's
//! tolerances at every step (see [`Decoder`]).
use super::wire::{Command, Commands, DvdArea};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

/// The receiver's colour and selection settings: the decoder options and the
/// `palette:` line of the stream configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Sixteen `0xRRGGBB` colours, or `None` for the receiver's automatic greys.
    pub palette: Option<[u32; 16]>,
    /// Drop every subpicture without a forced start (`forced_subs_only`).
    pub forced_only: bool,
}

impl Settings {
    /// Settings from VobSub-style configuration text. Each line beginning
    /// `palette:` replaces the palette; other lines are ignored.
    pub fn from_configuration(text: &[u8]) -> Self {
        let palette = text
            .split(|&b| b == b'\n')
            .filter_map(|line| line.strip_prefix(b"palette:"))
            .next_back()
            .map(parse_colour_list);
        Self {
            palette,
            forced_only: false,
        }
    }

    /// The `palette` option: up to sixteen hexadecimal colours separated by
    /// commas. It takes precedence over the configuration and the IFO palette.
    pub fn apply_palette_option(&mut self, text: &[u8]) {
        self.palette = Some(parse_colour_list(text));
    }

    /// The `ifo_palette` option, given the IFO file's bytes, or `None` when it
    /// could not be read. A file without a usable colour table removes the
    /// palette, as the reference does.
    pub fn apply_ifo(&mut self, ifo: Option<&[u8]>) {
        self.palette = ifo.and_then(ifo_palette);
    }
}

/// Up to sixteen hexadecimal numbers separated by commas, each keeping its low
/// 24 bits; the list stops at the first entry without digits and the remaining
/// colours are black.
fn parse_colour_list(text: &[u8]) -> [u32; 16] {
    let mut colours = [0; 16];
    let mut rest = text;
    for colour in &mut colours {
        rest = rest.trim_ascii_start();
        let digits = rest
            .strip_prefix(b"0x")
            .or_else(|| rest.strip_prefix(b"0X"))
            .filter(|tail| tail.first().is_some_and(u8::is_ascii_hexdigit))
            .unwrap_or(rest);
        let length = digits.iter().take_while(|b| b.is_ascii_hexdigit()).count();
        if length == 0 {
            break;
        }
        let value = digits[..length].iter().fold(0_u64, |value, &digit| {
            let nibble = char::from(digit).to_digit(16).unwrap_or(0);
            value.saturating_mul(16).saturating_add(u64::from(nibble))
        });
        *colour = (value & 0x00ff_ffff) as u32;
        rest = digits[length..].trim_ascii_start();
        rest = rest.strip_prefix(b",").unwrap_or(rest);
    }
    colours
}

/// The colour table of the first program chain of a DVD-Video title set IFO
/// (`DVDVIDEO-VTS`), converted to RGB as the reference does.
pub fn ifo_palette(ifo: &[u8]) -> Option<[u32; 16]> {
    if ifo.get(..12)? != b"DVDVIDEO-VTS" {
        return None;
    }
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(
            ifo.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let table = usize::try_from(word(0xcc)?).ok()?.checked_mul(0x800)?;
    let first = table.checked_add(usize::try_from(word(table.checked_add(12)?)?).ok()?)?;
    let clut = ifo.get(first.checked_add(0xa4)?..first.checked_add(0xe4)?)?;
    let mut colours = [0; 16];
    for (colour, entry) in colours.iter_mut().zip(clut.chunks_exact(4)) {
        *colour = YcbcrRounding::Nearest.rgb(entry[1], entry[2], entry[3]);
    }
    Some(colours)
}

/// The rounding of the reference's two Y'CbCr-to-RGB conversions: BT.601
/// studio range to full-range RGB in 10-bit fixed point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YcbcrRounding {
    /// Round to nearest: the `ifo_palette` option.
    Nearest,
    /// Half a step below nearest: the MP4 demuxer's palette text.
    HalfDown,
}

impl YcbcrRounding {
    /// `0xRRGGBB` for one colour-table entry.
    pub fn rgb(self, y: u8, cr: u8, cb: u8) -> u32 {
        const fn fix(value: f64) -> i32 {
            (value * 1024.0 + 0.5) as i32
        }
        let bias = match self {
            Self::Nearest => 512,
            Self::HalfDown => -512,
        };
        let luma = (i32::from(y) - 16) * fix(255.0 / 219.0) + bias;
        let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
        let channel = |chroma: i32| ((luma + chroma) >> 10).clamp(0, 255) as u32;
        let r = channel(fix(1.402 * 255.0 / 224.0) * cr);
        let g = channel(-fix(0.344136 * 255.0 / 224.0) * cb - fix(0.714136 * 255.0 / 224.0) * cr);
        let b = channel(fix(1.772 * 255.0 / 224.0) * cb);
        r << 16 | g << 8 | b
    }
}

/// Bounds of one [`Decoder`].
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Largest subpicture unit, in bytes, including what reassembly buffers.
    pub max_unit: usize,
    /// Largest decoded area, in pixels.
    pub max_pixels: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_unit: 1 << 24,
            max_pixels: 16 << 20,
        }
    }
}

/// One presented subpicture. Times are milliseconds after the packet's PTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subtitle<'a> {
    /// The last start command's date; 0 without one.
    pub start_ms: u32,
    /// The last stop command's date; 0 without one.
    pub end_ms: u32,
    /// A forced start preceded the displayed bitmap.
    pub forced: bool,
    pub bitmap: Bitmap<'a>,
}

impl Subtitle<'_> {
    /// The window the reference reports for a packet `duration_ms` long: a
    /// subtitle without an end (0) ends when its packet does.
    #[must_use]
    pub fn ending_with_packet(self, duration_ms: u32) -> Self {
        Self {
            end_ms: if self.end_ms == 0 {
                duration_ms
            } else {
                self.end_ms
            },
            ..self
        }
    }
}

/// The displayed rectangle: its ARGB colours (4, or 256 for eight-bit
/// pixels) and `height` rows of `width` colour indices, `stride` bytes apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bitmap<'a> {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub palette: &'a [u32],
    pub pixels: &'a [u8],
    pub stride: usize,
}

/// What [`Decoder::decode`] did with a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status<'a> {
    /// The packet completed a unit that presents this subpicture.
    Subtitle(Subtitle<'a>),
    /// Nothing to present: the packet was buffered, dropped, or its unit shows
    /// no visible pixels.
    Nothing,
    /// The work budget ran out. Nothing was consumed but the work, which counts
    /// towards the next attempt: decode the same packet again. A unit needs its
    /// bytes plus the pixels of every area it draws. Running out of memory
    /// ([`Error::Backpressure`]) consumes nothing either.
    Yield,
}

/// FFmpeg's `dvdsub` decoder.
///
/// * **Reassembly.** A packet of at least ten bytes starts a unit: a 16-bit
///   size and control offset, or (after a zero word) 32-bit ones, which also
///   widen the sequence links. The unit is complete once it holds its declared
///   size or the first sequence header (4 bytes, or 6 when wide). An
///   incomplete start whose control offset lies beyond its size, and a shorter
///   packet, are dropped; later packets are appended until the unit completes,
///   and bytes after it are discarded. Every structure is read from all of the
///   unit's bytes, regardless of the declared size.
/// * **Control.** Sequences are followed while the link moves forward. A
///   sequence ends at `0xff`, at an unknown command (including `CHG_COLCON`)
///   or at the end of the unit; truncated operands drop the unit. Every
///   sequence with both a display area and field offsets (and at least two
///   rows) redraws the bitmap with the colours set so far; a failed redraw
///   drops the unit after the rest of it has run. The last start and stop
///   dates give the display window.
/// * **Colours.** The colour selectors and the contrast of each colour carry
///   over from unit to unit, even from units that present nothing. `0x04`
///   sets the first four contrasts to its raw nibbles and `0x84` all 256 to
///   255 minus its bytes. Four-colour bitmaps take the supplied palette (or
///   automatic greys) with seventeen times the contrast as alpha; an
///   extended area (`0x85`) anywhere in the unit makes its bitmaps eight-bit,
///   with the unit's own `0x83` colours, converted with
///   [`YcbcrRounding::Nearest`], and the contrast as alpha.
/// * **Pixels.** Two-bit fields are read from their offset to the end of the
///   unit, zero beyond it; an offset within four bytes of the end drops the
///   unit. An eight-bit code may run past the end, but must start before it.
///   A run past the end of a line drops the unit.
/// * **Presentation.** A forced start anywhere in the unit keeps the whole
///   area (a menu); otherwise rows and columns of fully transparent pixels are
///   removed, and a fully transparent bitmap presents nothing.
#[derive(Debug)]
pub struct Decoder {
    settings: Settings,
    limits: Limits,
    pending: BudgetVec<u8>,
    plane: BudgetVec<u8>,
    colours: Colours,
    palette: [u32; 256],
    /// How to undo the last decode until the next one starts.
    last: Option<Undo>,
    /// Work granted to attempts at the current packet that yielded.
    credit: u64,
}

/// The state before a decode: its colours, the length of the reassembly
/// buffer, and whether the decode completed a buffered unit (which the next
/// decode then discards).
#[derive(Clone, Copy, Debug)]
struct Undo {
    colours: Colours,
    pending: usize,
    completed: bool,
}

/// The colour state that carries over between units.
#[derive(Clone, Copy, Debug)]
struct Colours {
    /// Palette entries of pixel values 0 to 3.
    selectors: [u8; 4],
    /// Each colour's contrast: 0 is transparent.
    contrast: [u8; 256],
}

impl Default for Colours {
    fn default() -> Self {
        Self {
            selectors: [0; 4],
            contrast: [0; 256],
        }
    }
}

impl Decoder {
    pub fn new(budget: &Budget, settings: Settings, limits: Limits) -> Self {
        Self {
            settings,
            limits,
            pending: BudgetVec::new(budget),
            plane: BudgetVec::new(budget),
            colours: Colours::default(),
            palette: [0; 256],
            last: None,
            credit: 0,
        }
    }

    /// Undo the last [`decode`](Self::decode), so that the same packet can be
    /// decoded again: for a consumer that could not store its subtitle.
    pub fn rewind(&mut self) -> Result<()> {
        if let Some(undo) = self.last.take() {
            self.colours = undo.colours;
            self.pending.try_resize(undo.pending, 0)?;
        }
        Ok(())
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Decode one packet. An empty packet presents nothing.
    pub fn decode(&mut self, packet: &[u8], work: &mut WorkBudget) -> Result<Status<'_>> {
        if self.last.take().is_some_and(|undo| undo.completed) {
            self.pending.clear();
        }
        let resumed = self.pending.len();
        let length = resumed + packet.len();
        if length > self.limits.max_unit {
            return Err(Error::Limit {
                requested: length,
                maximum: self.limits.max_unit,
            });
        }
        let before = Undo {
            colours: self.colours,
            pending: resumed,
            completed: resumed != 0,
        };
        self.last = Some(Undo {
            completed: false,
            ..before
        });
        let (unit, header) = if resumed == 0 {
            let Some(header) = Header::read(packet) else {
                return Ok(Status::Nothing);
            };
            if !header.complete(packet.len()) {
                if header.control <= header.size {
                    self.pending.try_extend_from_slice(packet)?;
                }
                return Ok(Status::Nothing);
            }
            (packet, header)
        } else {
            self.pending.try_extend_from_slice(packet)?;
            match Header::read(self.pending.as_slice()) {
                Some(header) if header.complete(length) => (self.pending.as_slice(), header),
                _ => return Ok(Status::Nothing),
            }
        };
        self.last = Some(before);
        let granted = work.remaining();
        let mut banked = WorkBudget::new(granted.saturating_add(self.credit));
        let mut pass = Pass {
            settings: &self.settings,
            limits: self.limits,
            colours: &mut self.colours,
            palette: &mut self.palette,
            plane: &mut self.plane,
            work: &mut banked,
        };
        let outcome = pass.run(unit, header);
        if matches!(outcome, Err(Halt::Yield)) {
            work.take(granted);
            self.credit = self.credit.saturating_add(granted);
        } else {
            work.take(banked.spent().saturating_sub(self.credit));
            self.credit = 0;
        }
        if let Err(Halt::Yield | Halt::Failed(Error::Backpressure { .. })) = outcome {
            self.rewind()?;
        }
        match outcome {
            Ok(shown) => Ok(Status::Subtitle(
                shown.subtitle(&self.palette, self.plane.as_slice()),
            )),
            Err(Halt::Malformed) => Ok(Status::Nothing),
            Err(Halt::Yield) => Ok(Status::Yield),
            Err(Halt::Failed(error)) => Err(error),
        }
    }
}

/// The leading words of a unit.
#[derive(Clone, Copy, Debug)]
struct Header {
    size: usize,
    control: usize,
    /// 32-bit size, control offset and sequence links.
    wide: bool,
}

impl Header {
    /// `None` for a packet the receiver does not start a unit with.
    fn read(unit: &[u8]) -> Option<Self> {
        let [s0, s1, s2, s3, s4, s5, s6, s7, s8, s9, ..] = *unit else {
            return None;
        };
        let word = |a: u8, b: u8| usize::from(u16::from_be_bytes([a, b]));
        let long = |b: [u8; 4]| u32::from_be_bytes(b) as usize;
        Some(match word(s0, s1) {
            0 => Self {
                size: long([s2, s3, s4, s5]),
                control: long([s6, s7, s8, s9]),
                wide: true,
            },
            size => Self {
                size,
                control: word(s2, s3),
                wide: false,
            },
        })
    }

    /// The bytes of a sequence's date and link.
    fn sequence_header(self) -> usize {
        if self.wide { 6 } else { 4 }
    }

    fn complete(self, length: usize) -> bool {
        length >= self.size || length >= self.control.saturating_add(self.sequence_header())
    }
}

/// Why a unit presents nothing.
#[derive(Clone, Copy, Debug)]
enum Halt {
    /// Nothing visible, or operands or a run past the end of the unit or line.
    Malformed,
    /// The work budget ran out.
    Yield,
    Failed(Error),
}

impl From<Error> for Halt {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

/// A presented subpicture whose colours and pixels sit in the decoder.
#[derive(Clone, Copy, Debug)]
struct Shown {
    start_ms: u32,
    end_ms: u32,
    forced: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    colours: usize,
    offset: usize,
    stride: usize,
}

impl Shown {
    fn subtitle<'a>(self, palette: &'a [u32; 256], plane: &'a [u8]) -> Subtitle<'a> {
        Subtitle {
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            forced: self.forced,
            bitmap: Bitmap {
                x: self.x,
                y: self.y,
                width: self.width,
                height: self.height,
                palette: &palette[..self.colours],
                pixels: &plane[self.offset..],
                stride: self.stride,
            },
        }
    }
}

/// What a unit's commands have set so far.
#[derive(Clone, Copy, Debug, Default)]
struct UnitState<'a> {
    start: u16,
    stop: u16,
    forced: bool,
    /// An extended area made the unit's bitmaps eight-bit.
    eight_bit: bool,
    /// The unit's last `0x83` colours.
    extended_palette: Option<&'a [u8; 768]>,
}

/// The last bitmap drawn.
#[derive(Clone, Copy, Debug)]
struct Drawn {
    area: DvdArea,
    width: usize,
    height: usize,
    colours: usize,
    /// The rectangle of visible pixels, if any.
    visible: Option<Crop>,
    forced: bool,
}

/// The field offsets of a sequence, of either width.
#[derive(Clone, Copy, Debug)]
struct Fields {
    top: usize,
    bottom: usize,
}

/// One run of a unit through its control sequences.
struct Pass<'d, 'w> {
    settings: &'d Settings,
    limits: Limits,
    colours: &'d mut Colours,
    palette: &'d mut [u32; 256],
    plane: &'d mut BudgetVec<u8>,
    work: &'w mut WorkBudget,
}

impl Pass<'_, '_> {
    fn run(&mut self, unit: &[u8], header: Header) -> std::result::Result<Shown, Halt> {
        if !self.work.try_charge(unit.len() as u64) {
            return Err(Halt::Yield);
        }
        let mut state = UnitState::default();
        let mut drawn = None;
        let mut failed = false;
        let mut at = header.control;
        let link_bytes = header.sequence_header() - 2;
        while let Some(head) = unit.get(at..at + header.sequence_header()) {
            let date = u16::from_be_bytes([head[0], head[1]]);
            let next = head[2..]
                .iter()
                .fold(0_usize, |n, &b| n << 8 | usize::from(b));
            let body = &unit[at + 2 + link_bytes..];
            let (mut area, mut fields) = (None, None);
            let mut commands = Commands::resume(body, 0, 0, usize::MAX);
            while commands.position() < body.len() {
                let command = commands.next().ok_or(Halt::Malformed)?;
                match command.map_err(|_| Halt::Malformed)? {
                    Command::ForcedStart => state.forced = true,
                    Command::Start => state.start = date,
                    Command::Stop => state.stop = date,
                    Command::Palette(word) => {
                        self.colours.selectors =
                            std::array::from_fn(|i| (word >> (4 * i) & 15) as u8);
                    }
                    Command::Alpha(word) => {
                        for i in 0..4 {
                            self.colours.contrast[i] = (word >> (4 * i) & 15) as u8;
                        }
                    }
                    Command::ExtendedContrast(values) => {
                        for (alpha, value) in self.colours.contrast.iter_mut().zip(values) {
                            *alpha = 255 - value;
                        }
                    }
                    Command::ExtendedPalette(colours) => state.extended_palette = Some(colours),
                    Command::Area(value) => area = Some(value),
                    Command::ExtendedArea(value) => {
                        area = Some(value);
                        state.eight_bit = true;
                    }
                    Command::Fields(value) => {
                        fields = Some(Fields {
                            top: usize::from(value.top),
                            bottom: usize::from(value.bottom),
                        });
                    }
                    Command::ExtendedFields { top, bottom } => {
                        fields = Some(Fields {
                            top: top as usize,
                            bottom: bottom as usize,
                        });
                    }
                    Command::End | Command::Opaque { .. } => break,
                }
            }
            if let (Some(area), Some(fields)) = (area, fields) {
                match self.draw(unit, area, fields, &state) {
                    Ok(Some(next)) => drawn = Some(next),
                    Ok(None) => {}
                    Err(Halt::Malformed) => failed = true,
                    Err(halt) => return Err(halt),
                }
            }
            if next <= at {
                break;
            }
            at = next;
        }
        let drawn = drawn.filter(|_| !failed).ok_or(Halt::Malformed)?;
        if self.settings.forced_only && !drawn.forced {
            return Err(Halt::Malformed);
        }
        let (width, stride) = (drawn.width, drawn.width + 16);
        let crop = if state.forced {
            Crop {
                x: 0,
                y: 0,
                width,
                height: drawn.height,
            }
        } else {
            drawn.visible.ok_or(Halt::Malformed)?
        };
        let milliseconds = |date: u16| u32::from(date) * 1024 / 90;
        // Crops lie within the drawn area, whose sides fit in 12 bits.
        let narrow = |value: usize| value as u16;
        Ok(Shown {
            start_ms: milliseconds(state.start),
            end_ms: milliseconds(state.stop),
            forced: drawn.forced,
            x: drawn.area.left + narrow(crop.x),
            y: drawn.area.top + narrow(crop.y),
            width: narrow(crop.width),
            height: narrow(crop.height),
            colours: drawn.colours,
            offset: crop.y * stride + crop.x,
            stride,
        })
    }

    /// Resolve the colours and expand one area into the plane; `None` for an
    /// area the receiver skips (reversed, or a single row).
    fn draw(
        &mut self,
        unit: &[u8],
        area: DvdArea,
        fields: Fields,
        state: &UnitState<'_>,
    ) -> std::result::Result<Option<Drawn>, Halt> {
        let Ok((width, height)) = area.dimensions() else {
            return Ok(None);
        };
        let (width, height) = (usize::from(width), usize::from(height));
        if height < 2 {
            return Ok(None);
        }
        let pixels = width * height;
        if pixels > self.limits.max_pixels {
            return Err(Halt::Failed(Error::Limit {
                requested: pixels,
                maximum: self.limits.max_pixels,
            }));
        }
        if !self.work.try_charge(pixels as u64) {
            return Err(Halt::Yield);
        }
        let colours = if state.eight_bit {
            let table = state.extended_palette.ok_or(Halt::Malformed)?;
            for ((argb, yuv), alpha) in self
                .palette
                .iter_mut()
                .zip(table.chunks_exact(3))
                .zip(self.colours.contrast)
            {
                *argb = u32::from(alpha) << 24 | YcbcrRounding::Nearest.rgb(yuv[0], yuv[1], yuv[2]);
            }
            256
        } else {
            let contrast = std::array::from_fn(|i| self.colours.contrast[i]);
            let four = four_colours(
                self.settings.palette.as_ref(),
                self.colours.selectors,
                contrast,
            );
            self.palette[..4].copy_from_slice(&four);
            4
        };
        let mut visible = [false; 256];
        for (seen, argb) in visible.iter_mut().zip(&self.palette[..colours]) {
            *seen = argb >> 24 != 0;
        }
        let offsets = [fields.top, fields.bottom];
        if !state.eight_bit
            && offsets
                .iter()
                .any(|&offset| offset.saturating_add(4) > unit.len())
        {
            return Err(Halt::Malformed);
        }
        // Each row has room for a short run's 16-byte store past its end.
        let stride = width + 16;
        // The plane only grows: every pixel shown is written before it is read.
        if self.plane.len() < stride * height {
            self.plane.try_resize(stride * height, 0)?;
        }
        let rows = &mut self.plane.as_mut_slice()[..stride * height];
        let [top, bottom] = offsets;
        let bounds = if state.eight_bit {
            fill_fields(
                rows,
                width,
                &visible,
                [
                    EightBitRuns::new(unit, top),
                    EightBitRuns::new(unit, bottom),
                ],
            )?
        } else {
            fill_fields(
                rows,
                width,
                &visible,
                [TwoBitRuns::new(unit, top), TwoBitRuns::new(unit, bottom)],
            )?
        };
        Ok(Some(Drawn {
            area,
            width,
            height,
            colours,
            visible: bounds.crop(),
            forced: state.forced,
        }))
    }
}

/// The four colours of a two-bit bitmap: the supplied palette's selected
/// entries, or without one the receiver's automatic greys, with alpha
/// seventeen times the contrast (modulo 256).
///
/// The greys rank the distinct selectors of the visible pixel values in pixel
/// order: for `m` of them the levels are the nearest integers to
/// `255 * rank / (m - 1)` (255 for a single one), scaled by 255/256.
fn four_colours(source: Option<&[u32; 16]>, selectors: [u8; 4], contrast: [u8; 4]) -> [u32; 4] {
    let alpha = contrast.map(|c| u32::from(c.wrapping_mul(17)) << 24);
    if let Some(rgb) = source {
        return std::array::from_fn(|i| rgb[usize::from(selectors[i])] & 0x00ff_ffff | alpha[i]);
    }
    let mut rank = [None; 16];
    let mut classes = 0_u32;
    for i in 0..4 {
        let slot = &mut rank[usize::from(selectors[i])];
        if contrast[i] != 0 && slot.is_none() {
            *slot = Some(classes);
            classes += 1;
        }
    }
    std::array::from_fn(|i| {
        if contrast[i] == 0 {
            return 0;
        }
        let rank = rank[usize::from(selectors[i])].unwrap_or(0);
        let level = if classes == 1 {
            255
        } else {
            (255 * rank + (classes - 1) / 2) / (classes - 1)
        };
        alpha[i] | ((255 * level / 256) * 0x0001_0101)
    })
}

/// A source of runs for one line: the count (0 for the rest of the line) and
/// colour of the next run, then a byte boundary at the end of the line.
trait Runs: Copy {
    fn next_run(&mut self) -> std::result::Result<(usize, u8), Halt>;
    fn end_line(&mut self);
}

/// Sixteen pixels of each colour, copied rather than filled so that a short
/// run is one load and one store. Aligned, so that no load straddles a line.
#[repr(align(16))]
struct Splats([[u8; 16]; 256]);

static SPLATS: Splats = {
    let mut table = [[0; 16]; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = [i as u8; 16];
        i += 1;
    }
    Splats(table)
};

/// Expand the two interlaced fields into `rows` (each `width` pixels and 16
/// more), returning the bounds of the visible pixels. A top and a bottom line
/// are decoded together: their runs depend only on their own field, so the
/// two dependency chains overlap.
fn fill_fields(
    rows: &mut [u8],
    width: usize,
    visible: &[bool; 256],
    mut fields: [impl Runs; 2],
) -> std::result::Result<Bounds, Halt> {
    let mut bounds = Bounds::default();
    for (pair, lines) in rows.chunks_mut(2 * (width + 16)).enumerate() {
        let (top, bottom) = lines.split_at_mut(lines.len().min(width + 16));
        let [first, second] = &mut fields;
        let spans = if bottom.is_empty() {
            [fill_lines([top], width, visible, [first])?[0], None]
        } else {
            fill_lines([top, bottom], width, visible, [first, second])?
        };
        bounds.add_line(2 * pair, spans[0]);
        bounds.add_line(2 * pair + 1, spans[1]);
    }
    Ok(bounds)
}

/// One line of each of `N` fields, decoded in step. Returns each line's first
/// and last column of a visible colour; a run past the end of a line fails.
// A small function of its own keeps the loop's state in registers.
#[inline(never)]
fn fill_lines<R: Runs, const N: usize>(
    rows: [&mut [u8]; N],
    width: usize,
    visible: &[bool; 256],
    sources: [&mut R; N],
) -> std::result::Result<[Option<(usize, usize)>; N], Halt> {
    let mut runs: [R; N] = std::array::from_fn(|i| *sources[i]);
    let mut lines = [Line::default(); N];
    while lines.iter().any(|line| line.x < width) {
        for i in 0..N {
            if lines[i].x < width {
                lines[i].run(rows[i], width, visible, &mut runs[i])?;
            }
        }
    }
    for (source, mut runs) in sources.into_iter().zip(runs) {
        runs.end_line();
        *source = runs;
    }
    Ok(lines.map(Line::span))
}

/// The progress of one line.
#[derive(Clone, Copy, Debug)]
struct Line {
    x: usize,
    first: usize,
    last: usize,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            x: 0,
            first: usize::MAX,
            last: 0,
        }
    }
}

impl Line {
    #[inline(always)]
    fn run(
        &mut self,
        row: &mut [u8],
        width: usize,
        visible: &[bool; 256],
        runs: &mut impl Runs,
    ) -> std::result::Result<(), Halt> {
        let (count, colour) = runs.next_run()?;
        let x = self.x;
        let run = if count == 0 { width - x } else { count };
        let end = x + run;
        // Short runs are the common case: one store of sixteen bytes, which
        // later runs of the line (or the row's slack) absorb, beats a fill.
        if run <= 16 && end <= width {
            row[x..x + 16].copy_from_slice(&SPLATS.0[usize::from(colour)]);
        } else if end <= width {
            row[x..end].fill(colour);
        } else {
            return Err(Halt::Malformed);
        }
        // Selects rather than a branch: colours alternate too irregularly to
        // predict.
        let seen = visible[usize::from(colour)];
        self.first = self.first.min(if seen { x } else { usize::MAX });
        self.last = if seen { end - 1 } else { self.last };
        self.x = end;
        Ok(())
    }

    fn span(self) -> Option<(usize, usize)> {
        (self.first != usize::MAX).then_some((self.first, self.last))
    }
}

/// A big-endian bit cursor over a unit, reading zero past its end.
#[derive(Clone, Copy)]
struct BitCursor<'a> {
    unit: &'a [u8],
    /// The next byte to load.
    next: usize,
    /// Loaded bits, most significant first.
    cache: u64,
    bits: u32,
}

impl<'a> BitCursor<'a> {
    fn new(unit: &'a [u8], byte: usize) -> Self {
        Self {
            unit,
            next: byte,
            cache: 0,
            bits: 0,
        }
    }

    /// At least 16 bits in the cache.
    #[inline]
    fn fill(&mut self) {
        if self.bits >= 16 {
            return;
        }
        if let Some(&word) = self
            .unit
            .get(self.next..)
            .and_then(<[u8]>::first_chunk::<4>)
        {
            self.cache |= u64::from(u32::from_be_bytes(word)) << (32 - self.bits);
            self.bits += 32;
            self.next += 4;
        } else {
            self.fill_past_end();
        }
    }

    #[cold]
    fn fill_past_end(&mut self) {
        while self.bits <= 56 {
            let byte = self.unit.get(self.next).copied().unwrap_or(0);
            self.cache |= u64::from(byte) << (56 - self.bits);
            self.bits += 8;
            self.next += 1;
        }
    }

    /// The next 16 bits, not consumed.
    fn peek(&mut self) -> u16 {
        self.fill();
        (self.cache >> 48) as u16
    }

    fn skip(&mut self, count: u32) {
        self.cache <<= count;
        self.bits -= count;
    }

    /// Up to 16 bits.
    fn take(&mut self, count: u32) -> u16 {
        let value = self.peek() >> (16 - count);
        self.skip(count);
        value
    }

    /// Bits consumed from the start of the unit.
    fn position(&self) -> usize {
        self.next.saturating_mul(8) - self.bits as usize
    }

    /// Skip to the next byte boundary (loads are whole bytes).
    fn align(&mut self) {
        self.skip(self.bits % 8);
    }
}

/// Two-bit runs: 4, 8, 12 or 16-bit codes, a count above two colour bits,
/// one code nibble per two leading zero bits.
#[derive(Clone, Copy)]
struct TwoBitRuns<'a>(BitCursor<'a>);

impl<'a> TwoBitRuns<'a> {
    fn new(unit: &'a [u8], byte: usize) -> Self {
        Self(BitCursor::new(unit, byte))
    }
}

impl Runs for TwoBitRuns<'_> {
    fn next_run(&mut self) -> std::result::Result<(usize, u8), Halt> {
        let c = &mut self.0;
        c.fill();
        // A marker bit caps the leading zeros at 6: at most four nibbles.
        let bits = ((c.cache | 1 << 57).leading_zeros() & !1) * 2 + 4;
        let code = (c.cache >> (64 - bits)) as u16;
        c.skip(bits);
        Ok((usize::from(code >> 2), (code & 3) as u8))
    }

    fn end_line(&mut self) {
        self.0.align();
    }
}

/// Eight-bit runs: a run flag and a colour-width flag, a 2 or 8-bit colour,
/// and for runs a 3-bit (2 to 9) or 7-bit (10 to 136, 0 for the rest of the
/// line) count. A code must start before the end of the unit.
#[derive(Clone, Copy)]
struct EightBitRuns<'a> {
    cursor: BitCursor<'a>,
    end: usize,
}

impl<'a> EightBitRuns<'a> {
    fn new(unit: &'a [u8], byte: usize) -> Self {
        Self {
            cursor: BitCursor::new(unit, byte),
            end: unit.len() * 8,
        }
    }
}

impl Runs for EightBitRuns<'_> {
    fn next_run(&mut self) -> std::result::Result<(usize, u8), Halt> {
        let c = &mut self.cursor;
        if c.position() >= self.end {
            return Err(Halt::Malformed);
        }
        let flags = c.take(2);
        let colour = c.take(if flags & 1 == 0 { 2 } else { 8 }) as u8;
        if flags & 2 == 0 {
            return Ok((1, colour));
        }
        let count = if c.take(1) == 0 {
            usize::from(c.take(3)) + 2
        } else {
            match usize::from(c.take(7)) {
                0 => 0,
                n => n + 9,
            }
        };
        Ok((count, colour))
    }

    fn end_line(&mut self) {
        self.cursor.align();
    }
}

#[derive(Clone, Copy, Debug)]
struct Crop {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// The smallest rectangle holding every visible pixel seen so far.
#[derive(Clone, Copy, Debug, Default)]
struct Bounds {
    rows: Option<(usize, usize)>,
    left: usize,
    right: usize,
}

impl Bounds {
    fn add_line(&mut self, y: usize, span: Option<(usize, usize)>) {
        let Some((first, last)) = span else {
            return;
        };
        match &mut self.rows {
            Some((top, bottom)) => {
                *top = (*top).min(y);
                *bottom = (*bottom).max(y);
                self.left = self.left.min(first);
                self.right = self.right.max(last);
            }
            None => {
                *self = Self {
                    rows: Some((y, y)),
                    left: first,
                    right: last,
                }
            }
        }
    }

    fn crop(self) -> Option<Crop> {
        self.rows.map(|(top, bottom)| Crop {
            x: self.left,
            y: top,
            width: self.right - self.left + 1,
            height: bottom - top + 1,
        })
    }
}
