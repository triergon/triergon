//! A record's byte strings in one budgeted allocation.

use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

/// `N` byte strings, each possibly absent (a NULL pointer in libass), stored
/// back to back in one allocation.
#[derive(Clone, Debug)]
pub(crate) struct Strings<const N: usize> {
    data: Bytes,
    ends: [u32; N],
    present: [bool; N],
}

impl<const N: usize> Strings<N> {
    pub(crate) fn new(budget: &Budget, parts: [Option<&[u8]>; N]) -> Result<Self> {
        let total = parts.iter().flatten().map(|p| p.len()).sum::<usize>();
        u32::try_from(total).map_err(|_| Error::Policy("ass string length"))?;
        let mut data = BudgetVec::new(budget);
        data.try_reserve_exact(total)?;
        let mut ends = [0; N];
        for (end, part) in ends.iter_mut().zip(parts) {
            data.try_extend_from_slice(part.unwrap_or_default())?;
            *end = data.len() as u32;
        }
        let data = data.freeze().map_err(|e| e.error)?;
        Ok(Self {
            data,
            ends,
            present: parts.map(|p| p.is_some()),
        })
    }

    pub(crate) fn get(&self, index: usize) -> Option<&[u8]> {
        self.present[index].then(|| {
            let start = index.checked_sub(1).map_or(0, |i| self.ends[i]);
            &self.data[start as usize..self.ends[index] as usize]
        })
    }

    /// The same strings with `index` replaced.
    pub(crate) fn with(&self, budget: &Budget, index: usize, value: &[u8]) -> Result<Self> {
        let mut parts: [Option<&[u8]>; N] = std::array::from_fn(|i| self.get(i));
        parts[index] = Some(value);
        Self::new(budget, parts)
    }
}

#[cfg(test)]
mod tests {
    use super::Strings;
    use triergon_core::Budget;

    #[test]
    fn strings_keep_absence_and_order() {
        let budget = Budget::new(1 << 16).unwrap();
        let s = Strings::new(&budget, [Some(&b"text"[..]), None, Some(b"")]).unwrap();
        assert_eq!(s.get(0), Some(&b"text"[..]));
        assert_eq!(s.get(1), None);
        assert_eq!(s.get(2), Some(&b""[..]));
        let t = s.with(&budget, 1, b"name").unwrap();
        assert_eq!(
            (t.get(0), t.get(1)),
            (Some(&b"text"[..]), Some(&b"name"[..]))
        );
    }
}
