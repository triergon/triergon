//! Source timestamp grammar with exact arithmetic and resumable token state.
use super::{
    Encoding, Field, FieldOwner,
    line::Line,
    unicode::{Cursor, Poll, cancelled},
};
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, Result, Time, WorkBudget};

#[derive(Debug)]
struct Timestamp {
    fields: [u128; 3],
    digits: [usize; 3],
    field: usize,
    fraction: u16,
    fraction_digits: u8,
    fractional: bool,
    comma: bool,
}

impl Timestamp {
    fn new(comma: bool) -> Self {
        Self {
            fields: [0; 3],
            digits: [0; 3],
            field: 0,
            fraction: 0,
            fraction_digits: 0,
            fractional: false,
            comma,
        }
    }

    fn push(&mut self, value: char) -> Result<()> {
        if let Some(digit) = value.to_digit(10).filter(|_| value.is_ascii_digit()) {
            if self.fractional {
                if self.fraction_digits == 3 {
                    return Err(Error::Invalid("timestamp fraction width"));
                }
                self.fraction = self.fraction * 10 + digit as u16;
                self.fraction_digits += 1;
            } else {
                self.fields[self.field] = self.fields[self.field]
                    .checked_mul(10)
                    .and_then(|number| number.checked_add(u128::from(digit)))
                    .ok_or(Error::Overflow)?;
                self.digits[self.field] = self.digits[self.field]
                    .checked_add(1)
                    .ok_or(Error::Overflow)?;
            }
            return Ok(());
        }
        if !self.fractional && value == ':' && self.field < 2 && self.digits[self.field] != 0 {
            self.field += 1;
            return Ok(());
        }
        let separator = if self.comma { ',' } else { '.' };
        if !self.fractional && value == separator && self.field >= 1 {
            self.fractional = true;
            return Ok(());
        }
        Err(Error::Invalid("timestamp character"))
    }

    fn finish(&self) -> Result<Time> {
        if !self.fractional
            || self.fraction_digits != 3
            || self.field == 0
            || (self.comma && self.field != 2)
        {
            return Err(Error::Invalid("timestamp syntax"));
        }
        let (hours, minutes, seconds) = if self.field == 2 {
            if self.digits[0] < 2 || self.digits[1] != 2 || self.digits[2] != 2 {
                return Err(Error::Invalid("timestamp field width"));
            }
            (self.fields[0], self.fields[1], self.fields[2])
        } else {
            if self.digits[0] != 2 || self.digits[1] != 2 {
                return Err(Error::Invalid("timestamp field width"));
            }
            (0, self.fields[0], self.fields[1])
        };
        if minutes >= 60 || seconds >= 60 {
            return Err(Error::Invalid("timestamp minute or second"));
        }
        let milliseconds = hours
            .checked_mul(60)
            .and_then(|value| value.checked_add(minutes))
            .and_then(|value| value.checked_mul(60))
            .and_then(|value| value.checked_add(seconds))
            .and_then(|value| value.checked_mul(1000))
            .and_then(|value| value.checked_add(u128::from(self.fraction)))
            .ok_or(Error::Overflow)?;
        Time::new(
            i128::try_from(milliseconds).map_err(|_| Error::Overflow)?,
            1000,
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Start,
    BeforeArrow,
    ArrowSecond,
    ArrowThird,
    AfterArrow,
    BeforeEnd,
    End,
    Settings,
}

#[derive(Debug)]
pub(crate) struct Parsed {
    pub start: Time,
    pub end: Time,
    pub duration: Time,
    pub settings: Option<Field>,
}

#[derive(Debug)]
pub(crate) struct Parser {
    cursor: Cursor,
    phase: Phase,
    timestamp: Timestamp,
    start: Option<Time>,
    end: Option<Time>,
    line_end: usize,
    comma: bool,
}

impl Parser {
    pub fn new(line: &Line, encoding: Encoding, comma: bool) -> Self {
        Self {
            cursor: Cursor::new(line.range(), encoding),
            phase: Phase::Start,
            timestamp: Timestamp::new(comma),
            start: None,
            end: None,
            line_end: line.end,
            comma,
        }
    }

    fn result(&self, settings: Option<Field>) -> Result<Parsed> {
        let start = self.start.ok_or(Error::Invalid("missing cue start"))?;
        let end = self.end.ok_or(Error::Invalid("missing cue end"))?;
        if end <= start {
            return Err(Error::Invalid("nonpositive cue interval"));
        }
        Ok(Parsed {
            start,
            end,
            duration: end.checked_sub(start)?,
            settings,
        })
    }

    pub fn poll(
        &mut self,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Poll<Parsed>> {
        loop {
            cancelled(cancel)?;
            let scalar = match self.cursor.poll(bytes, work, cancel)? {
                Poll::Yield => return Ok(Poll::Yield),
                Poll::End => {
                    match self.phase {
                        Phase::End => self.end = Some(self.timestamp.finish()?),
                        Phase::Settings => {}
                        _ => return Err(Error::Invalid("incomplete cue timing line")),
                    }
                    return self.result(None).map(Poll::Ready);
                }
                Poll::Ready(scalar) => scalar,
            };
            let space = matches!(scalar.value, ' ' | '\t');
            match self.phase {
                Phase::Start if space => {
                    self.start = Some(self.timestamp.finish()?);
                    self.phase = Phase::BeforeArrow;
                }
                Phase::Start => self.timestamp.push(scalar.value)?,
                Phase::BeforeArrow if space => {}
                Phase::BeforeArrow if scalar.value == '-' => self.phase = Phase::ArrowSecond,
                Phase::ArrowSecond if scalar.value == '-' => self.phase = Phase::ArrowThird,
                Phase::ArrowThird if scalar.value == '>' => self.phase = Phase::AfterArrow,
                Phase::AfterArrow if space => self.phase = Phase::BeforeEnd,
                Phase::BeforeEnd if space => {}
                Phase::BeforeEnd => {
                    self.timestamp = Timestamp::new(self.comma);
                    self.timestamp.push(scalar.value)?;
                    self.phase = Phase::End;
                }
                Phase::End if space => {
                    self.end = Some(self.timestamp.finish()?);
                    self.phase = Phase::Settings;
                }
                Phase::End => self.timestamp.push(scalar.value)?,
                Phase::Settings if space => {}
                Phase::Settings if !self.comma => {
                    let settings = Field {
                        owner: FieldOwner::Payload,
                        range: scalar.start..self.line_end,
                    };
                    return self.result(Some(settings)).map(Poll::Ready);
                }
                _ => return Err(Error::Invalid("cue timing separator or settings")),
            }
        }
    }
}
