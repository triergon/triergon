//! Subframe side information and audio data arrays shared by the core and XCh channel
//! sets (ETSI TS 102 114 clauses 5.5, 5.6, 6.4.4; inverse ADPCM C.3.3; joint intensity
//! C.3.4; block codes C.3.2).

use crate::DecodeError;
use crate::bits::BitReader;
use crate::books;
use crate::header::{MAX_SET_CH, SetHeader};
use crate::tables::{JOINT_SCALE, SCALE_6BIT, SCALE_7BIT, STEP_LOSSLESS, STEP_LOSSY};
use crate::vq::{ADPCM_VQ, HF_VQ};

/// ADPCM history length (NumADPCMCoeff).
pub(crate) const HIST: usize = 4;
/// Maximum subband samples per frame (NBLKS + 1 <= 128).
pub(crate) const MAX_T: usize = 128;

/// Frame-long subband sample storage: `slots x bands` rows of `stride` floats. The first
/// `HIST` floats of a row hold the ADPCM history (the previous frame's last samples).
/// Rows are sized for the longest frame and the most slots seen so far (most streams use
/// 16 samples per band, a small fraction of the 128 the syntax allows).
pub(crate) struct SubBuf {
    data: Vec<f32>,
    bands: usize,
    stride: usize,
    /// Last `HIST` reconstructed samples of every row, saved before synthesis-side
    /// in-place processing (sum/difference, down-mix removal, X96 recombination).
    saved: Vec<f32>,
}

impl SubBuf {
    pub(crate) fn new(slots: usize, bands: usize) -> Self {
        Self {
            data: Vec::new(),
            bands,
            stride: HIST,
            saved: vec![0.0; slots * bands * HIST],
        }
    }

    /// Room for `slots` slots of frames with `samples` subband samples. A longer frame
    /// changes the row layout, which is only valid before any row of the frame is
    /// written (the first `start_frame` of a frame); more slots only append rows.
    fn reserve(&mut self, slots: usize, samples: usize) {
        let stride = self.stride.max(HIST + samples.min(MAX_T));
        let len = (slots * self.bands * stride).max(self.data.len() / self.stride * stride);
        if stride != self.stride || len > self.data.len() {
            self.stride = stride;
            self.data.resize(len, 0.0);
        }
    }

    #[inline]
    pub(crate) fn row_mut(&mut self, slot: usize, band: usize) -> &mut [f32] {
        let o = (slot * self.bands + band) * self.stride;
        &mut self.data[o..o + self.stride]
    }

    #[inline]
    pub(crate) fn row(&self, slot: usize, band: usize) -> &[f32] {
        let o = (slot * self.bands + band) * self.stride;
        &self.data[o..o + self.stride]
    }

    /// Start a frame: load the saved history into the rows, or clear it.
    pub(crate) fn start_frame(
        &mut self,
        slots: core::ops::Range<usize>,
        samples: usize,
        keep: bool,
    ) {
        self.reserve(slots.end, samples);
        for s in slots {
            for b in 0..self.bands {
                let r = s * self.bands + b;
                let o = r * self.stride;
                if keep {
                    self.data[o..o + HIST].copy_from_slice(&self.saved[r * HIST..r * HIST + HIST]);
                } else {
                    self.data[o..o + HIST].iter_mut().for_each(|v| *v = 0.0);
                }
            }
        }
    }

    /// Save the last `HIST` samples of a frame of `len` samples as the next history.
    pub(crate) fn save_history(&mut self, slots: core::ops::Range<usize>, len: usize) {
        for s in slots {
            for b in 0..self.bands {
                let r = s * self.bands + b;
                let o = r * self.stride + len;
                self.saved[r * HIST..r * HIST + HIST].copy_from_slice(&self.data[o..o + HIST]);
            }
        }
    }
}

/// Saturate a reconstructed subband sample to the 24-bit range.
///
/// Not specified by ETSI TS 102 114; FFmpeg (the reference) saturates dequantized and
/// ADPCM-reconstructed subband samples at [-2^23, 2^23 - 1] (observed with the
/// `scalehigh` and `adpcm_ovf` synthetic streams).
#[inline]
pub(crate) fn sat24(v: f32) -> f32 {
    v.clamp(-8_388_608.0, 8_388_607.0)
}

/// Per-subframe side information of a channel set.
pub(crate) struct SideInfo {
    pub pmode: [[bool; 32]; MAX_SET_CH],
    pub pvq: [[u16; 32]; MAX_SET_CH],
    pub abits: [[u8; 32]; MAX_SET_CH],
    pub tmode: [[u8; 32]; MAX_SET_CH],
    pub scale: [[[f32; 2]; 32]; MAX_SET_CH],
    pub jscale: [[f32; 32]; MAX_SET_CH],
}

impl SideInfo {
    pub(crate) fn new() -> Self {
        Self {
            pmode: [[false; 32]; MAX_SET_CH],
            pvq: [[0; 32]; MAX_SET_CH],
            abits: [[0; 32]; MAX_SET_CH],
            tmode: [[0; 32]; MAX_SET_CH],
            scale: [[[0.0; 2]; 32]; MAX_SET_CH],
            jscale: [[0.0; 32]; MAX_SET_CH],
        }
    }
}

/// Per-frame parameters that apply to every subframe of a channel set.
pub(crate) struct SubframeParams {
    pub nssc: usize,
    pub aspf: bool,
    pub cpf: bool,
    pub lossless: bool,
    /// Core only: DYNF present (RANGE read after the joint scale factors).
    pub dynf: bool,
    /// Core only: LFF (decimated LFE samples read after the high-frequency VQ).
    pub lff: u8,
}

#[inline]
fn read_scale_index(br: &mut BitReader<'_>, shuff: u8, sum: &mut i32) -> i32 {
    match shuff {
        0..=4 => {
            *sum += br.huff(books::SCALE_BOOKS[shuff as usize]);
        }
        5 => *sum = br.read(6) as i32,
        _ => *sum = br.read(7) as i32,
    }
    *sum
}

#[inline]
fn scale_value(shuff: u8, idx: i32) -> Result<f32, DecodeError> {
    let v = if shuff == 6 {
        if !(0..125).contains(&idx) {
            return Err(DecodeError::InvalidData("7-bit scale factor index"));
        }
        SCALE_7BIT[idx as usize]
    } else {
        if !(0..63).contains(&idx) {
            return Err(DecodeError::InvalidData("6-bit scale factor index"));
        }
        SCALE_6BIT[idx as usize]
    };
    Ok(v as f32)
}

/// Decode one subframe of a channel set into `buf` rows `slot0 + ch`, samples
/// `t0 .. t0 + nssc * 8` (relative to the row's sample area).
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_subframe(
    br: &mut BitReader<'_>,
    hdr: &SetHeader,
    side: &mut SideInfo,
    p: &SubframeParams,
    buf: &mut SubBuf,
    slot0: usize,
    t0: usize,
    lfe: Option<&mut [f32]>,
    nsubs_all: &[u8; 8],
) -> Result<(), DecodeError> {
    let nch = hdr.nch;
    // JOINX counts channels globally (core channels first, then extension channels);
    // the source must precede the joint channel (Table 5-22).
    for ch in 0..nch {
        let j = hdr.joinx[ch] as usize;
        if j > slot0 + ch {
            return Err(DecodeError::InvalidData("JOINX source channel"));
        }
    }
    let nssc = p.nssc;
    let nsamp = nssc * 8;

    // --- side information (Table 5-28) ---
    for ch in 0..nch {
        for n in 0..32 {
            side.pmode[ch][n] = n < hdr.nsubs[ch] as usize && br.bit();
        }
    }
    for ch in 0..nch {
        for n in 0..hdr.nsubs[ch] as usize {
            if side.pmode[ch][n] {
                side.pvq[ch][n] = br.read(12) as u16;
            }
        }
    }
    for ch in 0..nch {
        let sel = hdr.bhuff[ch];
        let nvq = (hdr.nvqsub[ch] as usize).min(32);
        for n in 0..32 {
            side.abits[ch][n] = 0;
        }
        for n in 0..nvq {
            let a = match sel {
                0..=4 => br.huff(books::ABITS_BOOKS[sel as usize]) as u32,
                5 => br.read(4),
                _ => br.read(5),
            };
            if a > 26 {
                return Err(DecodeError::InvalidData("ABITS above 26"));
            }
            side.abits[ch][n] = a as u8;
        }
    }
    for ch in 0..nch {
        side.tmode[ch] = [0; 32];
    }
    if nssc > 1 {
        for ch in 0..nch {
            let book = books::TMODE_BOOKS[hdr.thuff[ch] as usize];
            let nvq = (hdr.nvqsub[ch] as usize).min(32);
            for n in 0..nvq {
                if side.abits[ch][n] > 0 {
                    side.tmode[ch][n] = br.huff(book) as u8;
                }
            }
        }
    }
    for ch in 0..nch {
        let shuff = hdr.shuff[ch];
        side.scale[ch] = [[0.0; 2]; 32];
        let mut sum = 0i32;
        let nvq = (hdr.nvqsub[ch] as usize).min(32);
        for n in 0..nvq {
            if side.abits[ch][n] > 0 {
                let i = read_scale_index(br, shuff, &mut sum);
                side.scale[ch][n][0] = scale_value(shuff, i)?;
                if side.tmode[ch][n] > 0 {
                    let i = read_scale_index(br, shuff, &mut sum);
                    side.scale[ch][n][1] = scale_value(shuff, i)?;
                }
            }
        }
        for n in nvq..hdr.nsubs[ch] as usize {
            let i = read_scale_index(br, shuff, &mut sum);
            side.scale[ch][n][0] = scale_value(shuff, i)?;
        }
    }
    let mut join_shuff = [0u8; MAX_SET_CH];
    for ch in 0..nch {
        if hdr.joinx[ch] > 0 {
            join_shuff[ch] = br.read(3) as u8;
            if join_shuff[ch] == 7 {
                return Err(DecodeError::InvalidData("JOIN_SHUFF = 7"));
            }
        }
    }
    for ch in 0..nch {
        if hdr.joinx[ch] > 0 {
            let src = hdr.joinx[ch] as usize - 1;
            for n in hdr.nsubs[ch] as usize..nsubs_all[src] as usize {
                let mut s = 0;
                let v = read_scale_index(br, join_shuff[ch], &mut s) + 64;
                if !(0..=128).contains(&v) {
                    return Err(DecodeError::InvalidData("joint scale index"));
                }
                side.jscale[ch][n] = JOINT_SCALE[v as usize];
            }
        }
    }
    // RANGE (dynamic range coefficient): parsed, not applied — FFmpeg, the reference,
    // does not apply it (synthetic `dynf` stream).
    if p.dynf {
        let _range = br.read(8);
    }
    if p.cpf {
        let _sicrc = br.read(16);
    }
    if br.overrun() {
        return Err(DecodeError::InvalidData("side information past frame end"));
    }

    // --- audio data arrays (Table 5-29) ---
    // Clear this subframe's span for every band so that inactive subbands are zero.
    for ch in 0..nch {
        for b in 0..32 {
            let row = buf.row_mut(slot0 + ch, b);
            row[HIST + t0..HIST + t0 + nsamp]
                .iter_mut()
                .for_each(|v| *v = 0.0);
        }
    }
    for ch in 0..nch {
        let nvq = (hdr.nvqsub[ch] as usize).min(32);
        for n in nvq..hdr.nsubs[ch] as usize {
            let idx = br.read(10) as usize;
            let s = side.scale[ch][n][0] * (1.0 / 16.0);
            let row = buf.row_mut(slot0 + ch, n);
            let v = &HF_VQ[idx];
            for m in 0..nsamp.min(32) {
                row[HIST + t0 + m] = sat24(s * f32::from(v[m]));
            }
        }
    }
    if let Some(lfe) = lfe {
        let count = 2 * p.lff as usize * nssc;
        let mut q = [0i32; 64];
        for v in q.iter_mut().take(count) {
            *v = br.read_signed(8);
        }
        let si = br.read(8) as usize;
        if si >= 125 {
            return Err(DecodeError::InvalidData("LFE scale index"));
        }
        let s = SCALE_7BIT[si] as f32 * 0.035;
        for k in 0..count {
            // Decimated LFE samples saturate to the 24-bit PCM range (observed FFmpeg
            // behaviour).
            lfe[k] = (q[k] as f32 * s).clamp(-8_388_608.0, 8_388_607.0);
        }
    }
    let steps: &[u32; 27] = if p.lossless {
        &STEP_LOSSLESS
    } else {
        &STEP_LOSSY
    };
    for ssf in 0..nssc {
        for ch in 0..nch {
            let nvq = (hdr.nvqsub[ch] as usize).min(32);
            for n in 0..nvq {
                let abits = u32::from(side.abits[ch][n]);
                let mut q = [0i32; 8];
                let mut adj = 1.0f32;
                if abits > 0 {
                    let sel = if abits <= 10 {
                        u32::from(hdr.sel[ch][abits as usize - 1])
                    } else {
                        0
                    };
                    let nh = books::huffman_sel_count(abits);
                    if abits <= 10 && sel < nh {
                        let book = books::sample_book(abits, sel)
                            .ok_or(DecodeError::InvalidData("sample codebook"))?;
                        for v in &mut q {
                            *v = br.huff(book);
                        }
                        adj = hdr.adj[ch][abits as usize - 1];
                    } else if abits <= 7 {
                        let (levels, bits) = books::block_code(abits);
                        let off = (levels - 1) / 2;
                        for blk in 0..2 {
                            let mut code = br.read(bits) as i32;
                            for j in 0..4 {
                                q[blk * 4 + j] = code % levels - off;
                                code /= levels;
                            }
                            if code != 0 {
                                return Err(DecodeError::InvalidData("block code"));
                            }
                        }
                    } else {
                        let bits = abits - 3;
                        for v in &mut q {
                            *v = br.read_signed(bits);
                        }
                    }
                }
                let tm = side.tmode[ch][n] as usize;
                let tm = if tm == 0 { nssc } else { tm };
                let sf = if ssf < tm {
                    side.scale[ch][n][0]
                } else {
                    side.scale[ch][n][1]
                };
                let scale = steps[abits as usize] as f32 * (1.0 / 4_194_304.0) * sf * adj;
                let row = buf.row_mut(slot0 + ch, n);
                let base = HIST + t0 + ssf * 8;
                for m in 0..8 {
                    row[base + m] = sat24(q[m] as f32 * scale);
                }
                if side.pmode[ch][n] {
                    let c = &ADPCM_VQ[side.pvq[ch][n] as usize];
                    let c = [
                        f32::from(c[0]) * (1.0 / 8192.0),
                        f32::from(c[1]) * (1.0 / 8192.0),
                        f32::from(c[2]) * (1.0 / 8192.0),
                        f32::from(c[3]) * (1.0 / 8192.0),
                    ];
                    // History lives immediately before `base` (previous subsubframe,
                    // subframe or, through the HIST area, the previous frame).
                    for m in base..base + 8 {
                        let p = c[0] * row[m - 1]
                            + c[1] * row[m - 2]
                            + c[2] * row[m - 3]
                            + c[3] * row[m - 4];
                        row[m] = sat24(row[m] + p);
                    }
                }
            }
        }
        if (ssf == nssc - 1 || p.aspf) && br.read(16) != 0xFFFF {
            return Err(DecodeError::InvalidData("DSYNC"));
        }
    }
    if br.overrun() {
        return Err(DecodeError::InvalidData("audio data past frame end"));
    }
    // Joint intensity (C.3.4): copy scaled source subbands.
    for ch in 0..nch {
        if hdr.joinx[ch] > 0 {
            let src = hdr.joinx[ch] as usize - 1;
            for n in hdr.nsubs[ch] as usize..nsubs_all[src] as usize {
                let js = side.jscale[ch][n];
                let mut tmp = [0f32; 32];
                {
                    let srow = buf.row(src, n);
                    for m in 0..nsamp {
                        tmp[m] = sat24(srow[HIST + t0 + m] * js);
                    }
                }
                let row = buf.row_mut(slot0 + ch, n);
                row[HIST + t0..HIST + t0 + nsamp].copy_from_slice(&tmp[..nsamp]);
            }
        }
    }
    Ok(())
}
