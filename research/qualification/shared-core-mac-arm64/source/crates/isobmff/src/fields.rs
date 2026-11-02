//! Checked fields inside an already bounded metadata box; no input/storage layer.
use safe_media_core::{Error, Result};

pub(crate) fn slice(bytes: &[u8], at: usize, length: usize) -> Result<&[u8]> {
    let end = at.checked_add(length).ok_or(Error::Overflow)?;
    bytes.get(at..end).ok_or(Error::Truncated)
}
pub(crate) fn u16_at(b: &[u8], p: usize) -> Result<u16> {
    let v = slice(b, p, 2)?;
    Ok(u16::from_be_bytes([v[0], v[1]]))
}
pub(crate) fn i16_at(b: &[u8], p: usize) -> Result<i16> {
    let v = slice(b, p, 2)?;
    Ok(i16::from_be_bytes([v[0], v[1]]))
}
pub(crate) fn u32_at(b: &[u8], p: usize) -> Result<u32> {
    let v = slice(b, p, 4)?;
    Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}
pub(crate) fn i32_at(b: &[u8], p: usize) -> Result<i32> {
    let v = slice(b, p, 4)?;
    Ok(i32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}
pub(crate) fn u64_at(b: &[u8], p: usize) -> Result<u64> {
    let v = slice(b, p, 8)?;
    Ok(u64::from_be_bytes([
        v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7],
    ]))
}
pub(crate) fn i64_at(b: &[u8], p: usize) -> Result<i64> {
    let v = slice(b, p, 8)?;
    Ok(i64::from_be_bytes([
        v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7],
    ]))
}
pub(crate) fn fourcc(b: &[u8], p: usize) -> Result<[u8; 4]> {
    let v = slice(b, p, 4)?;
    Ok([v[0], v[1], v[2], v[3]])
}
pub(crate) fn full(b: &[u8]) -> Result<(u8, u32)> {
    let v = slice(b, 0, 4)?;
    Ok((
        v[0],
        (u32::from(v[1]) << 16) | (u32::from(v[2]) << 8) | u32::from(v[3]),
    ))
}
pub(crate) fn full_zero(b: &[u8]) -> Result<()> {
    if full(b)? != (0, 0) {
        return Err(Error::Unsupported(
            "full box version/flags outside sourced subset",
        ));
    }
    Ok(())
}
pub(crate) fn exact_rows(bytes: &[u8], start: usize, count: u32, stride: usize) -> Result<()> {
    let length = u64::from(count)
        .checked_mul(u64::try_from(stride).map_err(|_| Error::Overflow)?)
        .ok_or(Error::Overflow)?;
    let end = u64::try_from(start)
        .map_err(|_| Error::Overflow)?
        .checked_add(length)
        .ok_or(Error::Overflow)?;
    let available = u64::try_from(bytes.len()).map_err(|_| Error::Overflow)?;
    if end > available {
        return Err(Error::Truncated);
    }
    if end != available {
        return Err(Error::Invalid("table has trailing bytes"));
    }
    Ok(())
}
