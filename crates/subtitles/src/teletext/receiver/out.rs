//! Budget-charged UTF-8 output.

use crate::decimal::decimal;
use triergon_core::{BudgetVec, Result};

/// Appends UTF-8 text to a charged buffer, growing it geometrically.
#[derive(Debug)]
pub(super) struct Out<'a>(pub(super) &'a mut BudgetVec<u8>);

impl Out<'_> {
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn str(&mut self, text: &str) -> Result<()> {
        self.bytes(text.as_bytes())
    }

    pub(super) fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let buffer = &mut *self.0;
        if buffer.capacity() - buffer.len() < bytes.len() {
            buffer.try_reserve_exact(bytes.len().max(buffer.capacity()).max(1024))?;
        }
        buffer.try_extend_from_slice(bytes)
    }

    pub(super) fn char(&mut self, c: char) -> Result<()> {
        self.str(c.encode_utf8(&mut [0; 4]))
    }

    /// A decimal number.
    pub(super) fn number(&mut self, value: u32) -> Result<()> {
        decimal(value, |digits| self.bytes(digits))
    }

    /// Six uppercase hexadecimal digits.
    pub(super) fn hex24(&mut self, value: u32) -> Result<()> {
        const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
        let digits: [u8; 6] =
            std::array::from_fn(|i| DIGITS[(value >> (20 - 4 * i) & 0xF) as usize]);
        self.bytes(&digits)
    }
}
