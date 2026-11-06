//! Table-driven Huffman decoding (two-level lookup) for all AAC / SBR / PS codebooks.

use crate::bits::BitReader;
use crate::huffman_tables as t;
use std::sync::OnceLock;

/// Two-level lookup table. Primary table indexed by the next `PRIMARY` bits.
/// Entry encoding (u32): bits 0..5 = code length (0 = invalid / escape to subtable),
/// bit 5 = subtable flag, bits 8.. = symbol (or subtable offset).
pub(crate) struct Vlc {
    primary_bits: u32,
    table: Vec<u32>,
    sub_bits: u32,
}

const SUB_FLAG: u32 = 1 << 6;

impl Vlc {
    pub(crate) fn new(codes: &[(u32, u8)], primary_bits: u32) -> Vlc {
        let max_len = codes.iter().map(|c| c.1 as u32).max().unwrap_or(1);
        let primary_bits = primary_bits.min(max_len);
        let sub_bits = max_len.saturating_sub(primary_bits);
        let mut table = vec![0u32; 1usize << primary_bits];
        for (sym, &(code, len)) in codes.iter().enumerate() {
            let len = len as u32;
            if len == 0 {
                continue;
            }
            if len <= primary_bits {
                let base = (code << (primary_bits - len)) as usize;
                for e in &mut table[base..base + (1usize << (primary_bits - len))] {
                    *e = ((sym as u32) << 8) | len;
                }
            } else {
                let prefix = (code >> (len - primary_bits)) as usize;
                let off = if table[prefix] & SUB_FLAG != 0 {
                    (table[prefix] >> 8) as usize
                } else {
                    let off = table.len();
                    table.resize(off + (1usize << sub_bits), 0);
                    table[prefix] = ((off as u32) << 8) | SUB_FLAG;
                    off
                };
                let rem = len - primary_bits;
                let low = code & ((1u32 << rem) - 1);
                let base = off + ((low << (sub_bits - rem)) as usize);
                for e in &mut table[base..base + (1usize << (sub_bits - rem))] {
                    *e = ((sym as u32) << 8) | len;
                }
            }
        }
        Vlc {
            primary_bits,
            table,
            sub_bits,
        }
    }

    /// Decode one symbol. Returns `None` on an invalid code.
    #[inline(always)]
    pub(crate) fn decode(&self, br: &mut BitReader<'_>) -> Option<u32> {
        let bits = br.peek32();
        let e = self.table[(bits >> (32 - self.primary_bits)) as usize];
        if e & SUB_FLAG == 0 {
            let len = e & 0x3f;
            if len == 0 {
                return None;
            }
            br.skip(len as usize);
            return Some(e >> 8);
        }
        let off = (e >> 8) as usize;
        let idx = ((bits << self.primary_bits) >> (32 - self.sub_bits)) as usize;
        let e2 = self.table[off + idx];
        let len = e2 & 0x3f;
        if len == 0 {
            return None;
        }
        br.skip(len as usize);
        Some(e2 >> 8)
    }
}

/// Spectral codebook: VLC plus unpacked tuple values per symbol.
pub(crate) struct SpectralBook {
    pub(crate) vlc: Vlc,
    /// Packed values: for quads 4 x i8, for pairs 2 x i8 (y, z) in bytes 0, 1.
    pub(crate) values: Vec<[i8; 4]>,
    /// The same values inverse quantised, sign(v) |v|^(4/3) (before the escape of book 11).
    pub(crate) fvalues: Vec<[f32; 4]>,
    /// Joint codeword + sign bit lookup on the next `FAST_BITS` bits (0 = not resolvable,
    /// use `vlc`): bits 0..5 = bits consumed, field i (6-bit signed value) at bits 8 + 6i.
    pub(crate) fast: Vec<u32>,
    pub(crate) unsigned: bool,
    pub(crate) quad: bool,
}

pub(crate) struct Books {
    pub(crate) scalefactor: Vlc,
    pub(crate) spectral: [SpectralBook; 11],
    pub(crate) sbr: [Vlc; 10],
    pub(crate) ps: [Vlc; 10],
}

fn spectral_book(n: usize) -> SpectralBook {
    let (codes, unsigned, quad, modulo, offset): (&[(u32, u8)], bool, bool, i32, i32) = match n {
        1 => (&t::SPECTRUM_1, false, true, 3, 1),
        2 => (&t::SPECTRUM_2, false, true, 3, 1),
        3 => (&t::SPECTRUM_3, true, true, 3, 0),
        4 => (&t::SPECTRUM_4, true, true, 3, 0),
        5 => (&t::SPECTRUM_5, false, false, 9, 4),
        6 => (&t::SPECTRUM_6, false, false, 9, 4),
        7 => (&t::SPECTRUM_7, true, false, 8, 0),
        8 => (&t::SPECTRUM_8, true, false, 8, 0),
        9 => (&t::SPECTRUM_9, true, false, 13, 0),
        10 => (&t::SPECTRUM_10, true, false, 13, 0),
        _ => (&t::SPECTRUM_11, true, false, 17, 0),
    };
    let mut values = Vec::with_capacity(codes.len());
    for i in 0..codes.len() as i32 {
        if quad {
            let w = i / 27 - offset;
            let x = (i / 9) % 3 - offset;
            let y = (i / 3) % 3 - offset;
            let z = i % 3 - offset;
            values.push([w as i8, x as i8, y as i8, z as i8]);
        } else {
            let y = i / modulo - offset;
            let z = i % modulo - offset;
            values.push([y as i8, z as i8, 0, 0]);
        }
    }
    let fvalues = values
        .iter()
        .map(|v| {
            v.map(|q| {
                (q.unsigned_abs() as f64).powf(4.0 / 3.0) as f32 * if q < 0 { -1.0 } else { 1.0 }
            })
        })
        .collect();
    let fast = fast_table(codes, &values, unsigned, if quad { 4 } else { 2 });
    SpectralBook {
        vlc: Vlc::new(codes, 9),
        values,
        fvalues,
        fast,
        unsigned,
        quad,
    }
}

/// Index width of `SpectralBook::fast`.
pub(crate) const FAST_BITS: u32 = 10;

/// Value field `i` of a `SpectralBook::fast` entry.
#[inline(always)]
pub(crate) fn fast_field(e: u32, i: usize) -> i32 {
    ((e << (18 - 6 * i)) as i32) >> 26
}

fn fast_table(codes: &[(u32, u8)], values: &[[i8; 4]], unsigned: bool, n: usize) -> Vec<u32> {
    let mut t = vec![0u32; 1 << FAST_BITS];
    for (sym, &(code, len)) in codes.iter().enumerate() {
        let v = values[sym];
        let nz: Vec<usize> = (0..n).filter(|&i| v[i] != 0).collect();
        let nsign = if unsigned { nz.len() as u32 } else { 0 };
        let total = len as u32 + nsign;
        if total > FAST_BITS {
            continue;
        }
        // sign bits follow the codeword, one per non-zero value, MSB first; 1 = negative
        for signs in 0..1u32 << nsign {
            let mut e = total;
            for (i, &vi) in v.iter().enumerate().take(n) {
                let mut x = vi as i32;
                if let Some(p) = nz.iter().position(|&j| j == i).filter(|_| unsigned)
                    && signs >> (nsign - 1 - p as u32) & 1 == 1
                {
                    x = -x;
                }
                e |= ((x as u32) & 63) << (8 + 6 * i);
            }
            let prefix = ((code << nsign) | signs) << (FAST_BITS - total);
            for slot in &mut t[prefix as usize..(prefix + (1 << (FAST_BITS - total))) as usize] {
                *slot = e;
            }
        }
    }
    t
}

pub(crate) fn books() -> &'static Books {
    static BOOKS: OnceLock<Books> = OnceLock::new();
    BOOKS.get_or_init(|| Books {
        scalefactor: Vlc::new(&t::SCALEFACTOR, 9),
        spectral: [
            spectral_book(1),
            spectral_book(2),
            spectral_book(3),
            spectral_book(4),
            spectral_book(5),
            spectral_book(6),
            spectral_book(7),
            spectral_book(8),
            spectral_book(9),
            spectral_book(10),
            spectral_book(11),
        ],
        sbr: [
            Vlc::new(&t::SBR_T_ENV15, 10),
            Vlc::new(&t::SBR_F_ENV15, 10),
            Vlc::new(&t::SBR_T_ENVBAL15, 10),
            Vlc::new(&t::SBR_F_ENVBAL15, 10),
            Vlc::new(&t::SBR_T_ENV30, 10),
            Vlc::new(&t::SBR_F_ENV30, 10),
            Vlc::new(&t::SBR_T_ENVBAL30, 10),
            Vlc::new(&t::SBR_F_ENVBAL30, 10),
            Vlc::new(&t::SBR_T_NOISE30, 10),
            Vlc::new(&t::SBR_T_NOISEBAL30, 10),
        ],
        ps: [
            Vlc::new(&t::PS_IID_DF1, 10),
            Vlc::new(&t::PS_IID_DT1, 10),
            Vlc::new(&t::PS_IID_DF0, 10),
            Vlc::new(&t::PS_IID_DT0, 10),
            Vlc::new(&t::PS_ICC_DF, 10),
            Vlc::new(&t::PS_ICC_DT, 10),
            Vlc::new(&t::PS_IPD_DF, 10),
            Vlc::new(&t::PS_IPD_DT, 10),
            Vlc::new(&t::PS_OPD_DF, 10),
            Vlc::new(&t::PS_OPD_DT, 10),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, codes: &[(u32, u8)]) {
        // Kraft equality (complete code) and prefix freedom.
        let max = codes.iter().map(|c| c.1).max().unwrap() as u32;
        let mut sum: u128 = 0;
        for &(c, l) in codes {
            assert!(c < (1u32 << l), "{name}: code too long");
            sum += 1u128 << (max - l as u32);
        }
        assert_eq!(sum, 1u128 << max, "{name}: Kraft sum");
        let mut v: Vec<(u32, u8)> = codes.to_vec();
        v.sort_by_key(|&(c, l)| ((c as u64) << (32 - l as u32), l));
        for w in v.windows(2) {
            let (a, la) = w[0];
            let (b, lb) = w[1];
            if la <= lb {
                assert_ne!(b >> (lb - la), a, "{name}: prefix conflict");
            }
        }
        // Every symbol decodes back to itself.
        let vlc = Vlc::new(codes, 9);
        for (sym, &(c, l)) in codes.iter().enumerate() {
            let word = (c as u64) << (40 - l as u32);
            let bytes = word.to_be_bytes();
            let mut br = BitReader::new(&bytes[3..]);
            assert_eq!(vlc.decode(&mut br), Some(sym as u32), "{name} sym {sym}");
            assert_eq!(br.position(), l as usize);
        }
    }

    #[test]
    fn sbr_frequency_balance_3db_reads_the_complete_outer_branches() {
        let vlc = &books().sbr[7];
        // Read actual wire words with nonzero trailing bits. Checking the
        // cursor catches an incorrect long leaf even if its symbol agrees.
        for (wire, len, delta) in [
            (0b11111111100_u16, 11, -5),
            (0b111111111010, 12, 5),
            (0b1111111110110, 13, 6),
            (0b1111111110111, 13, -12),
            (0b1111111111011, 13, -8),
            (0b11111111111000, 14, -7),
            (0b11111111111001, 14, -6),
            (0b11111111111010, 14, 7),
            (0b11111111111111, 14, 12),
        ] {
            let trailing = u16::MAX >> len;
            let bytes = ((wire << (16 - len)) | trailing).to_be_bytes();
            let mut reader = BitReader::new(&bytes);
            assert_eq!(vlc.decode(&mut reader), Some((delta + 12) as u32));
            assert_eq!(reader.position(), len);
        }
    }

    #[test]
    fn all_codebooks_complete_and_prefix_free() {
        check("sf", &t::SCALEFACTOR);
        for (i, c) in [
            &t::SPECTRUM_1[..],
            &t::SPECTRUM_2,
            &t::SPECTRUM_3,
            &t::SPECTRUM_4,
            &t::SPECTRUM_5,
            &t::SPECTRUM_6,
            &t::SPECTRUM_7,
            &t::SPECTRUM_8,
            &t::SPECTRUM_9,
            &t::SPECTRUM_10,
            &t::SPECTRUM_11,
        ]
        .iter()
        .enumerate()
        {
            check(&format!("cb{}", i + 1), c);
        }
        for c in [
            &t::SBR_T_ENV15[..],
            &t::SBR_F_ENV15,
            &t::SBR_T_ENVBAL15,
            &t::SBR_F_ENVBAL15,
            &t::SBR_T_ENV30,
            &t::SBR_F_ENV30,
            &t::SBR_T_ENVBAL30,
            &t::SBR_F_ENVBAL30,
            &t::SBR_T_NOISE30,
            &t::SBR_T_NOISEBAL30,
        ] {
            check("sbr", c);
        }
        for c in [
            &t::PS_IID_DF1[..],
            &t::PS_IID_DT1,
            &t::PS_IID_DF0,
            &t::PS_IID_DT0,
            &t::PS_ICC_DF,
            &t::PS_ICC_DT,
            &t::PS_IPD_DF,
            &t::PS_IPD_DT,
            &t::PS_OPD_DF,
            &t::PS_OPD_DT,
        ] {
            check("ps", c);
        }
    }
}
