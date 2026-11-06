//! Core-embedded X96 frequency extension, DTS_BCCORE_X96 (ETSI TS 102 114 clause 6.2.4:
//! Tables 6-1, 6-4, 6-8, 6-9, 6-10).

use crate::DecodeError;
use crate::bits::BitReader;
use crate::books;
use crate::subframe::{HIST, SubBuf, sat24};
use crate::tables::{JOINT_SCALE, SCALE_6BIT, STEP_LOSSLESS, STEP_LOSSY};
use crate::vq::{ADPCM_VQ, HF_VQ};

pub(crate) const X96_SYNC: u32 = 0x1D95_F262;
const MAX_CH: usize = 5;

/// X96 channel set header (Table 6-4, DTS_BCCORE_X96 form).
pub(crate) struct X96Header {
    pub nch: usize,
    pub highres: bool,
    pub sbs: usize,
    pub sbe: [usize; MAX_CH],
    pub joinx: [u8; MAX_CH],
    pub shuff: [u8; MAX_CH],
    pub bhuff: [u8; MAX_CH],
    /// SEL96 by ABITS96 - 2 (ABITS96 2..=11).
    pub sel: [[u8; 10]; MAX_CH],
}

/// Locate SYNC96: 32-bit aligned, its distance to the frame end equals FSIZE96.
pub(crate) fn find(frame: &[u8], from_bit: usize) -> Option<usize> {
    let end = frame.len() * 8;
    let mut p = from_bit.div_ceil(32) * 32;
    while p + 64 <= end {
        let byte = p / 8;
        if u32::from_be_bytes([
            frame[byte],
            frame[byte + 1],
            frame[byte + 2],
            frame[byte + 3],
        ]) == X96_SYNC
        {
            let mut b = BitReader::new(frame);
            b.seek(p + 32);
            let fsize96 = b.read(12) as usize + 1;
            if frame.len() - byte == fsize96 {
                return Some(p);
            }
        }
        p += 32;
    }
    None
}

impl X96Header {
    /// Parse the frame header (after SYNC96) and the single channel set header.
    /// `Ok(None)`: the extension must be ignored (REVNO 0 or above 8; Table 6-2), and the
    /// core decodes at its own rate, as FFmpeg does.
    pub(crate) fn parse(
        br: &mut BitReader<'_>,
        nch: usize,
        cpf: bool,
    ) -> Result<Option<Self>, DecodeError> {
        let _fsize96 = br.read(12);
        let revno = br.read(4);
        if revno == 0 || revno > 8 {
            return Ok(None);
        }
        if nch > MAX_CH {
            return Err(DecodeError::Unsupported("X96 with more than 5 channels"));
        }
        let highres = br.bit();
        let sbs = if revno < 8 {
            let v = br.read(5) as usize;
            if v > 27 {
                return Err(DecodeError::InvalidData("X96 nSBS96"));
            }
            v
        } else {
            32
        };
        let mut h = X96Header {
            nch,
            highres,
            sbs,
            sbe: [0; MAX_CH],
            joinx: [0; MAX_CH],
            shuff: [0; MAX_CH],
            bhuff: [0; MAX_CH],
            sel: [[0; 10]; MAX_CH],
        };
        for ch in 0..nch {
            h.sbe[ch] = br.read(6) as usize + 1;
            if h.sbe[ch] < 32 {
                return Err(DecodeError::InvalidData("X96 SBE96 below 31"));
            }
        }
        for ch in 0..nch {
            h.joinx[ch] = br.read(3) as u8;
            let j = h.joinx[ch] as usize;
            if j > ch {
                return Err(DecodeError::InvalidData("X96 JOINX96 source channel"));
            }
        }
        for ch in 0..nch {
            h.shuff[ch] = br.read(3) as u8;
            if h.shuff[ch] > 5 {
                return Err(DecodeError::InvalidData("X96 SHUFF96"));
            }
        }
        for ch in 0..nch {
            h.bhuff[ch] = br.read(3) as u8;
        }
        // SEL96: ABITS96 2 -> 1 bit, 3..=6 -> 2 bits, 7 -> 3 bits, 8..=11 (high res) -> 3 bits.
        let count = if highres { 10 } else { 6 };
        for n in 0..count {
            let w = match n {
                0 => 1,
                1..=4 => 2,
                _ => 3,
            };
            for ch in 0..nch {
                h.sel[ch][n] = br.read(w) as u8;
            }
        }
        if cpf {
            let _ahcrc96 = br.read(16);
        }
        Ok(Some(h))
    }
}

pub(crate) struct X96Side {
    pmode: [[bool; 64]; MAX_CH],
    pvq: [[u16; 64]; MAX_CH],
    abits: [[u8; 64]; MAX_CH],
    scale: [[f32; 64]; MAX_CH],
    jscale: [[f32; 64]; MAX_CH],
}

impl X96Side {
    pub(crate) fn new() -> Self {
        Self {
            pmode: [[false; 64]; MAX_CH],
            pvq: [[0; 64]; MAX_CH],
            abits: [[0; 64]; MAX_CH],
            scale: [[0.0; 64]; MAX_CH],
            jscale: [[0.0; 64]; MAX_CH],
        }
    }
}

/// Decode one X96 subframe (residual subband samples of bands 0..64) into `buf`
/// rows `ch`, samples `t0 .. t0 + nssc * 8`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_subframe(
    br: &mut BitReader<'_>,
    h: &X96Header,
    side: &mut X96Side,
    nssc: usize,
    aspf: bool,
    cpf: bool,
    lossless: bool,
    buf: &mut SubBuf,
    t0: usize,
) -> Result<(), DecodeError> {
    let nch = h.nch;
    let nsamp = nssc * 8;
    let maxabits = if h.highres { 15 } else { 7 };
    for ch in 0..nch {
        for n in 0..64 {
            side.pmode[ch][n] = n >= h.sbs && n < h.sbe[ch] && br.bit();
        }
    }
    for ch in 0..nch {
        for n in h.sbs..h.sbe[ch] {
            if side.pmode[ch][n] {
                side.pvq[ch][n] = br.read(12) as u16;
            }
        }
    }
    for ch in 0..nch {
        side.abits[ch] = [0; 64];
        let sel = h.bhuff[ch] as usize;
        let mut sum = 0i32;
        for n in h.sbs..h.sbe[ch] {
            let v = if sel < 7 {
                let book = if h.highres {
                    books::ABITS96_BOOKS_HI[sel]
                } else {
                    books::ABITS96_BOOKS_LO[sel]
                };
                sum += br.huff(book);
                sum
            } else if h.highres {
                br.read(4) as i32
            } else {
                br.read(3) as i32
            };
            if v < 0 || v > maxabits {
                return Err(DecodeError::InvalidData("X96 ABITS96"));
            }
            side.abits[ch][n] = v as u8;
        }
    }
    for ch in 0..nch {
        side.scale[ch] = [0.0; 64];
        let shuff = h.shuff[ch];
        let mut sum = 0i32;
        for n in h.sbs..h.sbe[ch] {
            if shuff < 5 {
                sum += br.huff(books::SCALE_BOOKS[shuff as usize]);
            } else {
                sum = br.read(6) as i32;
            }
            if !(0..63).contains(&sum) {
                return Err(DecodeError::InvalidData("X96 scale factor index"));
            }
            side.scale[ch][n] = SCALE_6BIT[sum as usize] as f32;
        }
    }
    let mut join_shuff = [0u8; MAX_CH];
    for ch in 0..nch {
        if h.joinx[ch] > 0 {
            join_shuff[ch] = br.read(3) as u8;
        }
    }
    for ch in 0..nch {
        if h.joinx[ch] > 0 {
            let src = h.joinx[ch] as usize - 1;
            let js = join_shuff[ch];
            for n in h.sbe[ch]..h.sbe[src] {
                let v = if js < 5 {
                    br.huff(books::SCALE_BOOKS[js as usize])
                } else {
                    br.read(6) as i32
                } + 64;
                if !(0..=128).contains(&v) {
                    return Err(DecodeError::InvalidData("X96 joint scale index"));
                }
                side.jscale[ch][n] = JOINT_SCALE[v as usize];
            }
        }
    }
    if cpf {
        let _sicrc96 = br.read(16);
    }
    if br.overrun() {
        return Err(DecodeError::InvalidData(
            "X96 side information past frame end",
        ));
    }

    // Clear the subframe span of every band.
    for ch in 0..nch {
        for b in 0..64 {
            let row = buf.row_mut(ch, b);
            row[HIST + t0..HIST + t0 + nsamp]
                .iter_mut()
                .for_each(|v| *v = 0.0);
        }
    }
    // High-frequency VQ (ABITS96 = 1): one 10-bit index per 16 samples; elements 0..16.
    for ch in 0..nch {
        for n in h.sbs..h.sbe[ch] {
            if side.abits[ch][n] == 1 {
                let s = side.scale[ch][n] * (1.0 / 16.0);
                let row = buf.row_mut(ch, n);
                let mut m = 0;
                while m < nsamp {
                    let idx = br.read(10) as usize;
                    let cnt = (nsamp - m).min(16);
                    for k in 0..cnt {
                        row[HIST + t0 + m + k] = sat24(s * f32::from(HF_VQ[idx][k]));
                    }
                    m += 16;
                }
            }
        }
    }
    let steps: &[u32; 27] = if lossless {
        &STEP_LOSSLESS
    } else {
        &STEP_LOSSY
    };
    for ssf in 0..nssc {
        for ch in 0..nch {
            for n in h.sbs..h.sbe[ch] {
                let a96 = u32::from(side.abits[ch][n]);
                if a96 < 2 {
                    continue;
                }
                let a = a96 - 1; // core quantizer group
                let mut q = [0i32; 8];
                let sel = if a96 <= 11 {
                    u32::from(h.sel[ch][a96 as usize - 2])
                } else {
                    0
                };
                let nh = books::huffman_sel_count(a);
                if a <= 10 && sel < nh {
                    let book = books::sample_book(a, sel)
                        .ok_or(DecodeError::InvalidData("X96 codebook"))?;
                    for v in &mut q {
                        *v = br.huff(book);
                    }
                } else if a <= 7 {
                    let (levels, bits) = books::block_code(a);
                    let off = (levels - 1) / 2;
                    for blk in 0..2 {
                        let mut code = br.read(bits) as i32;
                        for j in 0..4 {
                            q[blk * 4 + j] = code % levels - off;
                            code /= levels;
                        }
                        if code != 0 {
                            return Err(DecodeError::InvalidData("X96 block code"));
                        }
                    }
                } else {
                    let bits = a - 3;
                    for v in &mut q {
                        *v = br.read_signed(bits);
                    }
                }
                let scale = steps[a as usize] as f32 * (1.0 / 4_194_304.0) * side.scale[ch][n];
                let row = buf.row_mut(ch, n);
                let base = HIST + t0 + ssf * 8;
                for m in 0..8 {
                    row[base + m] = sat24(q[m] as f32 * scale);
                }
            }
        }
        if (ssf == nssc - 1 || aspf) && br.read(16) != 0xFFFF {
            return Err(DecodeError::InvalidData("X96 DSYNC"));
        }
    }
    if br.overrun() {
        return Err(DecodeError::InvalidData("X96 audio data past frame end"));
    }
    // Inverse ADPCM over the subframe.
    for ch in 0..nch {
        for n in h.sbs..h.sbe[ch] {
            if side.pmode[ch][n] {
                let c = &ADPCM_VQ[side.pvq[ch][n] as usize];
                let c = c.map(|v| f32::from(v) * (1.0 / 8192.0));
                let row = buf.row_mut(ch, n);
                for m in HIST + t0..HIST + t0 + nsamp {
                    let p = c[0] * row[m - 1]
                        + c[1] * row[m - 2]
                        + c[2] * row[m - 3]
                        + c[3] * row[m - 4];
                    row[m] = sat24(row[m] + p);
                }
            }
        }
    }
    // Joint intensity.
    for ch in 0..nch {
        if h.joinx[ch] > 0 {
            let src = h.joinx[ch] as usize - 1;
            for n in h.sbe[ch]..h.sbe[src] {
                let js = side.jscale[ch][n];
                let mut tmp = [0f32; 32];
                {
                    let srow = buf.row(src, n);
                    for m in 0..nsamp {
                        tmp[m] = sat24(srow[HIST + t0 + m] * js);
                    }
                }
                let row = buf.row_mut(ch, n);
                row[HIST + t0..HIST + t0 + nsamp].copy_from_slice(&tmp[..nsamp]);
            }
        }
    }
    Ok(())
}
