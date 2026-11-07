//! CAVLC residual parsing (clause 9.2) with two-level lookup tables built from the
//! spec tables in `tables_vlc` (Tables 9-5, 9-7, 9-8, 9-9, 9-10).

use crate::tables_vlc::*;
use std::sync::OnceLock;
use triergon_bitstream::BitReader;

/// Two-level VLC lookup table. Entry: (symbol or subtable base, length or -subtable_bits).
pub(crate) struct Vlc {
    bits: u32,
    table: Vec<(u16, i8)>,
}

impl Vlc {
    /// Build from (code, len, symbol) entries; codes must be prefix-free.
    fn build(codes: &[(u32, u8, u16)], bits: u32) -> Vlc {
        let size = 1usize << bits;
        let mut table = vec![(0u16, 0i8); size];
        // First level.
        for &(code, len, sym) in codes {
            let len = len as u32;
            if len <= bits {
                let base = (code << (bits - len)) as usize;
                for k in 0..(1usize << (bits - len)) {
                    table[base + k] = (sym, len as i8);
                }
            }
        }
        // Second level, per long-code prefix.
        let mut prefixes: Vec<u32> = codes
            .iter()
            .filter(|c| c.1 as u32 > bits)
            .map(|c| c.0 >> (c.1 as u32 - bits))
            .collect();
        prefixes.sort();
        prefixes.dedup();
        for p in prefixes {
            let sub_bits = codes
                .iter()
                .filter(|c| c.1 as u32 > bits && c.0 >> (c.1 as u32 - bits) == p)
                .map(|c| c.1 as u32 - bits)
                .max()
                .unwrap();
            let base = table.len();
            table.resize(base + (1 << sub_bits), (0, 0));
            for &(code, len, sym) in codes {
                let len = len as u32;
                if len > bits && code >> (len - bits) == p {
                    let rem = len - bits;
                    let low = code & ((1 << rem) - 1);
                    let b = base + ((low << (sub_bits - rem)) as usize);
                    for k in 0..(1usize << (sub_bits - rem)) {
                        table[b + k] = (sym, rem as i8);
                    }
                }
            }
            table[p as usize] = (base as u16, -(sub_bits as i8));
        }
        Vlc { bits, table }
    }

    /// Returns the symbol, or `None` for an invalid codeword.
    #[inline(always)]
    pub(crate) fn read(&self, r: &mut BitReader<'_>) -> Option<u16> {
        let (s, l) = self.table[r.peek(self.bits) as usize];
        if l > 0 {
            r.skip(l as usize);
            Some(s)
        } else if l < 0 {
            r.skip(self.bits as usize);
            let sub = (-l) as u32;
            let (s2, l2) = self.table[s as usize + r.peek(sub) as usize];
            if l2 <= 0 {
                return None;
            }
            r.skip(l2 as usize);
            Some(s2)
        } else {
            None
        }
    }
}

pub(crate) struct CavlcTables {
    /// coeff_token for nC ranges 0..2, 2..4, 4..8, >=8, -1 (chroma DC 4:2:0). Symbol = tc * 4 + t1.
    pub coeff_token: [Vlc; 5],
    pub total_zeros: [Vlc; 15],
    pub total_zeros_dc: [Vlc; 3],
    #[cfg_attr(not(test), allow(dead_code))]
    pub run_before: [Vlc; 7],
}

fn cvt3(t: &[(u32, u8, u8)]) -> Vec<(u32, u8, u16)> {
    t.iter().map(|&(c, l, v)| (c, l, v as u16)).collect()
}

pub(crate) fn tables() -> &'static CavlcTables {
    static T: OnceLock<CavlcTables> = OnceLock::new();
    T.get_or_init(|| {
        let ct = |t: &[(u32, u8, u8, u8)]| {
            let v: Vec<(u32, u8, u16)> = t
                .iter()
                .map(|&(c, l, tc, t1)| (c, l, (tc as u16) * 4 + t1 as u16))
                .collect();
            Vlc::build(&v, 8)
        };
        CavlcTables {
            coeff_token: [
                ct(&COEFF_TOKEN_0),
                ct(&COEFF_TOKEN_1),
                ct(&COEFF_TOKEN_2),
                ct(&COEFF_TOKEN_3),
                ct(&COEFF_TOKEN_4),
            ],
            total_zeros: std::array::from_fn(|i| Vlc::build(&cvt3(TOTAL_ZEROS_4X4[i]), 6)),
            total_zeros_dc: std::array::from_fn(|i| Vlc::build(&cvt3(TOTAL_ZEROS_2X2[i]), 3)),
            run_before: std::array::from_fn(|i| Vlc::build(&cvt3(RUN_BEFORE[i]), 6)),
        }
    })
}

/// run_before (Table 9-10): small direct table for zerosLeft <= 6, closed form for > 6.
#[inline(always)]
fn read_run_before(r: &mut BitReader<'_>, zeros_left: i32) -> i32 {
    // [zerosLeft - 1][next 3 bits] -> (run_before, length)
    const T: [[(u8, u8); 8]; 6] = [
        [
            (1, 1),
            (1, 1),
            (1, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
        ],
        [
            (2, 2),
            (2, 2),
            (1, 2),
            (1, 2),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
        ],
        [
            (3, 2),
            (3, 2),
            (2, 2),
            (2, 2),
            (1, 2),
            (1, 2),
            (0, 2),
            (0, 2),
        ],
        [
            (4, 3),
            (3, 3),
            (2, 2),
            (2, 2),
            (1, 2),
            (1, 2),
            (0, 2),
            (0, 2),
        ],
        [
            (5, 3),
            (4, 3),
            (3, 3),
            (2, 3),
            (1, 2),
            (1, 2),
            (0, 2),
            (0, 2),
        ],
        [
            (1, 3),
            (2, 3),
            (4, 3),
            (3, 3),
            (6, 3),
            (5, 3),
            (0, 2),
            (0, 2),
        ],
    ];
    if zeros_left <= 6 {
        let (v, l) = T[(zeros_left - 1) as usize][r.peek(3) as usize];
        r.skip(l as usize);
        v as i32
    } else {
        let w = r.peek(16);
        if w >> 13 != 0 {
            r.skip(3);
            7 - (w >> 13) as i32
        } else {
            let lz = w.leading_zeros() - 16;
            r.skip((lz + 1).min(16) as usize);
            4 + lz as i32
        }
    }
}

/// residual_block_cavlc (clause 7.3.5.3.2 / 9.2). `nc` is -1 for chroma DC (4:2:0).
/// Writes coefficient level `k` (scan index start_idx + k) to `dst[map[start_idx + k]]` and
/// returns TotalCoeff, or `None` on invalid data. `dst` must be zero on entry.
#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn residual_block(
    r: &mut BitReader<'_>,
    t: &CavlcTables,
    nc: i32,
    dst: &mut [i32],
    map: &[u8],
    start_idx: usize,
    end_idx: usize,
    max_num_coeff: usize,
) -> Option<u32> {
    // Work on a register copy of the reader (no stores on the panic paths of the hot loops).
    let mut br = *r;
    let res = residual_block_inner(&mut br, t, nc, dst, map, start_idx, end_idx, max_num_coeff);
    *r = br;
    res
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn residual_block_inner(
    r: &mut BitReader<'_>,
    t: &CavlcTables,
    nc: i32,
    dst: &mut [i32],
    map: &[u8],
    start_idx: usize,
    end_idx: usize,
    max_num_coeff: usize,
) -> Option<u32> {
    let vt = match nc {
        -1 => 4,
        0..=1 => 0,
        2..=3 => 1,
        4..=7 => 2,
        _ => 3,
    };
    let sym = if vt == 3 {
        // 8 <= nC: 6-bit fixed length code (Table 9-5 column 4: xxxxyy with special 000011).
        let v = r.read(6);
        if v == 3 {
            0
        } else {
            let tc = (v >> 2) + 1;
            let t1 = v & 3;
            if t1 > tc.min(3) {
                return None;
            }
            (tc * 4 + t1) as u16
        }
    } else {
        t.coeff_token[vt].read(r)?
    };
    let total = (sym >> 2) as usize;
    let t1s = (sym & 3) as usize;
    if total == 0 {
        return Some(0);
    }
    if total > max_num_coeff {
        return None;
    }
    let mut level_val = [0i32; 16];
    // Trailing ones signs.
    if t1s > 0 {
        let signs = r.read(t1s as u32);
        for i in 0..t1s {
            level_val[i] = if (signs >> (t1s - 1 - i)) & 1 != 0 {
                -1
            } else {
                1
            };
        }
    }
    let mut suffix_length: u32 = if total > 10 && t1s < 3 { 1 } else { 0 };
    for i in t1s..total {
        // level_prefix (9.2.2.1) and level_suffix from one 32-bit window.
        let w = r.peek(32);
        let lz = w.leading_zeros();
        if lz >= 32 {
            return None;
        }
        let level_prefix = lz;
        let level_suffix_size = if level_prefix == 14 && suffix_length == 0 {
            4
        } else if level_prefix >= 15 {
            level_prefix - 3
        } else {
            suffix_length
        };
        let level_suffix = if level_suffix_size == 0 {
            r.skip((lz + 1) as usize);
            0
        } else if lz + 1 + level_suffix_size <= 32 {
            r.skip((lz + 1 + level_suffix_size) as usize);
            ((w << (lz + 1)) >> (32 - level_suffix_size)) as i32
        } else {
            r.skip((lz + 1) as usize);
            r.read(level_suffix_size) as i32
        };
        let mut level_code = ((level_prefix.min(15) as i32) << suffix_length) + level_suffix;
        if level_prefix >= 15 && suffix_length == 0 {
            level_code += 15;
        }
        if level_prefix >= 16 {
            level_code += (1 << (level_prefix - 3)) - 4096;
        }
        if i == t1s && t1s < 3 {
            level_code += 2;
        }
        let v = if level_code & 1 == 0 {
            (level_code + 2) >> 1
        } else {
            (-level_code - 1) >> 1
        };
        level_val[i] = v;
        if suffix_length == 0 {
            suffix_length = 1;
        }
        if v.unsigned_abs() > (3u32 << (suffix_length - 1)) && suffix_length < 6 {
            suffix_length += 1;
        }
    }
    // total_zeros
    let mut zeros_left: i32 = if total < end_idx - start_idx + 1 {
        let tz = if nc == -1 {
            t.total_zeros_dc[total - 1].read(r)?
        } else {
            t.total_zeros[total - 1].read(r)?
        };
        tz as i32
    } else {
        0
    };
    if zeros_left as usize + total > max_num_coeff {
        return None;
    }
    // runs and placement (clause 9.2.4), from highest frequency coefficient downward.
    let mut coeff_num: i32 = (total as i32 + zeros_left) - 1;
    for i in 0..total {
        let run = if i + 1 < total && zeros_left > 0 {
            let rb = read_run_before(r, zeros_left);
            if rb > zeros_left {
                return None;
            }
            rb
        } else if i + 1 < total {
            0
        } else {
            zeros_left
        };
        let pos = start_idx as i32 + coeff_num;
        if pos < 0 || pos as usize >= map.len() {
            return None;
        }
        let d = map[pos as usize] as usize;
        if d >= dst.len() {
            return None;
        }
        dst[d] = level_val[i];
        zeros_left -= run;
        coeff_num -= run + 1;
    }
    Some(total as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kraft(codes: &[(u32, u8, u16)]) -> f64 {
        codes.iter().map(|c| 0.5f64.powi(c.1 as i32)).sum()
    }

    #[test]
    fn run_before_fast_path_matches_table() {
        let t = tables();
        for zl in 1..=14i32 {
            for bits in 0u32..(1 << 12) {
                let mut buf = vec![0u8; 32];
                buf[..4].copy_from_slice(&((bits << 20) | 0x8_0000).to_be_bytes());
                let mut a = BitReader::with_end(&buf, 8 * 8);
                let mut b = BitReader::with_end(&buf, 8 * 8);
                let va = t.run_before[(zl.min(7) - 1) as usize].read(&mut a);
                let vb = read_run_before(&mut b, zl);
                if let Some(va) = va
                    && va as i32 <= zl
                {
                    assert_eq!(va as i32, vb, "zl {zl} bits {bits:012b}");
                    assert_eq!(a.position(), b.position());
                }
            }
        }
    }

    #[test]
    fn vlc_tables_prefix_free_and_decodable() {
        let all: Vec<Vec<(u32, u8, u16)>> = [
            &COEFF_TOKEN_0[..],
            &COEFF_TOKEN_1[..],
            &COEFF_TOKEN_2[..],
            &COEFF_TOKEN_4[..],
        ]
        .iter()
        .map(|t| {
            t.iter()
                .map(|&(c, l, tc, t1)| (c, l, tc as u16 * 4 + t1 as u16))
                .collect()
        })
        .collect();
        for codes in &all {
            assert!(kraft(codes) <= 1.0 + 1e-12);
            let vlc = Vlc::build(codes, 8);
            for &(code, len, sym) in codes {
                let mut buf = vec![0u8; 32];
                let v = (code as u64) << (64 - len as u32);
                buf[..8].copy_from_slice(&v.to_be_bytes());
                let mut r = BitReader::with_end(&buf, 8 * 8);
                assert_eq!(vlc.read(&mut r), Some(sym));
                assert_eq!(r.position(), len as usize);
            }
        }
        for t in TOTAL_ZEROS_4X4
            .iter()
            .chain(TOTAL_ZEROS_2X2.iter())
            .chain(RUN_BEFORE.iter())
        {
            let c = cvt3(t);
            assert!((kraft(&c) - 1.0).abs() < 1e-9 || kraft(&c) < 1.0);
            let vlc = Vlc::build(&c, 6);
            for &(code, len, sym) in &c {
                let mut buf = vec![0u8; 32];
                let v = (code as u64) << (64 - len as u32);
                buf[..8].copy_from_slice(&v.to_be_bytes());
                let mut r = BitReader::with_end(&buf, 8 * 8);
                assert_eq!(vlc.read(&mut r), Some(sym));
            }
        }
    }
}
