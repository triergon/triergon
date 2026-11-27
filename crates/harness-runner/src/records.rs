//! Records: a kind and its values, which an operation hands to a [`Sink`] in the order
//! its reference worker produces them. [`Totals`] folds them into the work digest;
//! [`Dump`] prints each as one line, the kind's name and then its values.

use std::io::{self, Write};
use triergon_harness_protocol::digest::{fold, fold_framed, signed};

use crate::Totals;

/// The digest value of an absent value, which the dump prints as `-`.
pub const ABSENT: u64 = u64::MAX;

/// The FNV-1a offset basis, the hash of no bytes.
pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// `hash` extended by every byte of `data` (FNV-1a).
#[inline]
#[must_use]
pub fn fnv(mut hash: u64, data: &[u8]) -> u64 {
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// What a record reports: the tag the digest folds and the name the dump prints.
pub trait Kind: Copy {
    /// The tag the digest folds before the values.
    fn tag(self) -> u64;
    /// The word the dump prints before the values.
    fn name(self) -> &'static str;
    /// Whether a record of this kind counts as one packet of the totals.
    fn counted(self) -> bool {
        true
    }
}

/// Consumes the records of one operation: each a kind, then values, then its end.
/// A method fails only when the dump's writer does.
pub trait Sink<K: Kind> {
    /// Start a record of `kind`.
    fn begin(&mut self, kind: K) -> io::Result<()>;
    /// An integer value.
    fn int(&mut self, value: i64) -> io::Result<()>;
    /// A double, folded as its bits and printed as 16 hex digits.
    fn real(&mut self, value: f64) -> io::Result<()>;
    /// A byte string, folded as its length and every byte, printed quoted, and
    /// counted as bytes of the totals.
    fn text(&mut self, value: &[u8]) -> io::Result<()>;
    /// An absent value: [`ABSENT`] in the digest, `-` in the dump.
    fn absent(&mut self) -> io::Result<()>;
    /// End the record.
    fn end(&mut self) -> io::Result<()>;
    /// Count packets and bytes of output that no record's kind or text counts.
    fn count(&mut self, packets: usize, bytes: usize);

    /// Integer values, in order.
    fn ints(&mut self, values: &[i64]) -> io::Result<()> {
        values.iter().try_for_each(|&v| self.int(v))
    }

    /// Doubles, in order.
    fn reals(&mut self, values: &[f64]) -> io::Result<()> {
        values.iter().try_for_each(|&v| self.real(v))
    }

    /// A flag, as the integer 0 or 1.
    fn flag(&mut self, value: bool) -> io::Result<()> {
        self.int(value.into())
    }

    /// A byte string, or absent.
    fn optional(&mut self, value: Option<&[u8]>) -> io::Result<()> {
        match value {
            Some(value) => self.text(value),
            None => self.absent(),
        }
    }
}

impl<K: Kind> Sink<K> for Totals {
    fn begin(&mut self, kind: K) -> io::Result<()> {
        if kind.counted() {
            self.packets += 1;
        }
        fold(&mut self.digest, kind.tag());
        Ok(())
    }

    #[inline]
    fn int(&mut self, value: i64) -> io::Result<()> {
        fold(&mut self.digest, signed(value));
        Ok(())
    }

    #[inline]
    fn real(&mut self, value: f64) -> io::Result<()> {
        fold(&mut self.digest, value.to_bits());
        Ok(())
    }

    fn text(&mut self, value: &[u8]) -> io::Result<()> {
        self.bytes += value.len() as u64;
        fold_framed(&mut self.digest, value);
        Ok(())
    }

    fn absent(&mut self) -> io::Result<()> {
        fold(&mut self.digest, ABSENT);
        Ok(())
    }

    fn end(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn count(&mut self, packets: usize, bytes: usize) {
        self.packets += packets as u64;
        self.bytes += bytes as u64;
    }
}

/// The readable form of records: one line each, `name value value ...`, an absent
/// value as `-` and a byte string quoted with `\xHH` for anything but printable
/// ASCII other than `"` and `\`.
#[derive(Debug)]
pub struct Dump<W: Write>(pub W);

impl<K: Kind, W: Write> Sink<K> for Dump<W> {
    fn begin(&mut self, kind: K) -> io::Result<()> {
        write!(self.0, "{}", kind.name())
    }

    fn int(&mut self, value: i64) -> io::Result<()> {
        write!(self.0, " {value}")
    }

    fn real(&mut self, value: f64) -> io::Result<()> {
        write!(self.0, " {:016x}", value.to_bits())
    }

    fn text(&mut self, value: &[u8]) -> io::Result<()> {
        self.0.write_all(b" \"")?;
        for &c in value {
            if (0x20..0x7f).contains(&c) && c != b'"' && c != b'\\' {
                self.0.write_all(&[c])?;
            } else {
                write!(self.0, "\\x{c:02x}")?;
            }
        }
        self.0.write_all(b"\"")
    }

    fn absent(&mut self) -> io::Result<()> {
        write!(self.0, " -")
    }

    fn end(&mut self) -> io::Result<()> {
        writeln!(self.0)
    }

    fn count(&mut self, _packets: usize, _bytes: usize) {}
}
