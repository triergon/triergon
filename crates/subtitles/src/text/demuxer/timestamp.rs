//! SubRip times and cue numbers, read as the `srt` demuxer reads them.
//!
//! A number is read as C's `sscanf` reads `%d`: leading white space, an
//! optional sign, then decimal digits, saturated to the range of a 32-bit
//! `int` as on 32-bit targets. A field has no range of its own: minutes and
//! seconds above 59 count as they are, and the fraction is a count of
//! milliseconds whatever its width (`1,5` is 1.005 seconds).

/// A cursor over one line.
struct Scan<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn space(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.at += 1;
        }
    }

    /// One byte of `set`.
    fn one_of(&mut self, set: &[u8]) -> Option<()> {
        self.peek().filter(|byte| set.contains(byte))?;
        self.at += 1;
        Some(())
    }

    /// `%d`, as its sign and the magnitude of its digits.
    fn signed(&mut self) -> Option<(bool, i64)> {
        self.space();
        let negative = self.peek() == Some(b'-');
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.at += 1;
        }
        let start = self.at;
        let mut magnitude = 0_i64;
        while let Some(digit) = self.peek().filter(u8::is_ascii_digit) {
            magnitude = magnitude
                .saturating_mul(10)
                .saturating_add(i64::from(digit - b'0'));
            self.at += 1;
        }
        (self.at > start).then_some((negative, magnitude))
    }

    /// `%d` as an `int`.
    fn integer(&mut self) -> Option<i64> {
        let (negative, magnitude) = self.signed()?;
        let value = if negative { -magnitude } else { magnitude };
        Some(value.clamp(i32::MIN.into(), i32::MAX.into()))
    }

    /// `%d:%d:%d`, one byte of `separators`, `%d`; in milliseconds.
    fn clock(&mut self, separators: &[u8]) -> Option<i64> {
        let hours = self.integer()?;
        self.one_of(b":")?;
        let minutes = self.integer()?;
        self.one_of(b":")?;
        let seconds = self.integer()?;
        self.one_of(separators)?;
        let fraction = self.integer()?;
        Some(milliseconds(hours, minutes, seconds, fraction))
    }
}

/// C's `isspace` in the C locale.
pub(super) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn milliseconds(hours: i64, minutes: i64, seconds: i64, fraction: i64) -> i64 {
    ((hours * 60 + minutes) * 60 + seconds) * 1000 + fraction
}

/// A SubRip timing line's start and end in milliseconds: `H:M:S,F --> H:M:S,F`
/// with `,` or `.` before the fraction, white space around the arrow
/// optional and anything after the end.
pub(super) fn subrip(line: &[u8]) -> Option<(i64, i64)> {
    let mut scan = Scan { bytes: line, at: 0 };
    let start = scan.clock(b",.")?;
    scan.space();
    line.get(scan.at..)?.starts_with(b"-->").then_some(())?;
    scan.at += 3;
    let end = scan.clock(b",.")?;
    Some((start, end))
}

/// Whether a SubRip line reads as a cue number, one that is not negative:
/// the whole line when `whole`, otherwise its start.
pub(super) fn cue_number(line: &[u8], whole: bool) -> bool {
    let mut scan = Scan { bytes: line, at: 0 };
    scan.signed().is_some_and(|(negative, magnitude)| {
        (!negative || magnitude == 0) && (!whole || scan.at == line.len())
    })
}
