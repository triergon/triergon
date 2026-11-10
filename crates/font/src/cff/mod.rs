//! CFF and CFF2 outlines in OpenType (Adobe Technical Note 5176; OpenType `CFF2`).

mod charstring;

pub use charstring::{CharstringBounds, CharstringOperator, OperatorSet};
mod dict;
mod hint;
mod index;
mod matrix;
mod number;
mod path;

use crate::fixed::{Scale, div_fix, mul_div, mul_fix};
use crate::glyf::narrow;
use crate::outline::{Outline, Point};
use crate::read::{Reader, slice, u16_at, u32_at};
use crate::sfnt::Table;
use crate::work::charge;
use crate::{GlyphId, Limits};
use dict::{CFF_OPERANDS, CFF2_OPERANDS, Entry, op};
use hint::{Blues, HintDict};
use index::Index;
use matrix::FontMatrix;
use number::Operand;
use path::{Fixed, Scaling};
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
    vsindex: usize,
    /// The values the hinter reads.
    hints: HintDict,
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
    /// The face's units per em, from `head`, which bound the hinted scale.
    face_units_per_em: u16,
    /// Region scalars of each ItemVariationData at the default instance, in
    /// 16.16: one or zero.
    scalars: BudgetVec<Fixed>,
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
            face_units_per_em: units_per_em,
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
                        op::FONT_MATRIX => matrix = Some(FontMatrix::parse(entry.operands)?),
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

    fn dict<'a>(
        &self,
        bytes: &'a [u8],
        work: &mut WorkBudget,
        apply: impl FnMut(Entry<'a, '_>) -> Result<()>,
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
        self.dict(bytes, work, |Entry { operator, operands }| {
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
                op::FONT_MATRIX => top.matrix = Some(FontMatrix::parse(operands)?),
                _ => {}
            }
            Ok(())
        })?;
        Ok(top)
    }

    /// The Private DICT at `location` (size, offset); CFF2 Font DICTs may omit
    /// it. The hinter's values are read as `cff_load_private_dict` reads them:
    /// the DICT's entries over FreeType's defaults, or zeros for a Font DICT
    /// without a Private DICT (none, or an offset or size of zero).
    fn private_dict(
        &self,
        table: &[u8],
        location: Option<(usize, usize)>,
        work: &mut WorkBudget,
    ) -> Result<Private> {
        let mut private = Private {
            subrs: Index::default(),
            vsindex: 0,
            hints: HintDict::ABSENT,
        };
        let Some((size, offset)) = location else {
            return match self.version {
                Version::Cff2 => Ok(private),
                Version::Cff1 => Err(Error::Invalid("CFF without a Private DICT")),
            };
        };
        let bytes = slice(table, offset, size)?;
        let mut subrs = None;
        let mut hints = HintDict::DEFAULTS;
        self.dict(bytes, work, |Entry { operator, operands }| {
            let fixed = |o: &Operand<'_>| o.fixed(0);
            match operator {
                op::SUBRS => subrs = Some(dict::single(operands)?),
                op::VSINDEX => private.vsindex = dict::single(operands)?,
                // Unused here, but an entry without its operand fails the face,
                // as it fails FreeType's parser.
                op::DEFAULT_WIDTH_X | op::NOMINAL_WIDTH_X => {
                    dict::first(operands)?;
                }
                op::BLUE_VALUES => hints.blue_values.set(operands.iter().map(fixed)),
                op::OTHER_BLUES => hints.other_blues.set(operands.iter().map(fixed)),
                op::FAMILY_BLUES => hints.family_blues.set(operands.iter().map(fixed)),
                op::FAMILY_OTHER_BLUES => {
                    hints.family_other_blues.set(operands.iter().map(fixed));
                }
                op::BLUE_SCALE => hints.blue_scale = dict::first(operands)?.fixed(3),
                op::BLUE_SHIFT => hints.blue_shift = dict::first(operands)?.integer(),
                op::BLUE_FUZZ => hints.blue_fuzz = dict::first(operands)?.integer(),
                // FreeType stores LanguageGroup in an int.
                op::LANGUAGE_GROUP => {
                    hints.language_group = dict::first(operands)?.integer() as i32;
                }
                _ => {}
            }
            Ok(())
        })?;
        if size > 0 && offset > 0 {
            hints.sanitize();
            private.hints = hints;
        }
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
                let mut scalar = 0x10000;
                for _ in 0..axes {
                    let (start, peak, end) = (axis.i16()?, axis.i16()?, axis.i16()?);
                    // At the default instance every coordinate is zero: an axis whose
                    // valid, non-straddling range peaks away from zero excludes the
                    // region (OpenType "Item variation store", region scalars).
                    let ignored = start > peak || peak > end || peak == 0 || (start < 0 && end > 0);
                    if !ignored {
                        scalar = 0;
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

    fn region_scalars(&self, vsindex: usize) -> Result<&[Fixed]> {
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
        let index = u32::from(glyph.0);
        charstring::outline(
            self,
            table,
            index,
            &Scaling::UNHINTED,
            outline,
            limits,
            work,
        )?;
        let (matrix, scale) = self.placement(table, glyph, scale)?;
        if !matrix.is_identity() || scale != Scale::UNITS {
            for p in outline.points_mut() {
                let (x, y) = matrix.point((i64::from(p.point.x), i64::from(p.point.y)));
                p.point = Point::new(narrow(mul_fix(x, scale.x))?, narrow(mul_fix(y, scale.y))?);
            }
        }
        Ok(())
    }

    /// Loads `glyph` into `outline` as FreeType's default load leaves it in
    /// the glyph slot (`FT_LOAD_DEFAULT`: Adobe's hinter, without stem
    /// darkening), before any grid fitting of its metrics: the charstring is
    /// hinted at cf2's scale, a 64th of the 16.16 `scale` rounded, into 26.6
    /// pixels, which then go through its Font DICT's FontMatrix and are moved
    /// by the matrix offset in font units, unscaled. A Font DICT with other
    /// units per em adjusts the scale first, as [`Self::load`] does.
    ///
    /// The hinter refuses scales beyond 2000 pixels per em, which then load as
    /// [`Self::load`] loads them, as FreeType retries them unhinted; a scale
    /// below 32 fails the glyph (`Error::Invalid`), as it fails FreeType's
    /// load.
    pub(crate) fn load_hinted(
        &self,
        data: &[u8],
        glyph: GlyphId,
        scale: Scale,
        outline: &mut Outline,
        limits: &Limits,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let table = self.table.bytes(data);
        let (matrix, adjusted) = self.placement(table, glyph, scale)?;
        // `cf2_getScaleAndHintFlag`, in 32 bits.
        let hinted = |s: i64| (s as u32).wrapping_add(32).cast_signed() / 64;
        let (x, y) = (hinted(adjusted.x), hinted(adjusted.y));
        // `cf2_checkTransform`.
        if x <= 0 || y <= 0 {
            return Err(Error::Invalid("CFF hinting scale below 1/64"));
        }
        let units = self.face_units_per_em;
        let most = div_fix(2000 << 16, i64::from(units) << 16) as Fixed;
        if units > 0x7fff || x > most || y > most {
            return self.load(data, glyph, scale, outline, limits, work);
        }
        let index = u32::from(glyph.0);
        let scaling = Scaling {
            x,
            y,
            blues: Some(Blues::new(&self.private(table, index)?.hints, y)),
        };
        charstring::outline(self, table, index, &scaling, outline, limits, work)?;
        if !matrix.is_identity() {
            for p in outline.points_mut() {
                let (x, y) = matrix.point((i64::from(p.point.x), i64::from(p.point.y)));
                p.point = Point::new(narrow(x)?, narrow(y)?);
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

fn private_location(operands: &[Operand<'_>]) -> Result<(usize, usize)> {
    match operands {
        [size, offset] => Ok((size.index()?, offset.index()?)),
        _ => Err(Error::Invalid("CFF Private operands")),
    }
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
#[path = "../../tests/support/cff.rs"]
mod authoring;

#[cfg(test)]
mod tests {
    use super::authoring::{CffProgram, Part, charstring, number};
    use super::{Cff, FdSelect, Version, fd_index};
    use crate::fixed::Scale;
    use crate::outline::Outline;
    use crate::sfnt::Table;
    use crate::{GlyphId, Limits};
    use triergon_core::{Budget, Error, Result, WorkBudget};

    /// A name-keyed CFF table whose glyph 1 is `glyph`, with `private` added
    /// to its Private DICT, at 1000 units per em.
    fn table(glyph: &[Part], private: Vec<u8>) -> Vec<u8> {
        CffProgram {
            glyphs: vec![charstring(&[Op(14)]), charstring(glyph)],
            global: Vec::new(),
            local: Vec::new(),
            charset: None,
            strings: Vec::new(),
            encoding: None,
            matrix: None,
            nominal_width: 0,
            private,
        }
        .build()
    }

    /// The points of glyph 1 at the 16.16 `scale`, hinted or not.
    fn points(table: &[u8], scale: i64, hinted: bool) -> Vec<(i32, i32)> {
        try_points(table, scale, hinted).unwrap()
    }

    /// The points of glyph 1, or why it does not load.
    fn try_points(table: &[u8], scale: i64, hinted: bool) -> Result<Vec<(i32, i32)>> {
        let budget = Budget::new(1 << 20).unwrap();
        let mut work = WorkBudget::new(1 << 20);
        let whole = Table {
            offset: 0,
            len: table.len(),
        };
        let cff = Cff::parse(table, whole, Version::Cff1, 1000, &budget, &mut work).unwrap();
        let mut outline = Outline::new(&budget);
        let (glyph, scale) = (GlyphId(1), Scale { x: scale, y: scale });
        let limits = Limits::default();
        if hinted {
            cff.load_hinted(table, glyph, scale, &mut outline, &limits, &mut work)
        } else {
            cff.load(table, glyph, scale, &mut outline, &limits, &mut work)
        }?;
        let points = outline.points().len() as u32;
        assert!(outline.contour_ends() == [points] || points == 0);
        Ok(outline
            .points()
            .iter()
            .map(|p| (p.point.x, p.point.y))
            .collect())
    }

    use Part::{Byte, N, Op};

    /// A rectangle from (0, `bottom`), 100 wide, through `heights` above it.
    fn rectangle(bottom: i16, heights: &[i16]) -> Vec<Part> {
        let mut parts = vec![N(0), N(bottom), Op(21), N(100), Op(6)];
        for &height in heights {
            parts.extend([N(height), Op(7)]);
        }
        parts.extend([N(-100), Op(6), Op(14)]);
        parts
    }

    /// Two in 16.16, whose 64th FreeType's hinter scales by: 2048, exactly a
    /// 32nd of a pixel per unit, so unhinted points double.
    const TWO: i64 = 2 << 16;

    #[test]
    fn a_glyph_without_stems_scales_uniformly_at_the_hinted_scale() {
        // rmoveto 10 20, hlineto 100, rlineto 50 50.
        let glyph = table(
            &[N(10), N(20), Op(21), N(100), Op(6), N(50), N(50), Op(5)],
            Vec::new(),
        );
        assert_eq!(points(&glyph, TWO, true), [(20, 40), (220, 40), (320, 140)]);
        assert_eq!(points(&glyph, TWO, false), points(&glyph, TWO, true));
        // At 100000 the hinter scales by (100000 + 32) / 64 = 1563: 110 units
        // are 110 × 1563 = 171930 in 16.16, 167 in 26.6 after `>> 10`, where
        // the unhinted FT_MulFix(110, 100000) rounds to 168.
        assert_eq!(
            points(&glyph, 100_000, true),
            [(15, 30), (167, 30), (244, 106)]
        );
        assert_eq!(
            points(&glyph, 100_000, false),
            [(15, 31), (168, 31), (244, 107)]
        );
    }

    #[test]
    fn a_stem_pair_moves_its_nearer_edge_onto_a_pixel() {
        // hstem 100 50: edges at 100 × 2048 = 3.125 and 4.6875 pixels. The
        // initial map holds only the synthetic edge at zero, which places the
        // stem where the scale does; `cf2_hintmap_adjustHints` moves the pair
        // down by 0.125 (less than up by 0.3125) to 3 and 4.5625 pixels, 192
        // and 292 in 26.6, and points below follow at the nominal scale.
        let mut glyph = vec![N(100), N(50), Op(1)];
        glyph.extend(rectangle(100, &[50]));
        let glyph = table(&glyph, Vec::new());
        assert_eq!(
            points(&glyph, TWO, true),
            [(0, 192), (200, 192), (200, 292), (0, 292)]
        );
        assert_eq!(
            points(&glyph, TWO, false),
            [(0, 200), (200, 200), (200, 300), (0, 300)]
        );
    }

    #[test]
    fn a_hint_mask_switches_maps_after_the_element_before_it() {
        // hstemhm 100 50 150 50: stems at 100-150 and 300-350. The first mask
        // selects the lower, placed at 3 and 4.5625 pixels as above; the
        // second the upper, at 9.375 and 10.9375 pixels, moved up by 0.0625
        // to 9.4375 and 11, 604 and 704 in 26.6. The point at 150 ends the
        // line before the second mask, so the first map still places it, and
        // the closing line ends where the first map put the start.
        let mut glyph = vec![N(100), N(50), N(150), N(50), Op(18), Op(19), Byte(0x80)];
        let mut path = rectangle(100, &[50, 150, 50]);
        path.splice(7..7, [Op(19), Byte(0x40)]);
        glyph.extend(path);
        let glyph = table(&glyph, Vec::new());
        assert_eq!(
            points(&glyph, TWO, true),
            [
                (0, 192),
                (200, 192),
                (200, 292),
                (200, 604),
                (200, 704),
                (0, 704)
            ]
        );
    }

    #[test]
    fn a_blue_zone_captures_a_ghost_edge() {
        // BlueValues -10 0 500 510 and the default BlueScale: 2597 in 16.16
        // is above the scale, 2048, so overshoot is suppressed and zones are
        // boosted by 0.6 - 0.6 × 2048 / 2597, 8313. The top zone's flat edge,
        // 500 × 2048 + 8313, rounds to 16 pixels. hstem 505 -20, a ghost top
        // edge at 505, is captured there (1024 in 26.6, unhinted 1010); the
        // glyph's other points lie below the map's only edge and keep the
        // nominal scale from it: 0 maps to 16 - 505 / 32 pixels, 14 in 26.6.
        let blues = [number(-10), number(10), number(500), number(10), vec![6]].concat();
        let mut glyph = vec![N(505), N(-20), Op(1)];
        glyph.extend(rectangle(0, &[505]));
        let glyph = table(&glyph, blues);
        assert_eq!(
            points(&glyph, TWO, true),
            [(0, 14), (200, 14), (200, 1024), (0, 1024)]
        );
        assert_eq!(
            points(&glyph, TWO, false),
            [(0, 0), (200, 0), (200, 1010), (0, 1010)]
        );
    }

    #[test]
    fn sizes_beyond_2000_pixels_per_em_load_unhinted() {
        // (8388672 + 32) / 64 = 131073 is above 2000 / 1000 in 16.16.
        let mut glyph = vec![N(100), N(50), Op(1)];
        glyph.extend(rectangle(100, &[50]));
        let glyph = table(&glyph, Vec::new());
        let big = 8_388_672;
        assert_eq!(points(&glyph, big, true), points(&glyph, big, false));
        assert_eq!(points(&glyph, big, false)[1], (12800, 12800));
    }

    /// `count` horizontal stems 10 units high, 5 apart, declared by `hstem`
    /// operators of at most 20 stems each.
    fn many_stems(count: i16) -> Vec<Part> {
        let mut parts = Vec::new();
        for first in (0..count).step_by(20) {
            for _ in first..(first + 20).min(count) {
                parts.extend([N(5), N(10)]);
            }
            parts.push(Op(1));
        }
        parts
    }

    #[test]
    fn stems_declared_after_the_first_move_are_ignored() {
        // Eight stems take one mask byte. A ninth after the move would make
        // the hint mask two bytes long and swallow the hlineto's operand.
        let mut glyph = many_stems(8);
        glyph.extend([
            N(0),
            N(100),
            Op(21),
            N(300),
            N(50),
            Op(1),
            Op(19),
            Byte(0xff),
        ]);
        glyph.extend([N(100), Op(6), N(50), Op(7), N(-100), Op(6), Op(14)]);
        let glyph = table(&glyph, Vec::new());
        let square = [(0, 200), (200, 200), (200, 300), (0, 300)];
        assert_eq!(points(&glyph, TWO, false), square);
        assert_eq!(points(&glyph, TWO, true).len(), 4);
    }

    #[test]
    fn a_later_mask_with_operands_reads_no_mask_bytes() {
        // After the first mask fixes the stems, `1 2 hintmask` is skipped and
        // the bytes after it run as charstring: vlineto 50.
        let mut glyph = vec![N(100), N(50), Op(18), Op(19), Byte(0x80)];
        glyph.extend([N(0), N(100), Op(21), N(100), Op(6), N(1), N(2), Op(19)]);
        glyph.extend([N(50), Op(7), N(-100), Op(6), Op(14)]);
        let glyph = table(&glyph, Vec::new());
        let square = [(0, 200), (200, 200), (200, 300), (0, 300)];
        assert_eq!(points(&glyph, TWO, false), square);
        assert_eq!(points(&glyph, TWO, true).len(), 4);
    }

    #[test]
    fn more_than_96_stems_fail_only_once_a_mask_or_move_uses_them() {
        let unused = [many_stems(100), vec![Op(14)]].concat();
        assert_eq!(
            try_points(&table(&unused, Vec::new()), TWO, false),
            Ok(Vec::new())
        );
        let moved = [many_stems(100), rectangle(0, &[50])].concat();
        for hinted in [false, true] {
            assert_eq!(
                try_points(&table(&moved, Vec::new()), TWO, hinted),
                Err(Error::Invalid("more than 96 stem hints"))
            );
        }
        // 96 stems still load.
        let most = [many_stems(96), rectangle(0, &[50])].concat();
        assert_eq!(points(&table(&most, Vec::new()), TWO, false).len(), 4);
    }

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
