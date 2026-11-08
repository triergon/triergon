//! Three-stage lookup tables over code points.

/// A table mapping every code point to a value, as `cargo xtask shape ucd` packs it.
///
/// Code points below `limit` split into blocks of `2^(middle_bits + leaf_bits)`.
/// `top` gives each block's run of `2^middle_bits` entries in `middle`, each of
/// which gives a run of `2^leaf_bits` values in `leaves`. Identical runs are
/// stored once; code points from `limit` on have the `default` value.
#[derive(Debug)]
pub(super) struct Trie<T: 'static, M: 'static, L: 'static> {
    pub(super) limit: u32,
    pub(super) default: L,
    pub(super) leaf_bits: u32,
    pub(super) middle_bits: u32,
    pub(super) top: &'static [T],
    pub(super) middle: &'static [M],
    pub(super) leaves: &'static [L],
}

impl<T: Copy + Into<usize>, M: Copy + Into<usize>, L: Copy> Trie<T, M, L> {
    /// The value of `cp`; any `u32` is accepted.
    #[inline]
    pub(super) fn get(&self, cp: u32) -> L {
        if cp >= self.limit {
            return self.default;
        }
        let cp = cp as usize;
        let low = |bits: u32, value: usize| value & ((1 << bits) - 1);
        let run = self.top[cp >> (self.middle_bits + self.leaf_bits)].into();
        let entry = (run << self.middle_bits) | low(self.middle_bits, cp >> self.leaf_bits);
        let leaf = (self.middle[entry].into() << self.leaf_bits) | low(self.leaf_bits, cp);
        self.leaves[leaf]
    }
}
