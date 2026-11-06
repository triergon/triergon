//! Minimal EBML writing and walking for authored Matroska fixtures.
//!
//! Written from RFC 8794 independently of the library's reader, so fixtures do
//! not inherit its interpretation.

/// An EBML variable-size integer of `width` bytes, or the shortest width whose
/// all-ones value stays reserved.
pub fn vint(value: u64, width: Option<usize>) -> Vec<u8> {
    let width = width.unwrap_or_else(|| {
        (1..=8)
            .find(|w| value < (1u64 << (7 * w)) - 1)
            .expect("value fits")
    });
    let marked = (1u128 << (7 * width)) | u128::from(value);
    marked.to_be_bytes()[16 - width..].to_vec()
}

/// Big-endian bytes of `value` without leading zero bytes (at least one byte).
pub fn minimal(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = (value.leading_zeros() / 8).min(7) as usize;
    bytes[skip..].to_vec()
}

/// An element: identifier (as written), size, payload.
pub fn element(id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = minimal(u64::from(id));
    out.extend(vint(data.len() as u64, None));
    out.extend_from_slice(data);
    out
}

/// An unsigned-integer element in its shortest encoding.
pub fn uint(id: u32, value: u64) -> Vec<u8> {
    element(id, &minimal(value))
}

/// A signed-integer element of exactly eight bytes.
pub fn int8(id: u32, value: i64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

pub fn float64(id: u32, value: f64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

/// Concatenate byte strings.
pub fn cat<const N: usize>(parts: [&[u8]; N]) -> Vec<u8> {
    parts.concat()
}

/// The EBML header of the authored Matroska fixtures.
pub fn header() -> Vec<u8> {
    let body = [
        uint(0x4286, 1),
        uint(0x42F7, 1),
        uint(0x42F2, 4),
        uint(0x42F3, 8),
        element(0x4282, b"matroska"),
        uint(0x4287, 4),
        uint(0x4285, 2),
    ]
    .concat();
    element(0x1A45DFA3, &body)
}

/// One child element: identifier, payload start, payload end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Child {
    pub id: u32,
    pub start: usize,
    pub end: usize,
}

/// The elements in `data[begin..end]`; unknown sizes extend to `end`.
pub fn children(data: &[u8], mut begin: usize, end: usize) -> Result<Vec<Child>, String> {
    let mut out = Vec::new();
    while begin < end {
        let (id, at, _) = variable(data, begin, true)?;
        let (length, start, unknown) = variable(data, at, false)?;
        let stop = if unknown {
            end
        } else {
            start + length as usize
        };
        if stop > end {
            return Err("element exceeds parent".into());
        }
        out.push(Child {
            id: id as u32,
            start,
            end: stop,
        });
        begin = stop;
    }
    Ok(out)
}

fn variable(data: &[u8], at: usize, identifier: bool) -> Result<(u64, usize, bool), String> {
    let first = *data.get(at).ok_or("truncated VINT")?;
    if first == 0 {
        return Err("zero EBML marker".into());
    }
    let width = first.leading_zeros() as usize + 1;
    let end = at + width;
    let bytes = data.get(at..end).ok_or("truncated VINT")?;
    let mut value = bytes.iter().fold(0u64, |v, &b| (v << 8) | u64::from(b));
    if identifier {
        return Ok((value, end, false));
    }
    value -= 1 << (width * 7);
    Ok((value, end, value == (1 << (width * 7)) - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_sizes_and_integers() {
        assert_eq!(vint(0, None), [0x80]);
        assert_eq!(vint(126, None), [0xfe]);
        assert_eq!(vint(127, None), [0x40, 0x7f]);
        assert_eq!(vint(1, Some(2)), [0x40, 0x01]);
        assert_eq!(uint(0xE7, 0), [0xe7, 0x81, 0x00]);
        assert_eq!(
            uint(0x2AD7B1, 1_000_000),
            [0x2a, 0xd7, 0xb1, 0x83, 0x0f, 0x42, 0x40]
        );
    }

    #[test]
    fn walks_what_it_writes() {
        let data = cat([
            &header(),
            &element(0x18538067, &uint(0xE7, 5)),
            b"\x1f\x43\xb6\x75\xff\xe7\x81\x01",
        ]);
        let top = children(&data, 0, data.len()).unwrap();
        assert_eq!(
            top.iter().map(|c| c.id).collect::<Vec<_>>(),
            [0x1A45DFA3, 0x18538067, 0x1F43B675]
        );
        assert_eq!(top[2].end, data.len());
        assert_eq!(
            children(&data, top[1].start, top[1].end).unwrap()[0],
            Child {
                id: 0xE7,
                start: top[1].start + 2,
                end: top[1].end
            }
        );
        assert!(children(b"\x00", 0, 1).is_err());
    }
}
