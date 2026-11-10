//! CFF and CFF2 outlines in OpenType (Adobe Technical Note 5176; OpenType `CFF2`).

mod charstring;

pub use charstring::{CharstringBounds, CharstringOperator, OperatorSet};
mod dict;
mod index;
mod matrix;
mod number;
mod path;

use crate::fixed::{Scale, mul_div, mul_fix};
use crate::glyf::narrow;
use crate::outline::{Outline, Point};
use crate::read::{Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use crate::{GlyphId, Limits};
use dict::{CFF_OPERANDS, CFF2_OPERANDS, Entry, op};
use index::Index;
use matrix::FontMatrix;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    Cff1,
    Cff2,
}

/// Format ceilings: FDSelect indices are 8-bit in CFF, 16-bit in CFF2.
const MAX_CFF_FONT_DICTS: u32 = 256;
/// No glyph has this SID; glyph counts stay below it.
const NO_GLYPH: u16 = u16::MAX;
const MAX_CFF2_FONT_DICTS: u32 = 65536;

/// What a glyph's Font DICT provides; a font without Font DICTs has one, from
/// its Top DICT.
#[derive(Clone, Copy, Debug)]
struct FontDict {
    private: Private,
    /// Normalised, and after the Top DICT's for a Font DICT.
    matrix: FontMatrix,
}

#[derive(Clone, Copy, Debug)]
struct Private {
    subrs: Index,
    default_width: f64,
    nominal_width: f64,
    vsindex: usize,
}

/// A face's encoding as FreeType lists it (`cff_encoding_load`): the glyph
/// of each code, for the Adobe charmap a name-keyed CFF face gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Encoding {
    /// The Adobe encoding id: 0 Standard, 1 Expert, 2 Custom.
    pub(crate) id: u16,
    /// The glyph of each code, zero for none.
    pub(crate) glyphs: [u16; 256],
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
    font_dicts: BudgetVec<FontDict>,
    fd_select: Option<FdSelect>,
    /// The string INDEX, which holds the names of SIDs from 391.
    strings: Index,
    /// The charset: the SID of each glyph, or its CID in a CID-keyed font;
    /// empty for CFF2, which has none.
    sids: BudgetVec<u16>,
    /// The lowest glyph of each SID of a name-keyed face, [`NO_GLYPH`] for
    /// none (`cff_charset_compute_cids`).
    glyphs_by_sid: BudgetVec<u16>,
    /// The encoding of a name-keyed CFF face that maps a code.
    encoding: Option<Encoding>,
    cid: bool,
    glyph_count: u32,
    /// The units per em of the Top DICT's normalised FontMatrix; Font DICTs
    /// with other units per em scale their glyphs by the ratio.
    units_per_em: i64,
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
        work: &mut WorkBudget,
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
            font_dicts: BudgetVec::new(budget),
            fd_select: None,
            strings: Index::default(),
            sids: BudgetVec::new(budget),
            glyphs_by_sid: BudgetVec::new(budget),
            encoding: None,
            cid: false,
            glyph_count: 0,
            units_per_em: i64::from(units_per_em),
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
                cff.strings = Index::parse(&mut r, version)?;
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
        let top = cff.top_dict(top, work)?;
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
            cff.variation_store(bytes, vstore, work)?;
        }
        let top_matrix = top
            .matrix
            .unwrap_or(FontMatrix::identity(i64::from(units_per_em)))
            .normalized();
        cff.units_per_em = top_matrix.units_per_em();
        cff.cid = top.ros;
        if version == Version::Cff1 {
            cff.read_charset(bytes, top.charset, work)?;
            // CID-keyed fonts have no glyph names, so neither `seac` nor an
            // encoding.
            if !top.ros {
                cff.index_sids(work)?;
                cff.encoding = cff.read_encoding(bytes, top.encoding)?;
            }
        }
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
            charge(
                work,
                usize::try_from(fonts.len()).map_err(|_| Error::Overflow)?,
            )?;
            cff.font_dicts
                .try_reserve_exact(usize::try_from(fonts.len()).map_err(|_| Error::Overflow)?)?;
            for i in 0..fonts.len() {
                let font = fonts.get(bytes, i)?;
                let (mut location, mut matrix) = (None, None);
                cff.dict(font, work, |entry| {
                    match entry.operator {
                        op::PRIVATE => location = Some(private_location(entry.operands)?),
                        op::FONT_MATRIX => matrix = Some(FontMatrix::parse(entry.encoded)?),
                        _ => {}
                    }
                    Ok(())
                })?;
                // FreeType concatenates with the Top DICT's matrix only when the
                // Top DICT has one, and normalises copies of it again.
                let matrix = match (matrix, top.matrix) {
                    (Some(own), Some(_)) => own.after(&top_matrix),
                    (Some(own), None) => own,
                    (None, _) => top_matrix,
                };
                let private = cff.private_dict(bytes, location, work)?;
                cff.font_dicts
                    .try_push(FontDict {
                        private,
                        matrix: matrix.normalized(),
                    })
                    .map_err(|e| e.error)?;
            }
            match top.fd_select {
                Some(offset) => cff.fd_select = Some(cff.check_fd_select(bytes, offset)?),
                None if fonts.len() == 1 => {}
                None => return Err(Error::Invalid("CFF without FDSelect")),
            }
        } else {
            let private = cff.private_dict(bytes, top.private, work)?;
            cff.font_dicts
                .try_push(FontDict {
                    private,
                    matrix: top_matrix,
                })
                .map_err(|e| e.error)?;
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
        work: &mut WorkBudget,
        apply: impl FnMut(Entry<'_>) -> Result<()>,
    ) -> Result<()> {
        let scalars = |i| self.region_scalars(i);
        match self.version {
            Version::Cff1 => dict::parse::<CFF_OPERANDS>(bytes, work, &scalars, apply),
            Version::Cff2 => dict::parse::<CFF2_OPERANDS>(bytes, work, &scalars, apply),
        }
    }

    fn top_dict(&self, bytes: &[u8], work: &mut WorkBudget) -> Result<Top> {
        let mut top = Top {
            charstring_type: 2,
            ..Top::default()
        };
        self.dict(
            bytes,
            work,
            |Entry {
                 operator,
                 operands,
                 encoded,
             }| {
                match operator {
                    op::CHAR_STRINGS => top.charstrings = Some(dict::single(operands)?),
                    op::CHARSET => top.charset = dict::single(operands)?,
                    op::ENCODING => top.encoding = dict::single(operands)?,
                    op::PRIVATE => top.private = Some(private_location(operands)?),
                    op::FD_ARRAY => top.fd_array = Some(dict::single(operands)?),
                    op::FD_SELECT => top.fd_select = Some(dict::single(operands)?),
                    op::VSTORE => top.vstore = Some(dict::single(operands)?),
                    op::ROS => top.ros = true,
                    op::SYNTHETIC_BASE => top.synthetic = true,
                    op::CHARSTRING_TYPE => top.charstring_type = dict::single(operands)?,
                    op::FONT_MATRIX => top.matrix = Some(FontMatrix::parse(encoded)?),
                    _ => {}
                }
                Ok(())
            },
        )?;
        Ok(top)
    }

    /// The Private DICT at `location` (size, offset); CFF2 Font DICTs may omit it.
    fn private_dict(
        &self,
        table: &[u8],
        location: Option<(usize, usize)>,
        work: &mut WorkBudget,
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
        self.dict(
            bytes,
            work,
            |Entry {
                 operator, operands, ..
             }| {
                match operator {
                    op::SUBRS => subrs = Some(dict::single(operands)?),
                    op::DEFAULT_WIDTH_X => private.default_width = first(operands)?,
                    op::NOMINAL_WIDTH_X => private.nominal_width = first(operands)?,
                    op::VSINDEX => private.vsindex = dict::single(operands)?,
                    _ => {}
                }
                Ok(())
            },
        )?;
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

    /// The Font DICT of `glyph`.
    fn font_dict(&self, table: &[u8], glyph: u32) -> Result<&FontDict> {
        let fd = match self.fd_select {
            None => 0,
            Some(select) => fd_index(table, select, glyph)
                .ok_or(Error::Invalid("glyph outside CFF FDSelect"))?,
        };
        self.font_dicts
            .as_slice()
            .get(fd)
            .ok_or(Error::Invalid("CFF FDSelect index outside FDArray"))
    }

    /// The Private DICT values of `glyph`.
    fn private(&self, table: &[u8], glyph: u32) -> Result<&Private> {
        Ok(&self.font_dict(table, glyph)?.private)
    }

    fn variation_store(
        &mut self,
        table: &[u8],
        offset: usize,
        work: &mut WorkBudget,
    ) -> Result<()> {
        // A 16-bit length precedes the ItemVariationStore.
        let start = offset.checked_add(2).ok_or(Error::Overflow)?;
        let mut r = Reader::at(table, start)?;
        if r.u16()? != 1 {
            return Err(Error::Invalid("ItemVariationStore format"));
        }
        let regions = usize::try_from(r.u32()?).map_err(|_| Error::Overflow)?;
        let count = r.u16()?;
        charge(work, usize::from(count))?;
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
            // Every region index costs work, also when there are no axes.
            charge(work, usize::from(indices) * axes.max(1))?;
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

    /// The first glyph whose charset SID is `sid`, for `seac`
    /// (`cff_lookup_glyph_by_stdcharcode`).
    pub(super) fn glyph_for_sid(&self, sid: u16) -> Option<u32> {
        let glyph = *self.glyphs_by_sid.get(usize::from(sid))?;
        (glyph != NO_GLYPH).then_some(u32::from(glyph))
    }

    /// Fills [`Self::glyphs_by_sid`] from the charset.
    fn index_sids(&mut self, work: &mut WorkBudget) -> Result<()> {
        let most = self.sids.iter().copied().max().map_or(0, usize::from);
        charge(work, self.sids.len() + most)?;
        self.glyphs_by_sid.try_resize(most + 1, NO_GLYPH)?;
        for (glyph, &sid) in self.sids.iter().enumerate().rev() {
            // Glyph counts stay below NO_GLYPH.
            self.glyphs_by_sid[usize::from(sid)] = glyph as u16;
        }
        Ok(())
    }

    /// The glyph names in index order, as FreeType reads them for its
    /// Unicode charmap (`cff_sid_to_glyph_name`): each glyph's charset SID as
    /// a standard string below 391, else as an object of the string INDEX,
    /// up to a NUL; `None` beyond the strings. A CID-keyed font's CIDs are
    /// read as SIDs too. Empty for CFF2.
    pub(crate) fn glyph_names<'a>(
        &'a self,
        data: &'a [u8],
    ) -> impl Iterator<Item = Option<&'a [u8]>> + 'a {
        let table = self.table.bytes(data);
        self.sids.iter().map(move |&sid| match sid {
            0xffff => None,
            0..=390 => crate::postscript::standard_string(sid).map(str::as_bytes),
            _ => {
                let name = self.strings.get(table, u32::from(sid - 391)).ok()?;
                Some(&name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())])
            }
        })
    }

    /// Whether the face has glyph names to build a Unicode charmap from: a
    /// charset, which CFF2 has not.
    pub(crate) fn has_charset(&self) -> bool {
        !self.sids.is_empty()
    }

    /// The face's encoding, where it maps a code.
    pub(crate) const fn encoding(&self) -> Option<&Encoding> {
        self.encoding.as_ref()
    }

    /// `cff_charset_load`: the SID of every glyph, from a charset table
    /// (formats 0, 1 and 2) or a predefined one, which must cover the glyphs.
    fn read_charset(&mut self, table: &[u8], offset: usize, work: &mut WorkBudget) -> Result<()> {
        let count = usize::try_from(self.glyph_count).map_err(|_| Error::Overflow)?;
        charge(work, count)?;
        self.sids.try_reserve_exact(count)?;
        let predefined: Option<&[u16]> = match offset {
            0 => Some(&crate::postscript::ISOADOBE_CHARSET),
            1 => Some(&crate::postscript::EXPERT_CHARSET),
            2 => Some(&crate::postscript::EXPERTSUBSET_CHARSET),
            _ => None,
        };
        if let Some(charset) = predefined {
            let sids = charset.get(..count).ok_or(Error::Invalid(
                "CFF predefined charset shorter than the font",
            ))?;
            return self.sids.try_extend_from_slice(sids);
        }
        let mut r = Reader::at(table, offset)?;
        let format = r.u8()?;
        self.sids.try_push(0).map_err(|e| e.error)?;
        match format {
            0 => {
                for _ in 1..count {
                    self.sids.try_push(r.u16()?).map_err(|e| e.error)?;
                }
            }
            1 | 2 => {
                while self.sids.len() < count {
                    let first = r.u16()?;
                    let left = if format == 2 {
                        r.u16()?
                    } else {
                        u16::from(r.u8()?)
                    };
                    // FreeType keeps a range from running past SID 65535.
                    let left = left.min(u16::MAX - first);
                    for sid in first..=first + left {
                        if self.sids.len() == count {
                            break;
                        }
                        self.sids.try_push(sid).map_err(|e| e.error)?;
                    }
                }
            }
            _ => return Err(Error::Invalid("CFF charset format")),
        }
        Ok(())
    }

    /// `cff_encoding_load`: the glyph of each code, from an encoding table
    /// (formats 0 and 1, with supplements) or the Standard or Expert encoding
    /// through the charset; `None` when no code has a glyph, where FreeType
    /// lists no Adobe charmap.
    fn read_encoding(&self, table: &[u8], offset: usize) -> Result<Option<Encoding>> {
        let sids = self.sids.as_slice();
        let count = sids.len();
        let mut glyphs = [0_u16; 256];
        let narrow = |glyph: usize| u16::try_from(glyph).map_err(|_| Error::Overflow);
        let (id, coded) = match offset {
            0 | 1 => {
                let encoding = if offset == 0 {
                    &crate::postscript::STANDARD_ENCODING
                } else {
                    &crate::postscript::EXPERT_ENCODING
                };
                // The lowest glyph of each SID (`cff_charset_compute_cids`).
                let mut coded = 0;
                for (code, &sid) in encoding.iter().enumerate() {
                    if sid == 0 {
                        continue;
                    }
                    if let Some(glyph) = self.glyph_for_sid(sid).filter(|&g| g != 0) {
                        glyphs[code] = narrow(glyph as usize)?;
                        coded = code + 1;
                    }
                }
                (offset as u16, coded)
            }
            _ => {
                let mut r = Reader::at(table, offset)?;
                let format = r.u8()?;
                let ranges = usize::from(r.u8()?);
                let coded = match format & 0x7f {
                    0 => {
                        for glyph in 1..=ranges {
                            let code = usize::from(r.u8()?);
                            if glyph < count {
                                glyphs[code] = narrow(glyph)?;
                            }
                        }
                        ranges + 1
                    }
                    1 => {
                        let (mut glyph, mut widest) = (1, 0);
                        for _ in 0..ranges {
                            let first = usize::from(r.u8()?);
                            let left = usize::from(r.u8()?) + 1;
                            widest = widest.max(left);
                            for (code, glyph) in (first..).zip(glyph..glyph + left) {
                                if glyph < count && code < 256 {
                                    glyphs[code] = narrow(glyph)?;
                                }
                            }
                            glyph += left;
                        }
                        widest.min(256)
                    }
                    _ => return Err(Error::Invalid("CFF encoding format")),
                };
                if format & 0x80 != 0 {
                    for _ in 0..r.u8()? {
                        let code = usize::from(r.u8()?);
                        let sid = r.u16()?;
                        if let Some(glyph) = self.glyph_for_sid(sid) {
                            glyphs[code] = narrow(glyph as usize)?;
                        }
                    }
                }
                (2, coded)
            }
        };
        Ok((coded > 0).then_some(Encoding { id, glyphs }))
    }

    /// The operators `glyph`'s program executes; `outline` is scratch storage.
    pub(crate) fn operators(
        &self,
        data: &[u8],
        glyph: GlyphId,
        outline: &mut Outline,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<OperatorSet> {
        charstring::operators(
            self,
            self.table.bytes(data),
            u32::from(glyph.0),
            outline,
            limits,
            work,
        )
    }

    /// The bounds of what `glyph`'s program draws, without building it.
    pub(crate) fn bounds(
        &self,
        data: &[u8],
        glyph: GlyphId,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<Option<CharstringBounds>> {
        charstring::bounds(
            self,
            self.table.bytes(data),
            u32::from(glyph.0),
            limits,
            work,
        )
    }

    /// Loads `glyph` into `outline` as FreeType's CFF loader does without
    /// hinting: built in whole font units, through its Font DICT's FontMatrix,
    /// then multiplied by the 16.16 `scale` ([`Scale::UNITS`] for font units).
    pub(crate) fn load(
        &self,
        data: &[u8],
        glyph: GlyphId,
        scale: Scale,
        outline: &mut Outline,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let table = self.table.bytes(data);
        charstring::outline(self, table, u32::from(glyph.0), outline, limits, work)?;
        let (matrix, scale) = self.placement(table, glyph, scale)?;
        if !matrix.is_identity() || scale != Scale::UNITS {
            for p in outline.points_mut() {
                let (x, y) = matrix.point((i64::from(p.point.x), i64::from(p.point.y)));
                p.point = Point::new(narrow(mul_fix(x, scale.x))?, narrow(mul_fix(y, scale.y))?);
            }
        }
        Ok(())
    }

    /// The `hmtx` `advance` of `glyph` as FreeType's loader reports it with
    /// [`Self::load`]: through the FontMatrix, then scaled.
    pub(crate) fn advance(
        &self,
        data: &[u8],
        glyph: GlyphId,
        advance: u16,
        scale: Scale,
    ) -> Result<i64> {
        let (matrix, scale) = self.placement(self.table.bytes(data), glyph, scale)?;
        Ok(mul_fix(matrix.advance(i64::from(advance)), scale.x))
    }

    /// A vertical advance of `glyph` in font units as FreeType's loader
    /// reports it with [`Self::load`]: through the FontMatrix, then scaled.
    pub(crate) fn vertical_advance(
        &self,
        data: &[u8],
        glyph: GlyphId,
        advance: i64,
        scale: Scale,
    ) -> Result<i64> {
        let (matrix, scale) = self.placement(self.table.bytes(data), glyph, scale)?;
        Ok(mul_fix(matrix.vertical_advance(advance), scale.y))
    }

    /// The FontMatrix of `glyph` and `scale` adjusted for its Font DICT's
    /// units per em.
    fn placement(&self, table: &[u8], glyph: GlyphId, scale: Scale) -> Result<(FontMatrix, Scale)> {
        let dict = self.font_dict(table, u32::from(glyph.0))?;
        let own = dict.matrix.units_per_em();
        let scale = if own == self.units_per_em {
            scale
        } else {
            Scale {
                x: mul_div(scale.x, self.units_per_em, own),
                y: mul_div(scale.y, self.units_per_em, own),
            }
        };
        Ok((dict.matrix, scale))
    }
}

/// The Top DICT entries this reader uses.
#[derive(Debug, Default)]
struct Top {
    charstrings: Option<usize>,
    charset: usize,
    encoding: usize,
    private: Option<(usize, usize)>,
    fd_array: Option<usize>,
    fd_select: Option<usize>,
    vstore: Option<usize>,
    ros: bool,
    synthetic: bool,
    charstring_type: usize,
    matrix: Option<FontMatrix>,
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
    use super::{FdSelect, fd_index};

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
}
