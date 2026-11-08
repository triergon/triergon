//! CFF and CFF2 outlines in OpenType (Adobe Technical Note 5176; OpenType `CFF2`).

mod charstring;
mod dict;
mod index;
mod path;

use crate::outline::{Coordinate, Outline};
use crate::read::{Meter, Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::{GlyphId, Limits};
use dict::{CFF_OPERANDS, CFF2_OPERANDS, op};
use index::Index;
use triergon_core::{Budget, BudgetVec, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    Cff1,
    Cff2,
}

/// Format ceilings: FDSelect indices are 8-bit in CFF, 16-bit in CFF2.
const MAX_CFF_FONT_DICTS: u32 = 256;
const MAX_CFF2_FONT_DICTS: u32 = 65536;

#[derive(Clone, Copy, Debug)]
struct Private {
    subrs: Index,
    default_width: f64,
    nominal_width: f64,
    vsindex: usize,
}

/// The charset, which `seac` uses to find its components by standard code.
#[derive(Clone, Copy, Debug)]
enum Charset {
    IsoAdobe,
    /// Expert and Expert Subset: not supported for `seac`.
    Predefined,
    /// An explicit charset at this table offset.
    Custom(usize),
}

#[derive(Clone, Copy, Debug)]
struct FdSelect {
    offset: usize,
    format: u8,
}

/// A parsed CFF or CFF2 table. Object data stays in the font bytes.
#[derive(Debug)]
pub(crate) struct Cff {
    table: Table,
    version: Version,
    charstrings: Index,
    global: Index,
    privates: BudgetVec<Private>,
    fd_select: Option<FdSelect>,
    charset: Charset,
    cid: bool,
    glyph_count: u32,
    /// The FontMatrix in font units, when it is not 1/unitsPerEm.
    matrix: Option<[f64; 6]>,
    /// Region scalars of each ItemVariationData at the default instance.
    scalars: BudgetVec<f64>,
    variation_data: BudgetVec<(u32, u32)>,
}

impl Cff {
    pub(crate) fn parse(
        data: &[u8],
        table: Table,
        version: Version,
        units_per_em: u16,
        budget: &Budget,
        meter: &mut Meter,
    ) -> Result<Self> {
        let bytes = slice(data, table.offset, table.len)?;
        let mut r = Reader::new(bytes);
        let major = r.u8()?;
        r.u8()?; // minor
        let header = usize::from(r.u8()?);
        let mut cff = Self {
            table,
            version,
            charstrings: Index::default(),
            global: Index::default(),
            privates: BudgetVec::new(budget),
            fd_select: None,
            charset: Charset::IsoAdobe,
            cid: false,
            glyph_count: 0,
            matrix: None,
            scalars: BudgetVec::new(budget),
            variation_data: BudgetVec::new(budget),
        };
        let top = match (version, major) {
            (Version::Cff1, 1) => {
                r = r.moved_to(header)?;
                let names = Index::parse(&mut r, version)?;
                let tops = Index::parse(&mut r, version)?;
                if names.len() == 0 || tops.len() == 0 {
                    return Err(Error::Invalid("CFF without a font"));
                }
                Index::parse(&mut r, version)?; // strings
                cff.global = Index::parse(&mut r, version)?;
                tops.get(bytes, 0)?
            }
            (Version::Cff2, 2) => {
                let size = usize::from(r.u16()?);
                r = r.moved_to(header)?;
                let top = r.take(size)?;
                cff.global = Index::parse(&mut r, version)?;
                top
            }
            _ => return Err(Error::Invalid("CFF major version")),
        };
        let top = cff.top_dict(top, meter)?;
        let mut charstrings = Reader::at(
            bytes,
            top.charstrings
                .ok_or(Error::Invalid("CFF without CharStrings"))?,
        )?;
        cff.charstrings = Index::parse(&mut charstrings, version)?;
        cff.glyph_count = cff.charstrings.len().min(u32::from(u16::MAX));
        if cff.glyph_count == 0 {
            return Err(Error::Invalid("CFF without glyphs"));
        }
        if top.charstring_type != 2 {
            return Err(Error::Unsupported("CFF CharstringType other than 2"));
        }
        if top.synthetic {
            return Err(Error::Unsupported("synthetic CFF font"));
        }
        if let Some(vstore) = top.vstore {
            cff.variation_store(bytes, vstore, meter)?;
        }
        cff.matrix = top.matrix.and_then(|m| font_units(m, units_per_em));
        cff.cid = top.ros;
        cff.charset = match top.charset {
            0 => Charset::IsoAdobe,
            1 | 2 => Charset::Predefined,
            offset => Charset::Custom(offset),
        };
        let font_dicts = top.ros || version == Version::Cff2;
        if font_dicts {
            let mut array = Reader::at(
                bytes,
                top.fd_array.ok_or(Error::Invalid("CFF without FDArray"))?,
            )?;
            let fonts = Index::parse(&mut array, version)?;
            let ceiling = match version {
                Version::Cff1 => MAX_CFF_FONT_DICTS,
                Version::Cff2 => MAX_CFF2_FONT_DICTS,
            };
            if fonts.len() == 0 || fonts.len() > ceiling {
                return Err(Error::Invalid("CFF FDArray size"));
            }
            meter.charge(usize::try_from(fonts.len()).map_err(|_| Error::Overflow)?)?;
            cff.privates
                .try_reserve_exact(usize::try_from(fonts.len()).map_err(|_| Error::Overflow)?)?;
            for i in 0..fonts.len() {
                let font = fonts.get(bytes, i)?;
                let mut location = None;
                cff.dict(font, meter, |operator, operands| {
                    if operator == op::PRIVATE {
                        location = Some(private_location(operands)?);
                    }
                    Ok(())
                })?;
                let private = cff.private_dict(bytes, location, meter)?;
                cff.privates.try_push(private).map_err(|e| e.error)?;
            }
            match top.fd_select {
                Some(offset) => cff.fd_select = Some(cff.check_fd_select(bytes, offset)?),
                None if fonts.len() == 1 => {}
                None => return Err(Error::Invalid("CFF without FDSelect")),
            }
        } else {
            let private = cff.private_dict(bytes, top.private, meter)?;
            cff.privates.try_push(private).map_err(|e| e.error)?;
        }
        Ok(cff)
    }

    pub(crate) const fn glyph_count(&self) -> u32 {
        self.glyph_count
    }

    pub(crate) const fn is_cid_keyed(&self) -> bool {
        self.cid
    }

    pub(crate) const fn version(&self) -> Version {
        self.version
    }

    fn dict(
        &self,
        bytes: &[u8],
        meter: &mut Meter,
        apply: impl FnMut(u16, &[f64]) -> Result<()>,
    ) -> Result<()> {
        let scalars = |i| self.region_scalars(i);
        match self.version {
            Version::Cff1 => dict::parse::<CFF_OPERANDS>(bytes, meter, &scalars, apply),
            Version::Cff2 => dict::parse::<CFF2_OPERANDS>(bytes, meter, &scalars, apply),
        }
    }

    fn top_dict(&self, bytes: &[u8], meter: &mut Meter) -> Result<Top> {
        let mut top = Top {
            charstring_type: 2,
            ..Top::default()
        };
        self.dict(bytes, meter, |operator, operands| {
            match operator {
                op::CHAR_STRINGS => top.charstrings = Some(dict::single(operands)?),
                op::CHARSET => top.charset = dict::single(operands)?,
                op::PRIVATE => top.private = Some(private_location(operands)?),
                op::FD_ARRAY => top.fd_array = Some(dict::single(operands)?),
                op::FD_SELECT => top.fd_select = Some(dict::single(operands)?),
                op::VSTORE => top.vstore = Some(dict::single(operands)?),
                op::ROS => top.ros = true,
                op::SYNTHETIC_BASE => top.synthetic = true,
                op::CHARSTRING_TYPE => top.charstring_type = dict::single(operands)?,
                op::FONT_MATRIX => {
                    let &[a, b, c, d, e, f] = operands else {
                        return Err(Error::Invalid("CFF FontMatrix"));
                    };
                    top.matrix = Some([a, b, c, d, e, f]);
                }
                _ => {}
            }
            Ok(())
        })?;
        Ok(top)
    }

    /// The Private DICT at `location` (size, offset); CFF2 Font DICTs may omit it.
    fn private_dict(
        &self,
        table: &[u8],
        location: Option<(usize, usize)>,
        meter: &mut Meter,
    ) -> Result<Private> {
        let mut private = Private {
            subrs: Index::default(),
            default_width: 0.0,
            nominal_width: 0.0,
            vsindex: 0,
        };
        let Some((size, offset)) = location else {
            return match self.version {
                Version::Cff2 => Ok(private),
                Version::Cff1 => Err(Error::Invalid("CFF without a Private DICT")),
            };
        };
        let bytes = slice(table, offset, size)?;
        let mut subrs = None;
        self.dict(bytes, meter, |operator, operands| {
            match operator {
                op::SUBRS => subrs = Some(dict::single(operands)?),
                op::DEFAULT_WIDTH_X => private.default_width = first(operands)?,
                op::NOMINAL_WIDTH_X => private.nominal_width = first(operands)?,
                op::VSINDEX => private.vsindex = dict::single(operands)?,
                _ => {}
            }
            Ok(())
        })?;
        if self.version == Version::Cff2 {
            self.region_scalars(private.vsindex)?;
        }
        if let Some(relative) = subrs {
            let at = offset.checked_add(relative).ok_or(Error::Overflow)?;
            private.subrs = Index::parse(&mut Reader::at(table, at)?, self.version)?;
        }
        Ok(private)
    }

    /// Checks the FDSelect header; entries are read per glyph.
    fn check_fd_select(&self, table: &[u8], offset: usize) -> Result<FdSelect> {
        let format = *table
            .get(offset)
            .ok_or(Error::Invalid("CFF FDSelect outside the table"))?;
        let needed = match format {
            0 => Some(1 + self.glyph_count as usize),
            3 => u16_at(table, offset + 1).map(|n| 5 + 3 * usize::from(n)),
            4 if self.version == Version::Cff2 => u32_at(table, offset + 1)
                .and_then(|n| usize::try_from(n).ok()?.checked_mul(6)?.checked_add(9)),
            _ => return Err(Error::Invalid("CFF FDSelect format")),
        };
        if needed.is_none_or(|n| offset.checked_add(n).is_none_or(|end| end > table.len())) {
            return Err(Error::Invalid("CFF FDSelect outside the table"));
        }
        Ok(FdSelect { offset, format })
    }

    /// The Private DICT of `glyph`.
    fn private(&self, table: &[u8], glyph: u32) -> Result<&Private> {
        let fd = match self.fd_select {
            None => 0,
            Some(select) => fd_index(table, select, glyph)
                .ok_or(Error::Invalid("glyph outside CFF FDSelect"))?,
        };
        self.privates
            .as_slice()
            .get(fd)
            .ok_or(Error::Invalid("CFF FDSelect index outside FDArray"))
    }

    fn variation_store(&mut self, table: &[u8], offset: usize, meter: &mut Meter) -> Result<()> {
        // A 16-bit length precedes the ItemVariationStore.
        let start = offset.checked_add(2).ok_or(Error::Overflow)?;
        let mut r = Reader::at(table, start)?;
        if r.u16()? != 1 {
            return Err(Error::Invalid("ItemVariationStore format"));
        }
        let regions = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        let count = r.u16()?;
        meter.charge(usize::from(count))?;
        let mut list = Reader::at(table, start.checked_add(regions).ok_or(Error::Overflow)?)?;
        let axes = usize::from(list.u16()?);
        let region_count = list.u16()?;
        let records = list.offset();
        self.variation_data.try_reserve_exact(usize::from(count))?;
        for _ in 0..count {
            let data_offset = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
            let mut item = Reader::at(
                table,
                start.checked_add(data_offset).ok_or(Error::Overflow)?,
            )?;
            item.skip(4)?; // itemCount, wordDeltaCount
            let indices = item.u16()?;
            meter.charge(usize::from(indices) * axes)?;
            let first = u32::try_from(self.scalars.len()).map_err(|_| Error::Overflow)?;
            self.scalars.try_reserve_exact(usize::from(indices))?;
            for _ in 0..indices {
                let region = item.u16()?;
                if region >= region_count {
                    return Err(Error::Invalid("variation region index"));
                }
                let at = usize::from(region)
                    .checked_mul(axes * 6)
                    .and_then(|n| n.checked_add(records))
                    .ok_or(Error::Overflow)?;
                let mut axis = Reader::at(table, at)?;
                let mut scalar = 1.0;
                for _ in 0..axes {
                    let (start, peak, end) = (axis.i16()?, axis.i16()?, axis.i16()?);
                    // At the default instance every coordinate is zero: an axis whose
                    // valid, non-straddling range peaks away from zero excludes the
                    // region (OpenType "Item variation store", region scalars).
                    let ignored = start > peak || peak > end || peak == 0 || (start < 0 && end > 0);
                    if !ignored {
                        scalar = 0.0;
                    }
                }
                self.scalars.try_push(scalar).map_err(|e| e.error)?;
            }
            self.variation_data
                .try_push((first, u32::from(indices)))
                .map_err(|e| e.error)?;
        }
        Ok(())
    }

    fn region_scalars(&self, vsindex: usize) -> Result<&[f64]> {
        let &(first, len) = self
            .variation_data
            .as_slice()
            .get(vsindex)
            .ok_or(Error::Invalid("CFF2 vsindex outside the variation store"))?;
        let (first, len) = (first as usize, len as usize);
        Ok(&self.scalars.as_slice()[first..first + len])
    }

    /// The glyph whose charset SID is `sid`, for `seac`.
    fn glyph_for_sid(&self, table: &[u8], sid: u16) -> Option<u32> {
        match self.charset {
            Charset::IsoAdobe => {
                (u32::from(sid) < self.glyph_count && sid <= 228).then_some(u32::from(sid))
            }
            Charset::Predefined => None,
            Charset::Custom(offset) => {
                if sid == 0 {
                    return Some(0);
                }
                let mut r = Reader::at(table, offset).ok()?;
                let format = r.u8().ok()?;
                let mut glyph = 1_u32;
                while glyph < self.glyph_count {
                    let first = r.u16().ok()?;
                    let left = match format {
                        0 => 0,
                        1 => u16::from(r.u8().ok()?),
                        2 => r.u16().ok()?,
                        _ => return None,
                    };
                    if (first..=first.saturating_add(left)).contains(&sid) {
                        return Some(glyph + u32::from(sid - first));
                    }
                    glyph += u32::from(left) + 1;
                }
                None
            }
        }
    }

    /// Whether the FontMatrix maps font units other than by 1/unitsPerEm.
    pub(crate) const fn has_matrix(&self) -> bool {
        self.matrix.is_some()
    }

    /// Loads `glyph` into `outline` in whole font units as FreeType's CFF engine
    /// builds it, before the FontMatrix.
    pub(crate) fn outline_units<C: Coordinate>(
        &self,
        data: &[u8],
        glyph: GlyphId,
        outline: &mut Outline<C>,
        limits: &Limits,
        meter: &mut Meter,
    ) -> Result<()> {
        let table = self.table.bytes(data);
        charstring::outline(self, table, glyph.0, outline, limits, meter)
    }

    /// Loads `glyph` into `outline` in font units, the FontMatrix applied.
    pub(crate) fn outline(
        &self,
        data: &[u8],
        glyph: GlyphId,
        outline: &mut Outline,
        limits: &Limits,
        meter: &mut Meter,
    ) -> Result<()> {
        self.outline_units(data, glyph, outline, limits, meter)?;
        if let Some([a, b, c, d, e, f]) = self.matrix {
            for p in outline.points_mut() {
                let (x, y) = (f64::from(p.point.x), f64::from(p.point.y));
                p.point.x = (a * x + c * y + e) as f32;
                p.point.y = (b * x + d * y + f) as f32;
                if !p.point.x.is_finite() || !p.point.y.is_finite() {
                    return Err(Error::Invalid("glyph coordinate outside f32"));
                }
            }
        }
        Ok(())
    }
}

/// The Top DICT entries this reader uses.
#[derive(Debug, Default)]
struct Top {
    charstrings: Option<usize>,
    charset: usize,
    private: Option<(usize, usize)>,
    fd_array: Option<usize>,
    fd_select: Option<usize>,
    vstore: Option<usize>,
    ros: bool,
    synthetic: bool,
    charstring_type: usize,
    matrix: Option<[f64; 6]>,
}

fn private_location(operands: &[f64]) -> Result<(usize, usize)> {
    match operands {
        [size, offset] => Ok((dict::integer(*size)?, dict::integer(*offset)?)),
        _ => Err(Error::Invalid("CFF Private operands")),
    }
}

fn first(operands: &[f64]) -> Result<f64> {
    operands
        .first()
        .copied()
        .ok_or(Error::Invalid("CFF DICT operand count"))
}

/// The FontMatrix mapped to font units, or `None` when it is the identity there.
fn font_units(m: [f64; 6], units_per_em: u16) -> Option<[f64; 6]> {
    let scale = f64::from(units_per_em);
    let units = m.map(|v| v * scale);
    let identity = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let close = units
        .iter()
        .zip(identity)
        .all(|(v, i)| (v - i).abs() < 1e-9);
    (!close && m[..4].iter().any(|&v| v != 0.0)).then_some(units)
}

/// The FD of `glyph` in an FDSelect whose header [`Cff::check_fd_select`] checked.
fn fd_index(table: &[u8], select: FdSelect, glyph: u32) -> Option<usize> {
    let at = select.offset + 1;
    match select.format {
        0 => table
            .get(at + usize::try_from(glyph).ok()?)
            .map(|&fd| usize::from(fd)),
        3 => {
            let count = usize::from(u16_at(table, at)?);
            let (mut low, mut high) = (0, count);
            while low < high {
                let mid = low + (high - low) / 2;
                let range = at + 2 + 3 * mid;
                let first = u32::from(u16_at(table, range)?);
                let next = u32::from(u16_at(table, range + 3)?);
                if glyph < first {
                    high = mid;
                } else if glyph >= next {
                    low = mid + 1;
                } else {
                    return table.get(range + 2).map(|&fd| usize::from(fd));
                }
            }
            None
        }
        4 => {
            let count = usize::try_from(u32_at(table, at)?).ok()?;
            let (mut low, mut high) = (0, count);
            while low < high {
                let mid = low + (high - low) / 2;
                let range = at + 4 + 6 * mid;
                let first = u32_at(table, range)?;
                let next = u32_at(table, range + 6)?;
                if glyph < first {
                    high = mid;
                } else if glyph >= next {
                    low = mid + 1;
                } else {
                    return u16_at(table, range + 4).map(usize::from);
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{FdSelect, fd_index, font_units};

    #[test]
    fn fd_select_ranges() {
        // Format 3: glyphs 0-1 use FD 2, glyphs 2-4 FD 0; sentinel 5.
        let table = [3, 0, 2, 0, 0, 2, 0, 2, 0, 0, 5];
        let select = FdSelect {
            offset: 0,
            format: 3,
        };
        assert_eq!(
            (0..6)
                .map(|g| fd_index(&table, select, g))
                .collect::<Vec<_>>(),
            [Some(2), Some(2), Some(0), Some(0), Some(0), None]
        );
        let per_glyph = [0, 1, 0];
        assert_eq!(
            fd_index(
                &per_glyph,
                FdSelect {
                    offset: 0,
                    format: 0
                },
                0
            ),
            Some(1)
        );
        assert_eq!(
            fd_index(
                &per_glyph,
                FdSelect {
                    offset: 0,
                    format: 0
                },
                2
            ),
            None
        );
    }

    #[test]
    fn font_matrix_in_font_units() {
        assert_eq!(font_units([0.001, 0.0, 0.0, 0.001, 0.0, 0.0], 1000), None);
        assert_eq!(
            font_units([0.0005, 0.0, 0.0, 0.0005, 0.0, 0.0], 1000),
            Some([0.5, 0.0, 0.0, 0.5, 0.0, 0.0])
        );
    }
}
