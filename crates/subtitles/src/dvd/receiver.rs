//! FFmpeg's `dvdsub` receiver: packets in, presented subpictures out.
//!
//! The decoder reassembles subpicture units across packets, runs the display
//! control sequence table, expands the interlaced 2-bit or 8-bit pixels,
//! resolves the colours and removes transparent borders, with the reference's
//! tolerances at every step (see [`Decoder`]).
use super::c_text::{is_space, scan_int, strtoul16};
use super::wire::{Command, Commands, DvdArea};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use triergon_pixel::{ChannelRounding, YcbcrMatrix};

/// The receiver's colour and selection settings and its canvas: the decoder
/// options and the `palette:` and `size:` lines of the stream configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Sixteen `0xRRGGBB` colours, or `None` for the receiver's automatic greys.
    pub palette: Option<[u32; 16]>,
    /// The width and height the subpictures are placed on, from the last
    /// `size:` line; `None` without one, when the container's size applies.
    pub canvas: Option<[u32; 2]>,
    /// Drop every subpicture without a forced start (`forced_subs_only`).
    pub forced_only: bool,
}

impl Settings {
    /// Settings from VobSub-style configuration text, read as the reference
    /// reads it: up to the first NUL, a line at a time (CR and LF both end a
    /// line). A line beginning `palette:` replaces the palette and one
    /// beginning `size:` the canvas; other lines are ignored. Both values are
    /// scanned from the rest of the text, so they can continue on the next
    /// line. A `size:` line without two numbers is ignored; its numbers are
    /// converted with a 32-bit `long`, as on 32-bit ARMv7, so one beyond
    /// `int`'s range saturates and the size is refused.
    ///
    /// # Errors
    ///
    /// `Error::Invalid` for a size the reference refuses to open a decoder
    /// with: a dimension of zero or less, or a picture too large to address.
    pub fn from_configuration(text: &[u8]) -> Result<Self> {
        // Every `palette:` line replaces the whole palette, so only the last
        // one is read. The scans stop at a NUL by themselves.
        let mut palette = None;
        let mut canvas = None;
        let mut line = |start: usize| -> Result<()> {
            let rest = &text[start..];
            match rest.first() {
                Some(b'p') => palette = rest.strip_prefix(b"palette:").or(palette),
                Some(b's') => {
                    if let Some(size) = rest.strip_prefix(b"size:").and_then(scan_size) {
                        canvas = Some(checked_canvas(size)?);
                    }
                }
                _ => {}
            }
            Ok(())
        };
        // Lines start after each LF or CR. A word without a byte below 0x0e
        // has neither, nor a NUL, and is passed over whole.
        let mut start = 0;
        let mut at = 0;
        while at < text.len() {
            let end = text.len().min(at + 8);
            let chunk = &text[at..end];
            if let Ok(&word) = <&[u8; 8]>::try_from(chunk)
                && below_0e(u64::from_le_bytes(word)) == 0
            {
                at = end;
                continue;
            }
            for (offset, &byte) in chunk.iter().enumerate() {
                if matches!(byte, b'\n' | b'\r' | 0) {
                    line(start)?;
                    if byte == 0 {
                        return Ok(Self::configured(palette, canvas));
                    }
                    start = at + offset + 1;
                }
            }
            at = end;
        }
        line(start)?;
        Ok(Self::configured(palette, canvas))
    }

    fn configured(palette: Option<&[u8]>, canvas: Option<[u32; 2]>) -> Self {
        Self {
            palette: palette.map(parse_colour_list),
            canvas,
            forced_only: false,
        }
    }

    /// The `palette` option: up to sixteen hexadecimal colours separated by
    /// commas. It takes precedence over the configuration and the IFO palette.
    /// Each number is read as C's `strtoul` reads it with a 32-bit `unsigned
    /// long`, as on 32-bit ARMv7, and keeps its low 24 bits: a number beyond
    /// 32 bits is white, a negative one is negated modulo 2^32. A `palette:`
    /// line of the configuration is read the same way.
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

/// Non-zero when some byte of `word` is below 0x0e: subtracting 0x0e from
/// each byte then borrows into its top bit while the byte's own top bit is
/// clear.
fn below_0e(word: u64) -> u64 {
    const ONES: u64 = u64::from_le_bytes([0x01; 8]);
    const HIGHS: u64 = u64::from_le_bytes([0x80; 8]);
    word.wrapping_sub(ONES * 0x0e) & !word & HIGHS
}

/// Sixteen colours as the reference reads a colour list: each number
/// converted as `strtoul` does in base 16 (after white space, with an optional
/// sign and `0x`) with a 32-bit `unsigned long`, as on 32-bit ARMv7, and kept
/// to its low 24 bits, then any run of commas and white space skipped. A
/// number beyond 32 bits is therefore white (`ULONG_MAX`) whatever its sign.
/// An entry without digits is black and leaves the text where it was, so once
/// the skip cannot move either, the rest are black.
fn parse_colour_list(text: &[u8]) -> [u32; 16] {
    let mut colours = [0; 16];
    let mut at = 0;
    for colour in &mut colours {
        let start = at;
        let (value, end) = strtoul16(text, at);
        *colour = value & 0x00ff_ffff;
        at = end;
        while text.get(at).is_some_and(|&b| b == b',' || is_space(b)) {
            at += 1;
        }
        // Stuck: every later entry reads the same nothing.
        if at == start {
            break;
        }
    }
    colours
}

/// `%dx%d` as C's `sscanf` reads it: each number after optional white space,
/// with an optional sign, the `x` right after the first.
fn scan_size(text: &[u8]) -> Option<[i32; 2]> {
    let (width, end) = scan_int(text, 0)?;
    if text.get(end) != Some(&b'x') {
        return None;
    }
    let (height, _) = scan_int(text, end + 1)?;
    Some([width, height])
}

/// A size the reference opens a decoder with: both dimensions positive, and
/// eight bytes per pixel plus 128 pixels of padding each way addressable
/// with an `int`.
fn checked_canvas([width, height]: [i32; 2]) -> Result<[u32; 2]> {
    let limit = u64::from(i32::MAX.unsigned_abs());
    match (u32::try_from(width), u32::try_from(height)) {
        (Ok(w @ 1..), Ok(h @ 1..))
            if 8 * (u64::from(w) + 128) < limit
                && 8 * (u64::from(w) + 128) * (u64::from(h) + 128) < limit =>
        {
            Ok([w, h])
        }
        _ => Err(Error::Invalid("DVD configuration size")),
    }
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
        *colour = YcbcrMatrix::BT601.rgb(entry[1], entry[2], entry[3], ChannelRounding::Nearest);
    }
    Some(colours)
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
    /// The work budget ran out. The decoder keeps the unit and how far it got:
    /// [`resume`](Decoder::resume) it with more work, or
    /// [`abandon`](Decoder::abandon) it; [`decode`](Decoder::decode) refuses
    /// another packet until then. A unit needs its bytes, the command bytes each
    /// of its sequences reads and the pixels of every area it draws, each paid
    /// once however many slices it takes.
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
/// * **Control.** Sequences are followed from the control offset while the
///   link moves forward; a control offset of 0 runs none. A sequence ends at
///   `0xff`, at an unknown command (including `CHG_COLCON`) or at the end of
///   the unit; truncated operands drop the unit. Every sequence with both a
///   display area and field offsets (and at least two rows) redraws the
///   bitmap with the colours set so far; a failed redraw drops the unit after
///   the rest of it has run. The last start and stop dates give the display
///   window.
/// * **Colours.** The colour selectors and the contrast of each colour carry
///   over from unit to unit, even from units that present nothing. `0x04`
///   sets the first four contrasts to its raw nibbles and `0x84` all 256 to
///   255 minus its bytes. Four-colour bitmaps take the supplied palette (or
///   automatic greys) with seventeen times the contrast as alpha; an
///   extended area (`0x85`) anywhere in the unit makes its bitmaps eight-bit,
///   with the unit's own `0x83` colours, converted with
///   BT.601 rounded to nearest, and the contrast as alpha.
/// * **Pixels.** Two-bit fields are read from their offset to the end of the
///   unit, zero beyond it; an offset within four bytes of the end drops the
///   unit. An eight-bit code may run past the end, but must start before it.
///   A run past the end of a line drops the unit.
/// * **Presentation.** A forced start anywhere in the unit keeps the whole
///   area (a menu); otherwise rows and columns of fully transparent pixels are
///   removed, and a fully transparent bitmap presents nothing.
/// * **Refusal.** A decode that fails, or yields and is abandoned, changes
///   nothing the decoder carries to later packets: a unit over
///   [`Limits::max_unit`], an area over [`Limits::max_pixels`] or memory the
///   budget refuses leaves the buffered bytes, the colours and what
///   [`rewind`](Self::rewind) undoes as they were, so the decoder continues as
///   if the packet had never been sent.
#[derive(Debug)]
pub struct Decoder {
    settings: Settings,
    limits: Limits,
    pending: BudgetVec<u8>,
    plane: BudgetVec<u8>,
    colours: Colours,
    palette: [u32; 256],
    /// How to undo the last decode that neither yielded nor failed.
    last: Option<Undo>,
    /// The unit that yielded, if any.
    suspended: Option<Suspended>,
    /// A copy of a yielded unit that came whole in its packet, released when
    /// the unit completes or is abandoned.
    held: BudgetVec<u8>,
}

/// Where a run reads its unit.
#[derive(Clone, Copy, Debug)]
enum Source<'p> {
    /// The packet that brought the whole unit, still the caller's.
    Packet(&'p [u8]),
    /// The reassembly buffer.
    Pending,
    /// The copy of a packet that brought the whole unit and yielded.
    Held,
}

/// A unit that yielded: how far its run got and how to finish it.
#[derive(Clone, Copy, Debug)]
struct Suspended {
    cursor: Cursor,
    header: Header,
    /// The unit is in the reassembly buffer rather than in `held`.
    buffered: bool,
    undo: Undo,
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
            suspended: None,
            held: BudgetVec::new(budget),
        }
    }

    /// Undo the last [`decode`](Self::decode) that neither yielded nor
    /// failed, so that the same packet can be decoded again: for a consumer
    /// that could not store its subtitle. Refused while a unit is suspended.
    pub fn rewind(&mut self) -> Result<()> {
        if self.suspended.is_some() {
            return Err(Error::Invalid(
                "DVD receiver: resume or abandon the yielded unit",
            ));
        }
        if let Some(undo) = self.last.take() {
            self.colours = undo.colours;
            self.pending.truncate(undo.pending);
        }
        Ok(())
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Decode one packet. An empty packet presents nothing.
    pub fn decode(&mut self, packet: &[u8], work: &mut WorkBudget) -> Result<Status<'_>> {
        if self.suspended.is_some() {
            return Err(Error::Invalid(
                "DVD receiver: resume or abandon the yielded unit",
            ));
        }
        // A unit the last decode completed is discarded by this one.
        let resumed = match self.last {
            Some(Undo {
                completed: true, ..
            }) => 0,
            _ => self.pending.len(),
        };
        // The packet continues the buffered unit rather than starting one.
        let continues = resumed != 0;
        let length = resumed.saturating_add(packet.len());
        if length > self.limits.max_unit {
            return Err(Error::Limit {
                requested: length,
                maximum: self.limits.max_unit,
            });
        }
        let undo = Undo {
            colours: self.colours,
            pending: resumed,
            completed: false,
        };
        let header = if continues {
            self.pending.try_extend_from_slice(packet)?;
            Header::read(&self.pending)
        } else {
            Header::read(packet)
        };
        let Some(header) = header.filter(|header| header.complete(length)) else {
            if !continues {
                // An incomplete start is kept unless its control offset lies
                // beyond its size.
                let start = header.filter(|header| header.control <= header.size);
                self.restart_pending(start.map_or(&[], |_| packet))?;
            }
            self.last = Some(undo);
            return Ok(Status::Nothing);
        };
        let unit = Suspended {
            cursor: Cursor::new(self.colours),
            header,
            buffered: continues,
            undo,
        };
        let source = if continues {
            Source::Pending
        } else {
            Source::Packet(packet)
        };
        self.run(unit, source, work)
    }

    /// Continue the unit that yielded, from where it stopped.
    pub fn resume(&mut self, work: &mut WorkBudget) -> Result<Status<'_>> {
        let unit = self
            .suspended
            .take()
            .ok_or(Error::Invalid("DVD receiver: no yielded unit"))?;
        let source = if unit.buffered {
            Source::Pending
        } else {
            Source::Held
        };
        self.run(unit, source, work)
    }

    /// Drop the unit that yielded, as if its packet had never been sent.
    pub fn abandon(&mut self) {
        if let Some(unit) = self.suspended.take() {
            if unit.buffered {
                self.pending.truncate(unit.undo.pending);
            } else {
                self.release_held();
            }
        }
    }

    /// Free the copy of a yielded unit.
    fn release_held(&mut self) {
        let budget = self.held.budget().clone();
        self.held = BudgetVec::new(&budget);
    }

    /// Run a complete unit on from its cursor.
    ///
    /// A unit that came whole in its packet and yields is copied, for the
    /// packet goes back to its caller. A copy the pool refuses fails the call
    /// with that error, and the receiver is as it was before the packet: its
    /// colours and the reassembly buffer are only changed once a unit
    /// completes.
    fn run(
        &mut self,
        mut unit: Suspended,
        source: Source<'_>,
        work: &mut WorkBudget,
    ) -> Result<Status<'_>> {
        let bytes = match source {
            Source::Packet(packet) => packet,
            Source::Pending => self.pending.as_slice(),
            Source::Held => self.held.as_slice(),
        };
        let outcome = Pass {
            settings: &self.settings,
            limits: self.limits,
            palette: &mut self.palette,
            plane: &mut self.plane,
            work,
            cursor: &mut unit.cursor,
        }
        .run(bytes, unit.header);
        if matches!(source, Source::Held) && !matches!(outcome, Err(Halt::Yield)) {
            self.release_held();
        }
        match outcome {
            Ok(_) | Err(Halt::Malformed) => {
                self.colours = unit.cursor.colours;
                if !unit.buffered {
                    self.pending.clear();
                }
                self.last = Some(Undo {
                    completed: unit.buffered,
                    ..unit.undo
                });
            }
            Err(Halt::Yield) => {
                if let Source::Packet(packet) = source {
                    self.held.clear();
                    self.held.try_extend_from_slice(packet)?;
                }
                self.suspended = Some(unit);
            }
            Err(Halt::Failed(_)) => {
                if unit.buffered {
                    self.pending.truncate(unit.undo.pending);
                }
            }
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

    /// Replace the buffered bytes with `start`, or change nothing when the
    /// memory is refused.
    fn restart_pending(&mut self, start: &[u8]) -> Result<()> {
        self.pending
            .try_reserve_exact(start.len().saturating_sub(self.pending.len()))?;
        self.pending.clear();
        self.pending.try_extend_from_slice(start)
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
struct UnitState {
    start: u16,
    stop: u16,
    forced: bool,
    /// An extended area made the unit's bitmaps eight-bit.
    eight_bit: bool,
    /// Where the unit's last `0x83` colours start.
    extended_palette: Option<usize>,
}

/// How far a run through a unit got. Every step is paid for before it changes
/// anything, so a run that yields stops between steps and continues from here.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    /// The unit's bytes are paid for.
    paid: bool,
    /// Work granted towards the step that did not fit, paid with it.
    credit: u64,
    /// The sequence being run, and how far its commands have been read.
    at: usize,
    read: usize,
    /// Every command of that sequence has run.
    ended: bool,
    area: Option<DvdArea>,
    fields: Option<Fields>,
    state: UnitState,
    drawn: Option<Drawn>,
    failed: bool,
    colours: Colours,
}

impl Cursor {
    fn new(colours: Colours) -> Self {
        Self {
            paid: false,
            credit: 0,
            at: 0,
            read: 0,
            ended: false,
            area: None,
            fields: None,
            state: UnitState::default(),
            drawn: None,
            failed: false,
            colours,
        }
    }
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

/// One slice of a unit's run through its control sequences.
struct Pass<'d, 'w> {
    settings: &'d Settings,
    limits: Limits,
    palette: &'d mut [u32; 256],
    plane: &'d mut BudgetVec<u8>,
    work: &'w mut WorkBudget,
    cursor: &'d mut Cursor,
}

impl Pass<'_, '_> {
    fn run(&mut self, unit: &[u8], header: Header) -> std::result::Result<Shown, Halt> {
        if !self.cursor.paid {
            if !self.pay(unit.len()) {
                return Err(Halt::Yield);
            }
            self.cursor.paid = true;
            self.cursor.at = header.control;
        }
        // Offset 0 holds the unit's own header, never a sequence.
        if header.control == 0 {
            return Err(Halt::Malformed);
        }
        let head_bytes = header.sequence_header();
        while let Some((head, body)) = unit
            .get(self.cursor.at..)
            .and_then(|rest| rest.split_at_checked(head_bytes))
        {
            let date = u16::from_be_bytes([head[0], head[1]]);
            let next = head[2..]
                .iter()
                .fold(0_usize, |n, &b| n << 8 | usize::from(b));
            if !self.cursor.ended {
                self.commands(body, self.cursor.at + head_bytes, date)?;
                self.cursor.ended = true;
            }
            if let (Some(area), Some(fields)) = (self.cursor.area, self.cursor.fields) {
                let state = self.cursor.state;
                match self.draw(unit, area, fields, &state) {
                    Ok(Some(next)) => self.cursor.drawn = Some(next),
                    Ok(None) => {}
                    Err(Halt::Malformed) => self.cursor.failed = true,
                    Err(halt) => return Err(halt),
                }
            }
            if next <= self.cursor.at {
                break;
            }
            let cursor = &mut *self.cursor;
            (cursor.at, cursor.read, cursor.ended) = (next, 0, false);
            (cursor.area, cursor.fields) = (None, None);
        }
        let state = self.cursor.state;
        let drawn = (self.cursor.drawn)
            .filter(|_| !self.cursor.failed)
            .ok_or(Halt::Malformed)?;
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

    /// Pay for the next step, or bank all the work left towards it and yield.
    fn pay(&mut self, cost: usize) -> bool {
        let owed = (cost as u64).saturating_sub(self.cursor.credit);
        if self.work.try_charge(owed) {
            self.cursor.credit = 0;
            return true;
        }
        let rest = self.work.remaining();
        self.work.take(rest);
        self.cursor.credit = self.cursor.credit.saturating_add(rest);
        false
    }

    /// Run a sequence's commands from where the cursor stopped; `start` is
    /// where `body` sits in the unit.
    fn commands(&mut self, body: &[u8], start: usize, date: u16) -> std::result::Result<(), Halt> {
        let mut commands = Commands::resume(body, self.cursor.read);
        while commands.position() < body.len() {
            let from = commands.position();
            let command = commands.next().ok_or(Halt::Malformed)?;
            // A sequence reads on to its end command, through the bytes of the
            // sequences after it if need be: each sequence pays for what it
            // reads.
            if !self.pay(commands.position() - from) {
                return Err(Halt::Yield);
            }
            self.cursor.read = commands.position();
            let cursor = &mut *self.cursor;
            match command.map_err(|_| Halt::Malformed)? {
                Command::ForcedStart => cursor.state.forced = true,
                Command::Start => cursor.state.start = date,
                Command::Stop => cursor.state.stop = date,
                Command::Palette(word) => {
                    cursor.colours.selectors =
                        std::array::from_fn(|i| (word >> (4 * i) & 15) as u8);
                }
                Command::Alpha(word) => {
                    for i in 0..4 {
                        cursor.colours.contrast[i] = (word >> (4 * i) & 15) as u8;
                    }
                }
                Command::ExtendedContrast(values) => {
                    for (alpha, value) in cursor.colours.contrast.iter_mut().zip(values) {
                        *alpha = 255 - value;
                    }
                }
                // The colours follow the opcode.
                Command::ExtendedPalette(_) => {
                    cursor.state.extended_palette = Some(start + from + 1);
                }
                Command::Area(value) => cursor.area = Some(value),
                Command::ExtendedArea(value) => {
                    cursor.area = Some(value);
                    cursor.state.eight_bit = true;
                }
                Command::Fields(value) => {
                    cursor.fields = Some(Fields {
                        top: usize::from(value.top),
                        bottom: usize::from(value.bottom),
                    });
                }
                Command::ExtendedFields { top, bottom } => {
                    cursor.fields = Some(Fields {
                        top: top as usize,
                        bottom: bottom as usize,
                    });
                }
                Command::End | Command::Unknown => break,
            }
        }
        Ok(())
    }

    /// Resolve the colours and expand one area into the plane; `None` for an
    /// area the receiver skips (reversed, or a single row).
    fn draw(
        &mut self,
        unit: &[u8],
        area: DvdArea,
        fields: Fields,
        state: &UnitState,
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
        if !self.pay(pixels) {
            return Err(Halt::Yield);
        }
        let colours = if state.eight_bit {
            let table: &[u8; 768] = state
                .extended_palette
                .and_then(|at| unit.get(at..at.checked_add(768)?)?.try_into().ok())
                .ok_or(Halt::Malformed)?;
            for ((argb, yuv), alpha) in self
                .palette
                .iter_mut()
                .zip(table.chunks_exact(3))
                .zip(self.cursor.colours.contrast)
            {
                *argb = u32::from(alpha) << 24
                    | YcbcrMatrix::BT601.rgb(yuv[0], yuv[1], yuv[2], ChannelRounding::Nearest);
            }
            256
        } else {
            let contrast = std::array::from_fn(|i| self.cursor.colours.contrast[i]);
            let four = four_colours(
                self.settings.palette.as_ref(),
                self.cursor.colours.selectors,
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

#[cfg(test)]
mod tests {
    use super::{Settings, below_0e};

    #[test]
    fn below_0e_flags_exactly_the_words_with_a_low_byte() {
        for byte in 0..=u8::MAX {
            for at in 0..8 {
                for fill in [b'a', 0x0e, 0x80, 0xff] {
                    let mut bytes = [fill; 8];
                    bytes[at] = byte;
                    let word = u64::from_le_bytes(bytes);
                    assert_eq!(below_0e(word) != 0, byte < 0x0e, "{bytes:?}");
                }
            }
        }
    }

    /// Line starts are found at every alignment, after LF and CR alike, and
    /// nothing after a NUL is read.
    #[test]
    fn lines_start_after_every_break_at_every_offset() {
        for prefix in 0..20 {
            for (stop, found) in [(b'\n', true), (b'\r', true), (0, false), (b'\t', false)] {
                let mut text = vec![b'.'; prefix];
                text.push(stop);
                text.extend_from_slice(b"palette: 7");
                let palette = Settings::from_configuration(&text).unwrap().palette;
                assert_eq!(palette.map(|p| p[0]), found.then_some(7), "{text:?}");
            }
        }
    }
}
