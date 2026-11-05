use triergon_core::{Error, Result};

/// Read RBSP prefixes without copying or decoding their uninspected suffixes.
struct Rbsp<'a> {
    rest: &'a [u8],
    zeroes: u8,
    byte: u8,
    bits: u8,
}

impl<'a> Rbsp<'a> {
    fn new(rest: &'a [u8]) -> Self {
        Self {
            rest,
            zeroes: 0,
            byte: 0,
            bits: 0,
        }
    }

    fn bit(&mut self) -> Result<u32> {
        if self.bits == 0 {
            let (&first, rest) = self.rest.split_first().ok_or(Error::Truncated)?;
            let (byte, rest) = if self.zeroes == 2 && first == 3 {
                self.zeroes = 0;
                let (&byte, rest) = rest.split_first().ok_or(Error::Truncated)?;
                if byte > 3 {
                    return Err(Error::Invalid("AVC emulation-prevention byte"));
                }
                (byte, rest)
            } else {
                (first, rest)
            };
            self.rest = rest;
            self.zeroes = if byte == 0 {
                (self.zeroes + 1).min(2)
            } else {
                0
            };
            self.byte = byte;
            self.bits = 8;
        }
        self.bits -= 1;
        Ok(u32::from((self.byte >> self.bits) & 1))
    }

    fn fixed(&mut self, width: u32) -> Result<u32> {
        let mut value = 0;
        for _ in 0..width {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    fn ue(&mut self) -> Result<u32> {
        let mut prefix = 0;
        while self.bit()? == 0 {
            prefix += 1;
            if prefix > 31 {
                return Err(Error::Invalid("AVC Exp-Golomb width"));
            }
        }
        Ok((1 << prefix) - 1 + self.fixed(prefix)?)
    }

    fn bounded(&mut self, maximum: u32) -> Result<u32> {
        let value = self.ue()?;
        if value > maximum {
            return Err(Error::Invalid("AVC RBSP field out of range"));
        }
        Ok(value)
    }

    fn se(&mut self) -> Result<i32> {
        let value = self.ue()?;
        let magnitude = (value / 2) as i32;
        Ok(if value & 1 == 0 {
            -magnitude
        } else {
            magnitude + 1
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcRaster {
    pub width: u32,
    pub height: u32,
}

/// Literal timing fields from AVC SPS VUI (H.264 Annex E.1.1).
/// These fields alone do not establish presentation timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcVuiTiming {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub fixed_frame_rate_flag: bool,
}

pub(crate) fn sps(raw: &[u8]) -> Result<(u8, AvcRaster)> {
    let (id, raster, _) = sps_prefix(&mut Rbsp::new(raw))?;
    Ok((id, raster))
}

pub(crate) fn sps_with_timing(raw: &[u8]) -> Result<(u8, AvcRaster, Option<AvcVuiTiming>)> {
    let mut r = Rbsp::new(raw);
    let (id, raster, _) = sps_prefix(&mut r)?;
    let timing = if r.bit()? != 0 {
        vui_timing(&mut r)?
    } else {
        None
    };
    // The final set bit is rbsp_stop_one_bit. It cannot supply a VUI field.
    if r.rest.is_empty() && u32::from(r.bits) <= r.byte.trailing_zeros() {
        return Err(Error::Truncated);
    }
    Ok((id, raster, timing))
}

/// Progressive startup chain and its signalled reorder limit, when known.
pub(crate) fn startup_sps(raw: &[u8]) -> Result<(u8, u8)> {
    let mut r = Rbsp::new(raw);
    let (id, _, frame_only) = sps_prefix(&mut r)?;
    if !frame_only {
        return Err(Error::Unsupported("AVC discovery field pictures"));
    }
    let constrained_baseline = raw[0] == 66 && raw[1] & 0x40 != 0;
    let reorder = if r.bit()? != 0 {
        vui_timing(&mut r)?;
        vui_reorder(&mut r)?
    } else {
        None
    };
    if r.rest.is_empty() && u32::from(r.bits) <= r.byte.trailing_zeros() {
        return Err(Error::Truncated);
    }
    let reorder = match (constrained_baseline, reorder) {
        (true, None | Some(0)) => 0,
        (false, Some(reorder)) => reorder,
        _ => return Err(Error::Unsupported("AVC discovery reorder declaration")),
    };
    Ok((id, reorder))
}

fn vui_reorder(r: &mut Rbsp<'_>) -> Result<Option<u8>> {
    let nal_hrd = read_hrd(r)?;
    let vcl_hrd = read_hrd(r)?;
    if nal_hrd || vcl_hrd {
        r.bit()?; // low_delay_hrd_flag
    }
    r.bit()?; // pic_struct_present_flag
    if r.bit()? == 0 {
        return Ok(None);
    }
    r.bit()?; // motion_vectors_over_pic_boundaries_flag
    for _ in 0..4 {
        r.ue()?;
    }
    let reorder = r.bounded(16)?;
    let buffering = r.bounded(16)?;
    if reorder > buffering {
        return Err(Error::Invalid("AVC reorder exceeds decoded picture buffer"));
    }
    Ok(Some(reorder as u8))
}

fn read_hrd(r: &mut Rbsp<'_>) -> Result<bool> {
    if r.bit()? == 0 {
        return Ok(false);
    }
    let entries = r.bounded(31)? + 1;
    r.fixed(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..entries {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.bit()?; // cbr_flag
    }
    r.fixed(20)?; // Delay and time-offset length fields.
    Ok(true)
}

fn sps_prefix(r: &mut Rbsp<'_>) -> Result<(u8, AvcRaster, bool)> {
    let profile = r.fixed(8)?;
    r.fixed(16)?;
    let id = r.bounded(31)? as u8;
    let chroma_array = match profile {
        66 | 77 | 88 => 1,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244 => {
            let chroma = r.bounded(3)?;
            let separate_plane = chroma == 3 && r.bit()? != 0;
            r.bounded(6)?;
            r.bounded(6)?;
            r.bit()?;
            if r.bit()? != 0 {
                for index in 0..if chroma == 3 { 12 } else { 8 } {
                    if r.bit()? != 0 {
                        let (mut last, mut next) = (8, 8);
                        for _ in 0..if index < 6 { 16 } else { 64 } {
                            if next != 0 {
                                let delta = r.se()?;
                                if !(-128..=127).contains(&delta) {
                                    return Err(Error::Invalid("AVC scaling list delta"));
                                }
                                next = (last + delta).rem_euclid(256);
                            }
                            if next != 0 {
                                last = next;
                            }
                        }
                    }
                }
            }
            if separate_plane { 0 } else { chroma }
        }
        _ => return Err(Error::Unsupported("AVC SPS profile for raster projection")),
    };
    r.bounded(12)?;
    match r.bounded(2)? {
        0 => {
            r.bounded(12)?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.bounded(255)? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?;
    r.bit()?;
    let mb_width = r.ue()?.checked_add(1).ok_or(Error::Overflow)?;
    let map_height = r.ue()?.checked_add(1).ok_or(Error::Overflow)?;
    let field_factor = 2 - r.bit()?;
    if field_factor == 2 {
        r.bit()?;
    }
    r.bit()?;
    let coded_width = mb_width.checked_mul(16).ok_or(Error::Overflow)?;
    let coded_height = map_height
        .checked_mul(16 * field_factor)
        .ok_or(Error::Overflow)?;
    let (width, height) = if r.bit()? == 0 {
        (coded_width, coded_height)
    } else {
        let crop_x = if matches!(chroma_array, 1 | 2) { 2 } else { 1 };
        let crop_y = if chroma_array == 1 {
            2 * field_factor
        } else {
            field_factor
        };
        let left = r.ue()?;
        let right = r.ue()?;
        let top = r.ue()?;
        let bottom = r.ue()?;
        let crop = |size: u32, first: u32, second: u32, unit: u32| {
            first
                .checked_add(second)
                .and_then(|sum| sum.checked_mul(unit))
                .and_then(|sum| size.checked_sub(sum))
                .ok_or(Error::Invalid("AVC crop exceeds coded raster"))
        };
        (
            crop(coded_width, left, right, crop_x)?,
            crop(coded_height, top, bottom, crop_y)?,
        )
    };
    if width == 0 || height == 0 {
        return Err(Error::Invalid("empty AVC display raster"));
    }
    Ok((id, AvcRaster { width, height }, field_factor == 1))
}

fn vui_timing(r: &mut Rbsp<'_>) -> Result<Option<AvcVuiTiming>> {
    if r.bit()? != 0 && r.fixed(8)? == 255 {
        r.fixed(32)?; // Extended SAR width and height.
    }
    if r.bit()? != 0 {
        r.bit()?; // Overscan.
    }
    if r.bit()? != 0 {
        r.fixed(4)?; // Video format and full-range flag.
        if r.bit()? != 0 {
            r.fixed(24)?; // Colour primaries, transfer, and matrix.
        }
    }
    if r.bit()? != 0 {
        r.ue()?; // Top-field chroma sample location.
        r.ue()?; // Bottom-field chroma sample location.
    }
    if r.bit()? == 0 {
        return Ok(None);
    }
    Ok(Some(AvcVuiTiming {
        num_units_in_tick: r.fixed(32)?,
        time_scale: r.fixed(32)?,
        fixed_frame_rate_flag: r.bit()? != 0,
    }))
}

pub(crate) fn pps(raw: &[u8]) -> Result<(u8, u8)> {
    let mut r = Rbsp::new(raw);
    Ok((r.bounded(255)? as u8, r.bounded(31)? as u8))
}

pub(crate) fn slice_header_prefix(raw: &[u8]) -> Result<(u32, u8)> {
    let (first_mb, _, pps) = slice_picture(raw)?;
    Ok((first_mb, pps))
}

pub(crate) fn slice_picture(raw: &[u8]) -> Result<(u32, u32, u8)> {
    let mut r = Rbsp::new(raw);
    let first_mb = r.ue()?;
    let slice_type = r.bounded(9)?;
    Ok((first_mb, slice_type % 5, r.bounded(255)? as u8))
}
