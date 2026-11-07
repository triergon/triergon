//! The range decoder of RFC 6716 section 4.1, shared by the SILK and CELT layers.

/// Range decoder state over one Opus frame. Entropy-coded symbols are read from the
/// front and raw bits (CELT) from the back of the same bytes.
#[derive(Clone, Debug)]
pub(crate) struct RangeDecoder<'a> {
    data: &'a [u8],
    /// Next byte read from the front.
    offset: usize,
    /// Bytes read from the back for raw bits.
    end_offset: usize,
    end_window: u32,
    end_bits: u32,
    /// Bits consumed, for `tell` (RFC 6716 section 4.1.6).
    total_bits: i32,
    rng: u32,
    val: u32,
    rem: u32,
    /// A decoded symbol or raw-bit read was out of range.
    error: bool,
}

const SYM_BITS: u32 = 8;
const CODE_EXTRA: u32 = 7;
const CODE_TOP: u32 = 1 << 31;
const CODE_BOT: u32 = 1 << 23;
const SYM_MAX: u32 = 255;
const WINDOW_BITS: u32 = 32;

/// `ilog(x)`: bits needed to represent `x`, 0 for 0 (RFC 6716 section 1.1.10).
#[inline(always)]
pub(crate) fn ilog(x: u32) -> u32 {
    32 - x.leading_zeros()
}

impl<'a> RangeDecoder<'a> {
    /// Start decoding `data` (RFC 6716 section 4.1.1).
    pub(crate) fn new(data: &'a [u8]) -> Self {
        let mut decoder = RangeDecoder {
            data,
            offset: 0,
            end_offset: 0,
            end_window: 0,
            end_bits: 0,
            total_bits: (32 + 1 - ((32 - CODE_EXTRA) / SYM_BITS) * SYM_BITS) as i32,
            rng: 1 << CODE_EXTRA,
            val: 0,
            rem: 0,
            error: false,
        };
        decoder.rem = decoder.read_front();
        decoder.val = decoder.rng - 1 - (decoder.rem >> (SYM_BITS - CODE_EXTRA));
        decoder.normalize();
        decoder
    }

    #[inline(always)]
    fn read_front(&mut self) -> u32 {
        // Past the end the decoder reads zeros; the front never overlaps raw bits the
        // encoder wrote, and a malformed frame only decodes garbage.
        let byte = if self.offset + self.end_offset < self.data.len() {
            u32::from(self.data[self.offset])
        } else {
            0
        };
        self.offset += 1;
        byte
    }

    #[inline(always)]
    fn read_back(&mut self) -> u32 {
        if self.offset + self.end_offset < self.data.len() {
            self.end_offset += 1;
            u32::from(self.data[self.data.len() - self.end_offset])
        } else {
            0
        }
    }

    #[inline(always)]
    fn normalize(&mut self) {
        while self.rng <= CODE_BOT {
            self.total_bits += SYM_BITS as i32;
            self.rng <<= SYM_BITS;
            let previous = self.rem;
            self.rem = self.read_front();
            let sym = ((previous << SYM_BITS) | self.rem) >> (SYM_BITS - CODE_EXTRA);
            self.val = ((self.val << SYM_BITS) + (SYM_MAX & !sym)) & (CODE_TOP - 1);
        }
    }

    /// `ec_decode`: the cumulative frequency of the next symbol under total `ft`
    /// (RFC 6716 section 4.1.2). Must be followed by [`Self::update`].
    #[inline(always)]
    pub(crate) fn decode(&mut self, ft: u32) -> (u32, u32) {
        let ext = self.rng / ft;
        let fs = self.val / ext + 1;
        (ft - fs.min(ft), ext)
    }

    /// `ec_dec_update` for a symbol with cumulative range `fl..fh` of `ft`; `ext` is the
    /// second value [`Self::decode`] returned.
    #[inline(always)]
    pub(crate) fn update(&mut self, fl: u32, fh: u32, ft: u32, ext: u32) {
        let s = ext * (ft - fh);
        self.val -= s;
        self.rng = if fl > 0 { ext * (fh - fl) } else { self.rng - s };
        self.normalize();
    }

    /// A symbol from a frequency table given as `(fl, fh)` lookups: decodes with
    /// `ec_decode` and returns the index `k` with `cdf[k] <= fs < cdf[k + 1]`, where
    /// `cdf` has `ft = cdf[last]`.
    pub(crate) fn decode_cdf(&mut self, cdf: &[u32]) -> usize {
        let ft = *cdf.last().unwrap_or(&1);
        let (fs, ext) = self.decode(ft);
        let k = cdf[1..].iter().position(|&c| fs < c).unwrap_or(cdf.len() - 2);
        self.update(cdf[k], cdf[k + 1], ft, ext);
        k
    }

    /// `ec_dec_bit_logp`: one bit whose probability of being 1 is `1 / 2^logp`.
    #[inline(always)]
    pub(crate) fn bit_logp(&mut self, logp: u32) -> bool {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let bit = d < s;
        if !bit {
            self.val = d - s;
        }
        self.rng = if bit { s } else { r - s };
        self.normalize();
        bit
    }

    /// `ec_dec_icdf`: a symbol from an "inverse" CDF table whose entries are
    /// `2^ftb - cdf[k + 1]`, decreasing to 0 (RFC 6716 section 4.1.3.3).
    #[inline(always)]
    pub(crate) fn icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut k = 0;
        let mut t;
        loop {
            t = s;
            s = r * u32::from(icdf[k]);
            if d >= s {
                break;
            }
            k += 1;
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        k
    }

    /// `ec_dec_uint`: a value uniformly distributed in `0..ft`, `ft > 1` (RFC 6716
    /// section 4.1.5). An out-of-range value sets the error flag and saturates.
    pub(crate) fn uint(&mut self, ft: u32) -> u32 {
        debug_assert!(ft > 1);
        let ft1 = ft - 1;
        let bits = ilog(ft1);
        if bits > 8 {
            let shift = bits - 8;
            let top = (ft1 >> shift) + 1;
            let (s, ext) = self.decode(top);
            self.update(s, s + 1, top, ext);
            let value = (s << shift) | self.bits(shift);
            if value > ft1 {
                self.error = true;
                return ft1;
            }
            value
        } else {
            let (s, ext) = self.decode(ft);
            self.update(s, s + 1, ft, ext);
            s
        }
    }

    /// `ec_dec_bits`: `count` raw bits (at most 25) from the back of the frame, LSB
    /// first (RFC 6716 section 4.1.4).
    pub(crate) fn bits(&mut self, count: u32) -> u32 {
        debug_assert!(count <= 25);
        let mut window = self.end_window;
        let mut available = self.end_bits;
        if available < count {
            loop {
                window |= self.read_back() << available;
                available += SYM_BITS;
                if available > WINDOW_BITS - SYM_BITS {
                    break;
                }
            }
        }
        let value = window & ((1u32 << count) - 1);
        self.end_window = window >> count;
        self.end_bits = available - count;
        self.total_bits += count as i32;
        value
    }

    /// `ec_tell`: whole bits used so far, rounded up (RFC 6716 section 4.1.6.1).
    #[inline(always)]
    pub(crate) fn tell(&self) -> i32 {
        self.total_bits - ilog(self.rng) as i32
    }

    /// `ec_tell_frac`: bits used so far in 1/8 bit units, rounded up (section 4.1.6.2).
    pub(crate) fn tell_frac(&self) -> u32 {
        let whole = (self.total_bits as u32) << 3;
        let mut l = ilog(self.rng);
        let mut r = self.rng >> (l - 16);
        for _ in 0..3 {
            r = (r * r) >> 15;
            let b = r >> 16;
            l = (l << 1) | b;
            r >>= b;
        }
        whole - l
    }

    /// Total frame bytes.
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    /// The final range value, which the reference test vectors record per packet.
    pub(crate) fn range(&self) -> u32 {
        self.rng
    }

    /// Whether a symbol or raw-bit read was out of range.
    pub(crate) fn error(&self) -> bool {
        self.error
    }

    /// Shrink the frame so that the last `bytes` bytes are no longer read (used when a
    /// hybrid or redundancy split moves the end, RFC 6716 section 4.5.1).
    pub(crate) fn shrink(&mut self, bytes: usize) {
        self.data = &self.data[..self.data.len().saturating_sub(bytes)];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_frame_decodes_the_most_probable_symbols() {
        let mut rc = RangeDecoder::new(&[]);
        assert_eq!(rc.tell(), 1);
        // With all-zero input the decoder sees the highest cumulative frequencies.
        assert!(!rc.bit_logp(15));
        assert_eq!(rc.bits(8), 0);
        assert!(rc.tell() >= 9);
        assert!(!rc.error());
    }

    #[test]
    fn tell_frac_tracks_tell() {
        let data = [0x5a, 0xc3, 0x11, 0x80, 0x7f, 0x00, 0xff, 0x42];
        let mut rc = RangeDecoder::new(&data);
        for logp in [1, 2, 3, 15, 4, 1, 1] {
            let _ = rc.bit_logp(logp);
            let frac = rc.tell_frac() as i32;
            assert!(frac <= rc.tell() * 8 && frac > (rc.tell() - 1) * 8, "{frac} {}", rc.tell());
        }
        let _ = rc.uint(300);
        assert!(rc.tell() <= 64 + 8);
    }
}
