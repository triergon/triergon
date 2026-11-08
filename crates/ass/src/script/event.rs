//! Events and libass's reading of their fields.

use triergon_core::{Result, WorkBudget};

use super::{Margins, Script};
use crate::charge;
use crate::number::{header_int, scanf_int};
use crate::scan::next_token;
use crate::strings::Strings;

/// One dialogue event. Times are milliseconds.
#[derive(Clone, Debug)]
pub struct Event {
    /// When the event starts.
    pub start: i64,
    /// How long it is shown.
    pub duration: i64,
    /// The file position for a whole script, the Matroska ReadOrder for a
    /// packet, else 0.
    pub read_order: i32,
    /// Events of higher layers are drawn over those of lower ones.
    pub layer: i32,
    /// An index into [`Script::styles`].
    pub style: usize,
    /// 0 means the style's margin.
    pub margins: Margins,
    strings: Strings<3>,
    /// Where collision handling placed the event, for later frames.
    pub(crate) placed: crate::render::Placed,
}

impl Event {
    /// The text with its override blocks, trailing blanks and CRs removed.
    pub fn text(&self) -> &[u8] {
        self.strings.get(0).unwrap_or_default()
    }

    /// The name or actor field, if the format has one.
    pub fn name(&self) -> Option<&[u8]> {
        self.strings.get(1)
    }

    /// The effect field, if the format has one.
    pub fn effect(&self) -> Option<&[u8]> {
        self.strings.get(2)
    }

    /// `start + duration`, wrapping as libass's arithmetic does.
    pub const fn end(&self) -> i64 {
        self.start.wrapping_add(self.duration)
    }

    /// Whether libass renders the event at `now`: `start <= now < end`.
    pub const fn is_shown_at(&self, now: i64) -> bool {
        self.start <= now && now < self.end()
    }
}

/// libass's `string2timecode`: `H:M:S.CS` read by `sscanf("%d:%d:%d.%d")`,
/// with the last field taken as centiseconds; anything shorter is 0.
fn timecode(s: &[u8]) -> i64 {
    let mut fields = [0_i64; 4];
    let mut rest = s;
    for (i, field) in fields.iter_mut().enumerate() {
        let Some((value, used)) = scanf_int(rest) else {
            return 0;
        };
        *field = i64::from(value);
        rest = &rest[used..];
        if i < 3 {
            match rest.split_first() {
                Some((&c, tail)) if c == if i < 2 { b':' } else { b'.' } => rest = tail,
                _ => return 0,
            }
        }
    }
    let [h, m, s, cs] = fields;
    ((h * 60 + m) * 60 + s) * 1000 + cs * 10
}

impl Script {
    /// libass's `process_event_tail`: fields in event-format order after
    /// skipping `skip` format names, up to `Text`, which takes the rest of the
    /// line. `None` when the line ends before `Text` or the format has none.
    pub(super) fn event_tail(
        &self,
        line: &[u8],
        skip: usize,
        work: &mut WorkBudget,
    ) -> Result<Option<Event>> {
        let Some(format) = self.event_format.as_deref() else {
            return Ok(None);
        };
        let mut names = format;
        for _ in 0..skip {
            if next_token(&mut names, false).is_none() {
                break;
            }
        }
        let mut values = line;
        let (mut start, mut end) = (0, 0);
        let (mut layer, mut style, mut margins) = (0, 0, Margins::default());
        let (mut name, mut effect) = (None, None);
        while let Some(field) = next_token(&mut names, true) {
            charge(work, 1)?;
            if field.eq_ignore_ascii_case(b"Text") {
                let end_of_text = values
                    .iter()
                    .rposition(|&c| !matches!(c, b'\r' | b'\t' | b' '))
                    .map_or(0, |i| i + 1);
                return Ok(Some(Event {
                    start,
                    duration: end - start,
                    read_order: 0,
                    layer,
                    style,
                    margins,
                    strings: Strings::new(
                        &self.budget,
                        [Some(&values[..end_of_text]), name, effect],
                    )?,
                    placed: crate::render::Placed::default(),
                }));
            }
            let Some(token) = next_token(&mut values, false) else {
                return Ok(None);
            };
            let is = |n: &[u8]| field.eq_ignore_ascii_case(n);
            if is(b"Layer") {
                layer = header_int(token);
            } else if is(b"Style") {
                charge(work, self.styles.len())?;
                style = self.lookup_style(token);
            } else if is(b"Name") || is(b"Actor") {
                name = Some(token);
            } else if is(b"Effect") {
                effect = Some(token);
            } else if is(b"MarginL") {
                margins.left = header_int(token);
            } else if is(b"MarginR") {
                margins.right = header_int(token);
            } else if is(b"MarginV") {
                margins.vertical = header_int(token);
            } else if is(b"Start") {
                start = timecode(token);
            } else if is(b"End") || is(b"Duration") {
                end = timecode(token);
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::timecode;

    #[test]
    fn timecodes_read_centiseconds() {
        assert_eq!(timecode(b"0:00:01.50"), 1500);
        assert_eq!(timecode(b"0:00:01.5"), 1050);
        assert_eq!(timecode(b"0:00:01.500"), 6000);
        assert_eq!(timecode(b" 1:2:3.4"), 3_723_040);
        assert_eq!(timecode(b"-0:00:01.00"), 1000);
        assert_eq!(timecode(b"0:00:01"), 0);
        assert_eq!(timecode(b"0: 00:01.00"), 1000);
    }
}
