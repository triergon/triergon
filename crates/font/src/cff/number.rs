//! Numbers in DICT data and charstrings (TN 5176 Table 3, TN 5177 3.2): the
//! integer encodings both share, and DICT numbers read into 16.16 as FreeType's
//! DICT parser reads them (`cff_parse_integer`, `cff_parse_real`, `do_fixed`,
//! `cff_parse_fixed_dynamic` and `cff_parse_num`).

use crate::fixed::div_fix;
use crate::read::Reader;
use triergon_core::{Error, Result};

pub(super) const POWER_TENS: [i64; 10] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

/// `10^n`; FreeType's readings keep `n` within the table.
fn ten(n: i64) -> i64 {
    POWER_TENS[usize::try_from(n).unwrap_or(0).min(9)]
}

/// The integer that `byte` introduces: 28 is a 16-bit integer, 32 to 246 one byte,
/// 247 to 254 two bytes. `None` for the bytes whose meaning differs between DICT
/// data and charstrings.
pub(super) fn shared_integer(byte: u8, r: &mut Reader<'_>) -> Result<Option<i32>> {
    Ok(Some(match byte {
        28 => i32::from(r.i16()?),
        32..=246 => i32::from(byte) - 139,
        247..=250 => (i32::from(byte) - 247) * 256 + i32::from(r.u8()?) + 108,
        251..=254 => -(i32::from(byte) - 251) * 256 - i32::from(r.u8()?) - 108,
        _ => return Ok(None),
    }))
}

/// `cff_parse_integer`: a DICT integer. FreeType assembles the five-byte form
/// in an unsigned long, so its negative values read as large positive ones.
fn dict_integer(byte: u8, r: &mut Reader<'_>) -> Result<i64> {
    match byte {
        29 => Ok(i64::from(r.u32()?)),
        _ => shared_integer(byte, r)?
            .map(i64::from)
            .ok_or(Error::Invalid("reserved CFF DICT byte")),
    }
}

/// The digits of a real number as `cff_parse_real` collects them.
struct Real {
    negative: bool,
    number: i64,
    integer_length: i64,
    fraction_length: i64,
    /// The written exponent plus the digits that did not fit `number`.
    exponent: i64,
    /// The written exponent ran past 1000: an overflow, or an underflow when
    /// it is negative.
    overflow: Option<bool>,
}

impl Real {
    /// The nibbles after a real's 30 byte.
    fn read(r: &mut Reader<'_>) -> Self {
        let mut nibbles = Nibbles { r, low: None };
        let (mut number, mut exponent, mut exponent_add) = (0_i64, 0_i64, 0_i64);
        let (mut integer_length, mut fraction_length) = (0_i64, 0_i64);
        let mut negative = false;
        let mut nib;
        loop {
            nib = nibbles.next();
            match nib {
                0xe => negative = true,
                10.. => break,
                _ if number >= 0x0ccc_cccc => exponent_add += 1,
                _ if nib != 0 || number != 0 => {
                    integer_length += 1;
                    number = number * 10 + nib;
                }
                _ => {}
            }
        }
        if nib == 0xa {
            loop {
                nib = nibbles.next();
                if nib >= 10 {
                    break;
                }
                if nib == 0 && number == 0 {
                    exponent_add -= 1;
                } else if number < 0x0ccc_cccc && fraction_length < 9 {
                    fraction_length += 1;
                    number = number * 10 + nib;
                }
            }
        }
        let mut overflow = false;
        let negative_exponent = nib == 0xc;
        if nib == 0xb || nib == 0xc {
            loop {
                nib = nibbles.next();
                if nib >= 10 {
                    break;
                }
                if exponent > 1000 {
                    overflow = true;
                } else {
                    exponent = exponent * 10 + nib;
                }
            }
            if negative_exponent {
                exponent = -exponent;
            }
        }
        Self {
            negative,
            number,
            integer_length,
            fraction_length,
            exponent: exponent + exponent_add,
            overflow: overflow.then_some(negative_exponent),
        }
    }

    const fn sign(&self, v: i64) -> i64 {
        if self.negative { -v } else { v }
    }

    /// `cff_parse_real` without a scale: 16.16, times `10^power_ten`.
    fn fixed(self, power_ten: i64) -> i64 {
        if self.number == 0 {
            return 0;
        }
        match self.overflow {
            Some(true) => return 0,
            Some(false) => return self.sign(0x7fff_ffff),
            None => {}
        }
        let exponent = self.exponent + power_ten;
        let integer_length = self.integer_length + exponent;
        let mut fraction_length = self.fraction_length - exponent;
        if integer_length > 5 {
            return self.sign(0x7fff_ffff);
        }
        if integer_length < -5 {
            return 0;
        }
        let mut number = self.number;
        // Non-significant digits.
        if integer_length < 0 {
            number /= ten(-integer_length);
            fraction_length += integer_length;
        }
        if fraction_length == 10 {
            number /= 10;
            fraction_length -= 1;
        }
        let result = if fraction_length > 0 {
            if number / ten(fraction_length) > 0x7fff {
                0
            } else {
                div_fix(number, ten(fraction_length))
            }
        } else {
            number *= ten(-fraction_length);
            if number > 0x7fff {
                return self.sign(0x7fff_ffff);
            }
            number << 16
        };
        self.sign(result)
    }

    /// `cff_parse_real` with a dynamic scale: the significant digits as 16.16
    /// and the power of ten that scales them. Exponents past 1000 saturate.
    fn dynamic(self) -> (i64, i64) {
        if self.number == 0 {
            return (0, 0);
        }
        if let Some(negative_exponent) = self.overflow {
            return (
                self.sign(if negative_exponent { 0 } else { 0x7fff_ffff }),
                0,
            );
        }
        let mut number = self.number;
        let mut exponent = self.exponent + self.integer_length;
        let fraction_length = self.fraction_length + self.integer_length;
        let (result, scaling) = if fraction_length <= 5 {
            if number > 0x7fff {
                (div_fix(number, 10), exponent - fraction_length + 1)
            } else {
                if exponent > 0 {
                    let shift = exponent.min(5) - fraction_length;
                    if shift > 0 {
                        exponent -= exponent.min(5);
                        number *= ten(shift);
                        if number > 0x7fff {
                            number /= 10;
                            exponent += 1;
                        }
                    } else {
                        exponent -= fraction_length;
                    }
                } else {
                    exponent -= fraction_length;
                }
                (number << 16, exponent)
            }
        } else if number / ten(fraction_length - 5) > 0x7fff {
            (div_fix(number, ten(fraction_length - 4)), exponent - 4)
        } else {
            (div_fix(number, ten(fraction_length - 5)), exponent - 5)
        };
        (self.sign(result), scaling)
    }
}

/// The nibbles of a real, high first. The DICT parser checked that the real
/// ends; a read past the operands would end it too.
struct Nibbles<'r, 'a> {
    r: &'r mut Reader<'a>,
    low: Option<i64>,
}

impl Nibbles<'_, '_> {
    fn next(&mut self) -> i64 {
        if let Some(low) = self.low.take() {
            return low;
        }
        match self.r.u8() {
            Ok(byte) => {
                self.low = Some(i64::from(byte & 0xf));
                i64::from(byte >> 4)
            }
            Err(_) => 0xf,
        }
    }
}

/// `cff_parse_fixed_dynamic`: a DICT number as a 16.16 value and the power of
/// ten that scales it, keeping as many digits as fit.
pub(super) fn fixed_dynamic(r: &mut Reader<'_>) -> Result<(i64, i64)> {
    let byte = r.u8()?;
    let number = match byte {
        30 => return Ok(Real::read(r).dynamic()),
        29 => i64::from(r.i32()?),
        _ => shared_integer(byte, r)?
            .map(i64::from)
            .ok_or(Error::Invalid("reserved CFF DICT byte"))?,
    };
    if number <= 0x7fff {
        return Ok((number.wrapping_shl(16), 0));
    }
    let length = (5..10).find(|&n| number < POWER_TENS[n]).unwrap_or(10);
    Ok(if number / POWER_TENS[length - 5] > 0x7fff {
        (div_fix(number, POWER_TENS[length - 4]), (length - 4) as i64)
    } else {
        (div_fix(number, POWER_TENS[length - 5]), (length - 5) as i64)
    })
}

/// A DICT operand as FreeType's parser holds it: its encoding, or the 16.16
/// result of a CFF2 `blend`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Operand<'a> {
    /// The bytes from the operand's first on.
    Encoded(&'a [u8]),
    Blended(i64),
}

impl Default for Operand<'_> {
    fn default() -> Self {
        Self::Blended(0)
    }
}

impl Operand<'_> {
    /// `do_fixed`: the operand in 16.16, times `10^scaling`, saturating as
    /// FreeType saturates.
    pub(crate) fn fixed(self, scaling: usize) -> i64 {
        let power = POWER_TENS[scaling.min(9)];
        let limit = i64::MAX / power;
        let saturated = |v: i64| if v > 0 { 0x7fff_ffff } else { -0x7fff_ffff };
        match self {
            Self::Encoded(bytes) => {
                let mut r = Reader::new(bytes);
                let Ok(byte) = r.u8() else { return 0 };
                if byte == 30 {
                    return Real::read(&mut r).fixed(scaling as i64);
                }
                let Ok(mut value) = dict_integer(byte, &mut r) else {
                    return 0;
                };
                if scaling > 0 {
                    if value.abs() << 16 > limit {
                        return saturated(value);
                    }
                    value *= power;
                }
                if (-0x7fff..=0x7fff).contains(&value) {
                    value << 16
                } else {
                    saturated(value)
                }
            }
            Self::Blended(sum) => {
                // The blend's result is pushed as a 32-bit number.
                let value = i64::from(sum as i32);
                if scaling > 0 {
                    if value.abs() > limit {
                        return saturated(value);
                    }
                    return value * power;
                }
                value
            }
        }
    }

    /// The operand as a count, index or offset: a nonnegative whole number
    /// below 2^32, from the five-byte form read signed.
    pub(crate) fn index(self) -> Result<usize> {
        let value = match self {
            Self::Encoded(bytes) => {
                let mut r = Reader::new(bytes);
                match r.u8()? {
                    30 => {
                        let fixed = Real::read(&mut r).fixed(0);
                        (fixed & 0xffff == 0).then_some(fixed >> 16)
                    }
                    29 => Some(i64::from(r.i32()?)),
                    byte => shared_integer(byte, &mut r)?.map(i64::from),
                }
            }
            Self::Blended(sum) => (sum & 0xffff == 0).then_some(sum >> 16),
        };
        value
            .and_then(|v| u32::try_from(v).ok())
            .and_then(|v| usize::try_from(v).ok())
            .ok_or(Error::Invalid("CFF operand is not an index"))
    }

    /// `cff_parse_num`: the operand as an integer, a real truncated toward
    /// negative infinity and a blend rounded to 16 bits.
    pub(crate) fn integer(self) -> i64 {
        match self {
            Self::Encoded(bytes) => {
                let mut r = Reader::new(bytes);
                match r.u8() {
                    Ok(30) => Real::read(&mut r).fixed(0) >> 16,
                    Ok(byte) => dict_integer(byte, &mut r).unwrap_or(0),
                    Err(_) => 0,
                }
            }
            Self::Blended(sum) => {
                let rounded = (((sum as u32) >> 8) + 0x80) >> 8;
                i64::from((rounded as u16).cast_signed())
            }
        }
    }
}

/// The length of the DICT number `bytes` start with, `None` for an operator or
/// a number cut short.
pub(super) fn number_length(bytes: &[u8]) -> Option<usize> {
    let len = match *bytes.first()? {
        28 => 3,
        29 => 5,
        30 => {
            1 + bytes[1..]
                .iter()
                .position(|&b| b >> 4 == 0xf || b & 0xf == 0xf)?
                + 1
        }
        32..=246 => 1,
        247..=254 => 2,
        _ => return None,
    };
    (len <= bytes.len()).then_some(len)
}

#[cfg(test)]
mod tests {
    use super::{Operand, shared_integer};
    use crate::read::Reader;

    #[test]
    fn one_two_and_three_byte_integers() {
        let decode =
            |bytes: &[u8]| shared_integer(bytes[0], &mut Reader::new(&bytes[1..])).unwrap();
        assert_eq!(decode(&[32]), Some(-107));
        assert_eq!(decode(&[246]), Some(107));
        assert_eq!(decode(&[247, 0]), Some(108));
        assert_eq!(decode(&[250, 255]), Some(1131));
        assert_eq!(decode(&[251, 0]), Some(-108));
        assert_eq!(decode(&[254, 255]), Some(-1131));
        assert_eq!(decode(&[28, 0x80, 0]), Some(-32768));
        assert_eq!(decode(&[29, 0, 0, 0, 1]), None);
        assert_eq!(decode(&[255, 0, 1, 0, 0]), None);
    }

    #[test]
    fn dict_operands_read_as_freetype_reads_them() {
        let operand = |bytes: &'static [u8]| Operand::Encoded(bytes);
        // 0.039625 (BlueScale) times 1000 in 16.16: 39.625 → 2596864;
        // -10, and 0.5 truncated to an integer.
        assert_eq!(
            operand(&[30, 0x0a, 0x03, 0x96, 0x25, 0xff]).fixed(3),
            2596864
        );
        assert_eq!(operand(&[129]).fixed(0), -10 << 16);
        assert_eq!(operand(&[30, 0x0a, 0x5f]).integer(), 0);
        // Out of range integers saturate; the five-byte form is unsigned.
        let large = operand(&[29, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(
            (large.fixed(0), large.integer()),
            (0x7fff_ffff, 0xffff_ffff)
        );
        // A blend in 16.16: 12.5 rounds to 13 as a number.
        let blended = Operand::Blended(12 << 16 | 0x8000);
        assert_eq!((blended.fixed(0), blended.integer()), (819200, 13));
        // Indices are whole and nonnegative.
        assert_eq!(operand(&[239]).index(), Ok(100));
        assert_eq!(operand(&[30, 0x3a, 0x0f]).index(), Ok(3));
        assert!(operand(&[30, 0x3a, 0x5f]).index().is_err());
        assert!(operand(&[129]).index().is_err());
        assert!(operand(&[29, 0xff, 0xff, 0xff, 0xff]).index().is_err());
        assert!(Operand::Blended(1 << 15).index().is_err());
    }
}
