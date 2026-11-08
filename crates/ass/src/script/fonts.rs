//! Fonts embedded in `[Fonts]` with SSA's uuencode variant.

use triergon_core::{BudgetVec, Bytes, Result};

use super::Script;

/// A decoded embedded font.
#[derive(Clone, Debug)]
pub struct EmbeddedFont {
    /// The name after `fontname:`.
    pub name: Bytes,
    /// The decoded font file.
    pub data: Bytes,
}

/// A `fontname:` line and the encoded lines collected after it.
#[derive(Debug)]
pub(super) struct PendingFont {
    name: Bytes,
    encoded: BudgetVec<u8>,
}

/// Decodes SSA's font encoding: each group of four characters carries 24 bits,
/// six per character as `c - 33`; a final group of two or three characters
/// carries one or two bytes, and a single character is malformed.
fn decode(encoded: &[u8], out: &mut BudgetVec<u8>) -> Result<bool> {
    if encoded.len() % 4 == 1 {
        return Ok(false);
    }
    out.try_reserve_exact(encoded.len() / 4 * 3 + (encoded.len() % 4).saturating_sub(1))?;
    for group in encoded.chunks(4) {
        let value = group.iter().enumerate().fold(0_u32, |v, (i, &c)| {
            v | (u32::from(c.wrapping_sub(33)) & 63) << (6 * (3 - i))
        });
        let bytes = value.to_be_bytes();
        out.try_extend_from_slice(&bytes[1..group.len()])?;
    }
    Ok(true)
}

impl Script {
    /// libass's `process_fonts_line`: a `fontname:` line starts a font and
    /// ends the previous one; other lines append to the current font, or are
    /// ignored before any.
    pub(super) fn fonts_line(&mut self, line: &[u8]) -> Result<()> {
        if let Some(name) = line.strip_prefix(b"fontname:") {
            if self.pending_font.is_some() {
                self.decode_font()?;
            }
            self.pending_font = Some(PendingFont {
                name: Bytes::copy_from_slice(&self.budget, crate::scan::trim_start(name))?,
                encoded: BudgetVec::new(&self.budget),
            });
        } else if let Some(font) = &mut self.pending_font
            && self.extract_fonts
        {
            // Without extraction the data would be decoded and dropped.
            font.encoded.try_extend_from_slice(line)?;
        }
        Ok(())
    }

    /// Decodes the pending font and, when fonts are extracted and the result
    /// is not empty, keeps it.
    pub(super) fn decode_font(&mut self) -> Result<()> {
        let Some(font) = self.pending_font.take() else {
            return Ok(());
        };
        if !self.extract_fonts {
            return Ok(());
        }
        let mut data = BudgetVec::new(&self.budget);
        if decode(font.encoded.as_slice(), &mut data)? && !data.is_empty() {
            let data = data.freeze().map_err(|e| e.error)?;
            let font = EmbeddedFont {
                name: font.name,
                data,
            };
            self.fonts.try_push(font).map_err(|e| e.error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::decode;
    use triergon_core::{Budget, BudgetVec};

    #[test]
    fn groups_decode_to_bytes() {
        let budget = Budget::new(1 << 16).unwrap();
        // "Hi!" encodes as the four characters of 0x486921.
        let encode = |v: u32| (0..4).map(move |i| ((v >> (18 - 6 * i)) & 63) as u8 + 33);
        let text: Vec<u8> = encode(0x48_69_21).collect();
        let mut out = BudgetVec::new(&budget);
        assert_eq!(decode(&text, &mut out), Ok(true));
        assert_eq!(out.as_slice(), b"Hi!");
        let mut out = BudgetVec::new(&budget);
        assert_eq!(decode(&text[..2], &mut out), Ok(true));
        assert_eq!(out.as_slice(), b"H");
        assert_eq!(decode(&text[..1], &mut BudgetVec::new(&budget)), Ok(false));
    }
}
