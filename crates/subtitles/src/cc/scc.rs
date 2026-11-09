//! Scenarist SCC files read as FFmpeg's `scc` demuxer reads them.
//!
//! Each line of at least 14 characters that starts with a time code
//! `H:M:S:F` (or `;` before the frames; numbers as C's `%d` reads them) becomes
//! one packet of field 1 constructs (`FC` and a byte pair per word) at
//! `(3600 H + 60 M + S) * 1000 + 33 F` milliseconds. Words are read from the
//! thirteenth character on: four characters each, the next word after the next
//! space; a word containing a space, or fewer than four characters left, ends
//! the line. Each character is a hex digit by its distance from `0`, `A` or
//! `a`, so other characters still yield a value. A resume caption loading
//! followed by an erase displayed memory (`9420 942C`) after at least five
//! words of a packet starts a new packet, timed one frame (33 ms) per word into
//! the line. A packet lasts until the next packet's time (the last one 0 ms);
//! packets are ordered by time, packets of the same time in file order, and a
//! packet equal to the one before it in time, duration and data is dropped.

use super::charge;
use super::text::Text;
use triergon_core::{Budget, BudgetVec, Result, WorkBudget};

/// One packet: times in milliseconds and its `cc_data`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SccPacket<'a> {
    pub pts: i64,
    pub duration: i64,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    pts: i64,
    duration: i64,
    start: usize,
    end: usize,
}

/// The packets of one SCC file, in presentation order.
#[derive(Debug)]
pub struct SccPackets {
    data: Text,
    entries: BudgetVec<Entry>,
}

impl SccPackets {
    /// Reads `text`. Charges one work unit per byte.
    ///
    /// # Errors
    ///
    /// When the budget cannot hold the packets or `work` cannot cover `text`.
    pub fn read(text: &[u8], pool: &Budget, work: &mut WorkBudget) -> Result<Self> {
        charge(work, text.len())?;
        let mut packets = Self {
            data: Text::new(pool),
            entries: BudgetVec::new(pool),
        };
        for line in text.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(pts) = time_code(line).filter(|_| line.len() >= 14) else {
                continue;
            };
            let mut words = words(line).peekable();
            let mut start = packets.push_entry(pts)?;
            let mut index = 0i64;
            while let Some(word) = words.next() {
                let in_packet = (packets.data.len() - start) / 3;
                if in_packet >= 5 && word == [0x94, 0x20] && words.peek() == Some(&[0x94, 0x2c]) {
                    start = packets.push_entry(pts + index * 33)?;
                }
                packets.data.put(&[0xfc, word[0], word[1]])?;
                packets.close_entry();
                index += 1;
            }
        }
        let entries = packets.entries.as_mut_slice();
        for i in 1..entries.len() {
            entries[i - 1].duration = entries[i].pts.wrapping_sub(entries[i - 1].pts);
        }
        entries.sort_by_key(|entry| entry.pts);
        packets.drop_repeats();
        Ok(packets)
    }

    /// Drops each packet equal to the one before it in time, duration and data.
    fn drop_repeats(&mut self) {
        let data = self.data.as_bytes();
        let entries = self.entries.as_mut_slice();
        let mut kept = 0;
        for i in 0..entries.len() {
            let entry = entries[i];
            let repeat = kept > 0 && {
                let last = entries[kept - 1];
                last.pts == entry.pts
                    && last.duration == entry.duration
                    && data[last.start..last.end] == data[entry.start..entry.end]
            };
            if !repeat {
                entries[kept] = entry;
                kept += 1;
            }
        }
        while self.entries.len() > kept {
            self.entries.pop();
        }
    }

    /// Starts a packet at `pts`; returns where its data starts.
    fn push_entry(&mut self, pts: i64) -> Result<usize> {
        if self.entries.len() == self.entries.capacity() {
            self.entries.try_reserve_exact(self.entries.len().max(16))?;
        }
        let start = self.data.len();
        self.entries
            .try_push(Entry {
                pts,
                duration: 0,
                start,
                end: start,
            })
            .map_err(|rejected| rejected.error)?;
        Ok(start)
    }

    /// Extends the last packet to the data written.
    fn close_entry(&mut self) {
        let end = self.data.len();
        if let Some(last) = self.entries.as_mut_slice().last_mut() {
            last.end = end;
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = SccPacket<'_>> {
        self.entries.as_slice().iter().map(|entry| SccPacket {
            pts: entry.pts,
            duration: entry.duration,
            data: &self.data.as_bytes()[entry.start..entry.end],
        })
    }
}

/// The time code at the start of `line`, in milliseconds.
fn time_code(line: &[u8]) -> Option<i64> {
    let mut rest = line;
    let hours = decimal(&mut rest)?;
    rest = rest.strip_prefix(b":")?;
    let minutes = decimal(&mut rest)?;
    rest = rest.strip_prefix(b":")?;
    let seconds = decimal(&mut rest)?;
    let (&separator, after) = rest.split_first()?;
    if separator != b':' && separator != b';' {
        return None;
    }
    rest = after;
    let frames = decimal(&mut rest)?;
    let seconds = (hours * 60 + minutes) * 60 + seconds;
    Some(seconds * 1000 + frames * 33)
}

/// A C `%d`: optional white space and sign, then at least one digit.
fn decimal(rest: &mut &[u8]) -> Option<i64> {
    let mut s = *rest;
    while let [b' ' | b'\t' | b'\x0b' | b'\x0c' | b'\r', tail @ ..] = s {
        s = tail;
    }
    let negative = s.first() == Some(&b'-');
    if matches!(s.first(), Some(b'+' | b'-')) {
        s = &s[1..];
    }
    let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let value = s[..digits].iter().fold(0i64, |value, &digit| {
        value.wrapping_mul(10).wrapping_add(i64::from(digit - b'0'))
    });
    *rest = &s[digits..];
    // `%d` stores an `int`.
    let value = i64::from(value as i32);
    Some(if negative { -value } else { value })
}

/// The byte pairs of a line's words.
fn words(line: &[u8]) -> impl Iterator<Item = [u8; 2]> + '_ {
    let mut at = Some(12);
    std::iter::from_fn(move || {
        let mut start = at?;
        while line.get(start) == Some(&b' ') {
            start += 1;
        }
        let word = line.get(start..start + 4).filter(|w| !w.contains(&b' '));
        at = word.and_then(|_| {
            let after = start + 4;
            line[after..]
                .iter()
                .position(|&b| b == b' ')
                .map(|n| after + n)
        });
        word.map(|w| [byte(w[0], w[1]), byte(w[2], w[3])])
    })
}

fn byte(high: u8, low: u8) -> u8 {
    ((digit(high) << 4) | digit(low)) as u8
}

/// A hex digit's value by its distance from `0`, `A` or `a`.
fn digit(c: u8) -> i32 {
    let c = i32::from(c);
    if c >= i32::from(b'a') {
        c - i32::from(b'a') + 10
    } else if c >= i32::from(b'A') {
        c - i32::from(b'A') + 10
    } else {
        c - i32::from(b'0')
    }
}
