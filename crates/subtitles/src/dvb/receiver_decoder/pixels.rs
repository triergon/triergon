//! Pixel-data sub-blocks (EN 300 743 §7.2.5.1): run-length strings, map
//! tables and end-of-line codes, drawn into a region's index plane.
use crate::dvb::Depth;

/// A region's index plane as a drawing target.
#[derive(Debug)]
pub(super) struct Canvas<'a> {
    pub(super) pixels: &'a mut [u8],
    pub(super) width: usize,
    pub(super) depth: Depth,
}

/// Where a field starts and how its codes apply.
#[derive(Clone, Copy, Debug)]
pub(super) struct Pen {
    pub(super) x: usize,
    pub(super) y: usize,
    /// Pixel code 1 is the non-modifying colour.
    pub(super) non_modifying: bool,
}

/// The map tables of one field, reset to the defaults of §10.4–10.6.
#[derive(Clone, Copy, Debug)]
struct Maps {
    two_to_four: [u8; 4],
    two_to_eight: [u8; 4],
    four_to_eight: [u8; 16],
}

impl Default for Maps {
    fn default() -> Self {
        Self {
            two_to_four: [0x0, 0x7, 0x8, 0xf],
            two_to_eight: [0x00, 0x77, 0x88, 0xff],
            four_to_eight: std::array::from_fn(|i| i as u8 * 0x11),
        }
    }
}

/// The map of a string as deep as its region.
static IDENTITY: [u8; 256] = {
    let mut map = [0; 256];
    let mut i = 0;
    while i < 256 {
        map[i] = i as u8;
        i += 1;
    }
    map
};

/// A big-endian bit reader that reads zeros past its end.
#[derive(Debug)]
struct Bits<'a> {
    bytes: &'a [u8],
    /// The next byte to load into `cache`.
    next: usize,
    /// Unread bits, most significant first; `available` of them are valid.
    cache: u64,
    available: u32,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            next: 0,
            cache: 0,
            available: 0,
        }
    }

    /// Bits read so far.
    fn position(&self) -> usize {
        self.next * 8 - self.available as usize
    }

    fn exhausted(&self) -> bool {
        self.position() >= self.bytes.len() * 8
    }

    /// Keep at least 32 bits in the cache.
    #[inline]
    fn refill(&mut self) {
        if self.available < 32 {
            let mut window = [0; 4];
            if let Some(chunk) = self.bytes.get(self.next..self.next + 4) {
                window.copy_from_slice(chunk);
            } else if let Some(rest) = self.bytes.get(self.next..) {
                window[..rest.len()].copy_from_slice(rest);
            }
            self.cache |= u64::from(u32::from_be_bytes(window)) << (32 - self.available);
            self.available += 32;
            self.next += 4;
        }
    }

    /// The next `count` bits, 1 to 8, from the cache: at most the 32 bits
    /// the last [`refill`](Self::refill) guaranteed.
    #[inline]
    fn take(&mut self, count: u32) -> usize {
        let value = (self.cache >> (64 - count)) as usize;
        self.cache <<= count;
        self.available -= count;
        value
    }

    #[inline]
    fn read(&mut self, count: u32) -> usize {
        self.refill();
        self.take(count)
    }

    /// Bytes consumed, rounded up to the next byte boundary.
    fn consumed(&self) -> usize {
        self.position().div_ceil(8)
    }
}

/// One line of a field from the pen position to the region's right edge.
#[derive(Debug)]
struct Line<'a> {
    row: &'a mut [u8],
    /// Pixels decoded, up to the row length.
    read: usize,
    /// Pixels written, at most `read`.
    written: usize,
    /// Region index of each pixel code.
    map: &'a [u8],
    non_modifying: bool,
}

impl Line<'_> {
    fn full(&self) -> bool {
        self.read >= self.row.len()
    }

    /// One pixel of a nonzero `code`, the commonest code in glyph rows, on
    /// a line that is not full.
    #[inline]
    fn pixel(&mut self, code: usize) {
        self.read += 1;
        if !(self.non_modifying && code == 1) {
            self.row[self.written] = self.map[code];
            self.written += 1;
        }
    }

    /// `length` pixels of `code`, clipped at the edge. Non-modifying pixels
    /// are counted but not written, and the pixels after them move up: the
    /// receiver drops them rather than leaving the region's pixels in place.
    fn run(&mut self, length: usize, code: usize) {
        let length = length.min(self.row.len() - self.read);
        self.read += length;
        if self.non_modifying && code == 1 {
            return;
        }
        let index = self.map[code];
        paint(&mut self.row[self.written..self.written + length], index);
        self.written += length;
    }
}

/// Fill a run. Short runs, most of a glyph's, take two overlapping
/// fixed-size stores rather than a `memset` call.
#[inline]
fn paint(run: &mut [u8], index: u8) {
    fn ends<const N: usize>(run: &mut [u8], index: u8) {
        let n = run.len();
        run[..N].copy_from_slice(&[index; N]);
        run[n - N..].copy_from_slice(&[index; N]);
    }
    match run.len() {
        0 => {}
        n @ 1..=3 => {
            run[0] = index;
            run[n / 2] = index;
            run[n - 1] = index;
        }
        4..=7 => ends::<4>(run, index),
        8..=15 => ends::<8>(run, index),
        16..=31 => ends::<16>(run, index),
        _ => run.fill(index),
    }
}

/// Draw one field: lines at `pen.y`, `pen.y + 2`, ... The field stops at
/// a sub-block other than an end of line past the right edge, at any below
/// the bottom, and at a pixel string deeper than the region; it returns
/// whether it reached the end of `block` instead.
pub(super) fn draw_field(canvas: &mut Canvas<'_>, block: &[u8], pen: Pen) -> bool {
    let mut maps = Maps::default();
    let (mut x, mut y) = (pen.x, pen.y);
    let height = canvas.pixels.len() / canvas.width.max(1);
    let mut at = 0;
    while let Some(&kind) = block.get(at) {
        if (kind != 0xf0 && x >= canvas.width) || y >= height {
            return false;
        }
        at += 1;
        let rest = &block[at..];
        match kind {
            0x10..=0x12 => {
                let bits = [2, 4, 8][usize::from(kind - 0x10)];
                if bits > canvas.depth.bits() {
                    return false;
                }
                let map: &[u8] = match (bits, canvas.depth) {
                    (2, Depth::Four) => &maps.two_to_four,
                    (2, Depth::Eight) => &maps.two_to_eight,
                    (4, Depth::Eight) => &maps.four_to_eight,
                    _ => &IDENTITY,
                };
                let row = &mut canvas.pixels[y * canvas.width..][x..canvas.width];
                let mut line = Line {
                    row,
                    read: 0,
                    written: 0,
                    map,
                    non_modifying: pen.non_modifying,
                };
                at += string(rest, bits, &mut line);
                x += line.read;
            }
            0x20 => {
                if let Some(&[a, b]) = rest.first_chunk::<2>() {
                    maps.two_to_four = [a >> 4, a & 15, b >> 4, b & 15];
                }
                at += 2;
            }
            0x21 => {
                if let Some(table) = rest.first_chunk::<4>() {
                    maps.two_to_eight = *table;
                }
                at += 4;
            }
            0x22 => {
                if let Some(table) = rest.first_chunk::<16>() {
                    maps.four_to_eight = *table;
                }
                at += 16;
            }
            0xf0 => {
                x = pen.x;
                y += 2;
            }
            _ => {}
        }
    }
    true
}

/// Decode one pixel-code string into `line`; returns the bytes it used.
/// Decoding stops at the end-of-string code or once the line is full; a full
/// line is followed by an end-of-string code's worth of bits, which the
/// receiver reads and drops whatever they hold.
fn string(bytes: &[u8], bits: u8, line: &mut Line<'_>) -> usize {
    if bits == 8 {
        return eight(bytes, line);
    }
    let mut b = Bits::new(bytes);
    let ended = if bits == 2 {
        two(&mut b, line)
    } else {
        four(&mut b, line)
    };
    if !ended {
        b.read(if bits == 2 { 6 } else { 8 });
    }
    b.consumed()
}

/// What a zero code introduces: a run of `length` pixels of `code`, or the
/// end of the string.
enum Escape {
    Run { length: usize, code: usize },
    End,
}

/// 2-bit/pixel code string (Table 22); `true` at the end-of-string code.
fn two(b: &mut Bits<'_>, line: &mut Line<'_>) -> bool {
    while !b.exhausted() && !line.full() {
        // One code takes at most 16 bits.
        b.refill();
        match b.take(2) {
            0 => match two_escape(b) {
                Escape::Run { length, code } => line.run(length, code),
                Escape::End => return true,
            },
            code => line.pixel(code),
        }
    }
    false
}

fn two_escape(b: &mut Bits<'_>) -> Escape {
    let run = |length, code| Escape::Run { length, code };
    if b.take(1) == 1 {
        return run(3 + b.take(3), b.take(2));
    }
    if b.take(1) == 1 {
        return run(1, 0);
    }
    match b.take(2) {
        0 => Escape::End,
        1 => run(2, 0),
        2 => run(12 + b.take(4), b.take(2)),
        _ => run(29 + b.take(8), b.take(2)),
    }
}

/// 4-bit/pixel code string (Table 24); `true` at the end-of-string code.
fn four(b: &mut Bits<'_>, line: &mut Line<'_>) -> bool {
    while !b.exhausted() && !line.full() {
        // One code takes at most 20 bits.
        b.refill();
        match b.take(4) {
            0 => match four_escape(b) {
                Escape::Run { length, code } => line.run(length, code),
                Escape::End => return true,
            },
            code => line.pixel(code),
        }
    }
    false
}

fn four_escape(b: &mut Bits<'_>) -> Escape {
    let run = |length, code| Escape::Run { length, code };
    if b.take(1) == 0 {
        return match b.take(3) {
            0 => Escape::End,
            n => run(n + 2, 0),
        };
    }
    if b.take(1) == 0 {
        return run(4 + b.take(2), b.take(4));
    }
    match b.take(2) {
        0 => run(1, 0),
        1 => run(2, 0),
        2 => run(9 + b.take(4), b.take(4)),
        _ => run(25 + b.take(8), b.take(4)),
    }
}

/// 8-bit/pixel code string (Table 26), whose codes are whole bytes;
/// returns the bytes it used. After a full line the receiver takes one byte
/// more, and a zero byte after it.
fn eight(bytes: &[u8], line: &mut Line<'_>) -> usize {
    let byte = |at: usize| bytes.get(at).copied().map_or(0, usize::from);
    let mut at = 0;
    while at < bytes.len() && !line.full() {
        let code = byte(at);
        if code != 0 {
            line.pixel(code);
            at += 1;
            continue;
        }
        let switch = byte(at + 1);
        let length = switch & 0x7f;
        if switch & 0x80 == 0 {
            at += 2;
            if length == 0 {
                return at;
            }
            line.run(length, 0);
        } else {
            line.run(length, byte(at + 2));
            at += 3;
        }
    }
    at += 1;
    if at < bytes.len() && byte(at) == 0 {
        at += 1;
    }
    at
}
