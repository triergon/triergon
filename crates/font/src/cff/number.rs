//! The integer encodings DICT data and charstrings share (TN 5176 Table 3,
//! TN 5177 3.2).

use crate::read::Reader;
use triergon_core::Result;

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

#[cfg(test)]
mod tests {
    use super::shared_integer;
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
}
