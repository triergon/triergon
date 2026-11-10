//! The fixture directives shared with `harness/reference_ass.h`. A fixture is
//! split at LF; a final LF ends the last line.
//!
//! ```text
//! ;@force OVERRIDE       a [Style.]Field=Value style override (at most 255)
//! ;@extract-fonts        keep the fonts of [Fonts]
//! ;@time MS...           the times events are walked at (at most 64)
//! ;@attach PATH            a font file a renderer is given (at most 64)
//! ;@default-family NAME    the renderer's default font family
//! ;@default-font PATH      the renderer's default font file
//! ;@frames START STEP END  a whole script's frames, START to END milliseconds
//! ```
//!
//! A stream fixture also holds commands, run in order on a new track:
//!
//! ```text
//! @private ... @end      the lines between, each with its LF, as codec private
//! @data ... @end         as more script data
//! @styles ... @end       as style lines (ass_read_styles)
//! @chunk START DURATION  the next line as one packet
//! @dump                  records the track so far
//! @flush                 ass_flush_events
//! @prune DEADLINE        ass_prune_events
//! @check-readorder N     ass_set_check_readorder
//! @frames START STEP END renders those frames (rendering only)
//! ```
//!
//! A block without `@end` runs to the end of the file. Numbers are an
//! optional `-` and 1 to 18 digits after any spaces; `@chunk` without two
//! of them is ignored, and leaves its next line to be read as a command.

/// Directives bound the work of the harness itself, as the reference's
/// arrays do.
const MAX_OVERRIDES: usize = 255;
const MAX_TIMES: usize = 64;
const MAX_FONTS: usize = 64;
const MAX_FRAMES: usize = 64;

/// Frames rendered every `step` milliseconds from `start` to `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frames {
    pub start: i64,
    pub step: i64,
    pub end: i64,
}

impl Frames {
    /// Three numbers with a positive step.
    fn read(s: &[u8]) -> Option<Self> {
        let (start, rest) = number(s)?;
        let (step, rest) = number(rest)?;
        let (end, _) = number(rest)?;
        (step > 0).then_some(Self { start, step, end })
    }

    /// The frame times.
    pub fn times(self) -> impl Iterator<Item = i64> {
        let Self { start, step, end } = self;
        std::iter::successors(Some(start), move |&t| t.checked_add(step))
            .take_while(move |&t| t <= end)
    }
}

/// One command of a stream fixture.
#[derive(Debug)]
pub enum Command<'a> {
    Private(Vec<u8>),
    Data(Vec<u8>),
    Styles(Vec<u8>),
    Chunk {
        start: i64,
        duration: i64,
        packet: &'a [u8],
    },
    Dump,
    Flush,
    Prune(i64),
    CheckReadOrder(bool),
    Frames(Frames),
}

/// A fixture's directives and, for a stream, its commands.
#[derive(Debug, Default)]
pub struct Fixture<'a> {
    pub overrides: Vec<&'a [u8]>,
    pub extract_fonts: bool,
    pub times: Vec<i64>,
    pub attach: Vec<&'a [u8]>,
    pub default_family: Option<&'a [u8]>,
    pub default_font: Option<&'a [u8]>,
    pub frames: Vec<Frames>,
    /// A `;@frames` directive was malformed or one too many, which the
    /// renderers refuse.
    pub bad_frames: bool,
    pub commands: Vec<Command<'a>>,
}

/// The lines of `data` without their LFs.
fn lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = data.split(|&c| c == b'\n').collect();
    if data.is_empty() || data.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

/// A number at the start of `s`, and the rest.
fn number(s: &[u8]) -> Option<(i64, &[u8])> {
    let start = s.iter().position(|&c| c != b' ').unwrap_or(s.len());
    let s = &s[start..];
    let negative = s.first() == Some(&b'-');
    let digits = &s[usize::from(negative)..];
    let n = digits.iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 || n > 18 {
        return None;
    }
    let value = digits[..n]
        .iter()
        .fold(0_i64, |v, &d| v * 10 + i64::from(d - b'0'));
    Some((if negative { -value } else { value }, &digits[n..]))
}

impl<'a> Fixture<'a> {
    /// Reads the directives, and with `stream` the commands.
    pub fn read(data: &'a [u8], stream: bool) -> Self {
        let lines = lines(data);
        let mut fixture = Fixture::default();
        for line in &lines {
            if let Some(item) = line.strip_prefix(b";@force ") {
                if fixture.overrides.len() < MAX_OVERRIDES {
                    fixture.overrides.push(item);
                }
            } else if line.starts_with(b";@extract-fonts") {
                fixture.extract_fonts = true;
            } else if let Some(mut rest) = line.strip_prefix(b";@time ") {
                while fixture.times.len() < MAX_TIMES {
                    let Some((time, after)) = number(rest) else {
                        break;
                    };
                    fixture.times.push(time);
                    rest = after;
                }
            } else if let Some(path) = line.strip_prefix(b";@attach ") {
                if fixture.attach.len() < MAX_FONTS {
                    fixture.attach.push(path);
                }
            } else if let Some(name) = line.strip_prefix(b";@default-family ") {
                fixture.default_family = Some(name);
            } else if let Some(path) = line.strip_prefix(b";@default-font ") {
                fixture.default_font = Some(path);
            } else if let Some(frames) = line.strip_prefix(b";@frames ") {
                match Frames::read(frames) {
                    Some(frames) if fixture.frames.len() < MAX_FRAMES => {
                        fixture.frames.push(frames);
                    }
                    _ => fixture.bad_frames = true,
                }
            }
        }
        if stream {
            fixture.commands = commands(&lines);
        }
        fixture
    }
}

fn commands<'a>(lines: &[&'a [u8]]) -> Vec<Command<'a>> {
    let mut commands = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let block = |i: usize| {
            let end = (i + 1..lines.len())
                .find(|&j| lines[j].starts_with(b"@end"))
                .unwrap_or(lines.len());
            let text: Vec<u8> = lines[i + 1..end]
                .iter()
                .flat_map(|l| l.iter().copied().chain(*b"\n"))
                .collect();
            (text, end)
        };
        if line.starts_with(b"@private") {
            let (text, end) = block(i);
            commands.push(Command::Private(text));
            i = end;
        } else if line.starts_with(b"@data") {
            let (text, end) = block(i);
            commands.push(Command::Data(text));
            i = end;
        } else if line.starts_with(b"@styles") {
            let (text, end) = block(i);
            commands.push(Command::Styles(text));
            i = end;
        } else if let Some(args) = line.strip_prefix(b"@chunk ") {
            if let Some((start, rest)) = number(args)
                && let Some((duration, _)) = number(rest)
                && i + 1 < lines.len()
            {
                i += 1;
                commands.push(Command::Chunk {
                    start,
                    duration,
                    packet: lines[i],
                });
            }
        } else if let Some(frames) = line.strip_prefix(b"@frames ") {
            if let Some(frames) = Frames::read(frames) {
                commands.push(Command::Frames(frames));
            }
        } else if line.starts_with(b"@dump") {
            commands.push(Command::Dump);
        } else if line.starts_with(b"@flush") {
            commands.push(Command::Flush);
        } else if let Some(deadline) = line.strip_prefix(b"@prune ") {
            commands.push(Command::Prune(number(deadline).map_or(0, |(n, _)| n)));
        } else if let Some(check) = line.strip_prefix(b"@check-readorder ") {
            commands.push(Command::CheckReadOrder(
                number(check).is_some_and(|(n, _)| n == 1),
            ));
        }
        i += 1;
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::{Command, Fixture, number};

    #[test]
    fn directives_and_commands_read_as_the_reference_reads_them() {
        assert_eq!(number(b"  -12 7"), Some((-12, &b" 7"[..])));
        assert_eq!(number(b"+1"), None);
        assert_eq!(number(b"1234567890123456789"), None);
        let data = b";@time 0 500 x 9\n;@force A=B\n@chunk 1 2\npacket\n@chunk 1\n@prune 9\n\
            @frames 0 250 600\n@frames 0 0 9\n;@frames 1 1 2\n;@attach a.ttf\n";
        let fixture = Fixture::read(data, true);
        assert_eq!(fixture.times, [0, 500]);
        assert_eq!(fixture.attach, [&b"a.ttf"[..]]);
        assert_eq!(fixture.frames[0].times().collect::<Vec<_>>(), [1, 2]);
        assert!(!fixture.bad_frames);
        // Two numbers are no frames directive.
        assert!(Fixture::read(b";@frames 0 500\n", false).bad_frames);
        assert_eq!(fixture.overrides, [&b"A=B"[..]]);
        assert!(matches!(
            fixture.commands[..],
            [
                Command::Chunk {
                    start: 1,
                    duration: 2,
                    packet: b"packet"
                },
                Command::Prune(9),
                Command::Frames(super::Frames {
                    start: 0,
                    step: 250,
                    end: 600
                })
            ]
        ));
    }
}
