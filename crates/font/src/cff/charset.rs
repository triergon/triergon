//! CFF1 charsets, glyph names and Adobe encodings (Adobe Technical Note 5176).

use super::{Cff, Encoding, NO_GLYPH};
use crate::read::Reader;
use crate::work::charge;
use triergon_core::{Error, Result, WorkBudget};

impl Cff {
    pub(super) fn load_charmaps(
        &mut self,
        table: &[u8],
        charset: usize,
        encoding: usize,
        unicode: bool,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if self.cid {
            return self.read_charset(table, charset, !unicode, work);
        }
        let standard_sids_suffice = encoding <= 1 || matches!(table.get(encoding), Some(0 | 1));
        if unicode && standard_sids_suffice {
            // Predefined encodings and seac query only standard strings.
            // A custom encoding without supplements uses glyph ordinals.
            self.index_standard_sids(table, charset, encoding == 1, work)?;
        } else {
            self.read_charset(table, charset, true, work)?;
            self.index_sids(work)?;
        }
        self.encoding = self.read_encoding(table, encoding)?;
        Ok(())
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

    /// Whether the face retained a CFF charset for glyph-name lookup.
    pub(crate) fn has_charset(&self) -> bool {
        !self.sids.is_empty()
    }

    /// The face's encoding, where it maps a code.
    pub(crate) const fn encoding(&self) -> Option<&Encoding> {
        self.encoding.as_ref()
    }

    /// `cff_charset_load`: the SID of every glyph, from a charset table
    /// (formats 0, 1 and 2) or a predefined one, which must cover the glyphs.
    pub(super) fn read_charset(
        &mut self,
        table: &[u8],
        offset: usize,
        retain: bool,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let count = usize::try_from(self.glyph_count).map_err(|_| Error::Overflow)?;
        charge(work, count)?;
        if !retain {
            return charset_ranges(table, offset, count, |_, _| Ok(()));
        }
        self.sids.try_reserve_exact(count)?;
        charset_ranges(table, offset, count, |first, last| {
            for sid in first..=last {
                self.sids.try_push(sid).map_err(|e| e.error)?;
            }
            Ok(())
        })
    }

    /// Index the lowest glyph for each SID the encoding or a seac can query.
    fn index_standard_sids(
        &mut self,
        table: &[u8],
        offset: usize,
        expert: bool,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let count = usize::try_from(self.glyph_count).map_err(|_| Error::Overflow)?;
        charge(work, count)?;
        use crate::postscript::{EXPERT_ENCODING, STANDARD_ENCODING};
        const STANDARD: usize = encoding_sid_count(&STANDARD_ENCODING);
        const EXPERT: usize = encoding_sid_count(&EXPERT_ENCODING);
        const { assert!(EXPERT >= STANDARD) };
        let mut storage = [NO_GLYPH; EXPERT];
        let glyphs = if expert {
            &mut storage[..]
        } else {
            &mut storage[..STANDARD]
        };
        let mut glyph = 0_usize;
        let mut most = 0;
        charset_ranges(table, offset, count, |first, last| {
            most = most.max(usize::from(last));
            let end = usize::from(last).min(glyphs.len() - 1);
            if let Some(slots) = glyphs.get_mut(usize::from(first)..=end) {
                for (offset, slot) in slots.iter_mut().enumerate() {
                    // Glyph IDs increase in file order. Keep the first.
                    *slot = (*slot).min((glyph + offset) as u16);
                }
            }
            glyph += usize::from(last - first) + 1;
            Ok(())
        })?;
        // Keep the same work refusal boundary as the full SID index.
        charge(work, count + most)?;
        let used = (most + 1).min(glyphs.len());
        self.glyphs_by_sid.try_extend_from_slice(&glyphs[..used])
    }

    /// `cff_encoding_load`: the glyph of each code, from an encoding table
    /// (formats 0 and 1, with supplements) or the Standard or Expert encoding
    /// through the charset; `None` when no code has a glyph, where FreeType
    /// lists no Adobe charmap.
    fn read_encoding(&self, table: &[u8], offset: usize) -> Result<Option<Encoding>> {
        let count = self.glyph_count as usize;
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
                // Codes below space are unassigned in both predefined encodings.
                for (code, &sid) in encoding.iter().enumerate().skip(32) {
                    if sid == 0 {
                        continue;
                    }
                    if let Some(glyph) = self.glyph_for_sid(sid).filter(|&g| g != 0) {
                        glyphs[code] = narrow(glyph as usize)?;
                        coded = code + 1;
                    } else if offset == 0 && usize::from(sid) >= self.glyphs_by_sid.len() {
                        // Assigned StandardEncoding SIDs are in increasing order.
                        break;
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
}

/// Length of the SID-to-glyph index needed by a predefined encoding.
const fn encoding_sid_count(mut encoding: &[u16]) -> usize {
    let mut highest = 0;
    while let Some((&sid, rest)) = encoding.split_first() {
        if sid > highest {
            highest = sid;
        }
        encoding = rest;
    }
    highest as usize + 1
}

/// Visit consecutive string IDs in glyph order. Format 0 and the predefined
/// charsets list individual IDs; formats 1 and 2 can describe entire ranges.
fn charset_ranges(
    table: &[u8],
    offset: usize,
    count: usize,
    mut visit: impl FnMut(u16, u16) -> Result<()>,
) -> Result<()> {
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
        for &sid in sids {
            visit(sid, sid)?;
        }
        return Ok(());
    }

    let mut r = Reader::at(table, offset)?;
    let format = r.u8()?;
    visit(0, 0)?;
    let mut remaining = count - 1;
    match format {
        0 => {
            for &bytes in r.take(2 * remaining)?.as_chunks::<2>().0 {
                let sid = u16::from_be_bytes(bytes);
                visit(sid, sid)?;
            }
        }
        1 | 2 => {
            while remaining != 0 {
                let first = r.u16()?;
                let left = if format == 2 {
                    r.u16()?
                } else {
                    u16::from(r.u8()?)
                };
                // The last declared range may extend beyond the glyph count
                // or the greatest SID. Consume exactly the available entries.
                let last = first.saturating_add(left.min((remaining - 1) as u16));
                visit(first, last)?;
                remaining -= usize::from(last - first) + 1;
            }
        }
        _ => return Err(Error::Invalid("CFF charset format")),
    }
    Ok(())
}
