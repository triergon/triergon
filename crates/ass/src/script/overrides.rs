//! `[Style.]Field=Value` overrides, libass's `ass_process_force_style`.

use triergon_core::{Result, WorkBudget};

use super::Script;
use super::info::Header;
use super::style::Field;
use crate::charge;
use crate::number::header_int;

impl Script {
    /// One override. The value follows the last `=`. Header names set the
    /// header; then the field after the last `.` is set in the style named
    /// before it (ignoring ASCII case), or in every style without a `.`.
    /// Style fields keep their values as written: no `/100` scale, no
    /// alignment conversion, no clamping.
    pub(super) fn apply_override(&mut self, item: &[u8], work: &mut WorkBudget) -> Result<()> {
        let Some(eq) = item.iter().rposition(|&c| c == b'=') else {
            return Ok(());
        };
        let (key, value) = (&item[..eq], &item[eq + 1..]);
        if let Some(header) = Header::overridable(key) {
            self.info.set(header, value);
        }
        let (style, field) = match key.iter().rposition(|&c| c == b'.') {
            Some(dot) => (Some(&key[..dot]), &key[dot + 1..]),
            None => (None, key),
        };
        let Some(field) = Field::named(field).filter(|&f| f != Field::Name) else {
            return Ok(());
        };
        charge(work, self.styles.len())?;
        for target in self.styles.as_mut_slice() {
            if style.is_some_and(|s| !target.name().eq_ignore_ascii_case(s)) {
                continue;
            }
            match field {
                Field::FontName => target.set_font_name(&self.budget, value)?,
                Field::AlphaLevel => {
                    let alpha = header_int(value);
                    target.set_alpha(alpha, alpha);
                }
                field => target.assign(field, value),
            }
        }
        Ok(())
    }
}
