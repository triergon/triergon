//! Band shapes (RFC 6716 section 4.3.4): the per-band bit budget, recursive splits with
//! their angle, stereo coupling, time-frequency changes, spectral folding and the
//! anti-collapse noise.

use super::alloc::{
    Allocation, BITRES, bits_to_pulses, pulse_count, pulses_to_bits, split_threshold,
};
use super::pvq::{self, MAX_BAND, Spread};
use super::tables::{BANDS, EBANDS, LOG_N};
use crate::range::{RangeDecoder, ilog};
use core::ops::Range;

/// Normalised folding source of both channels: bins up to the last band's start.
pub(super) const NORM_LEN: usize = 8 * 100;

use core::f32::consts::FRAC_1_SQRT_2 as FRAC_SQRT_2;

/// The reference's linear congruential generator.
#[inline]
pub(super) fn lcg(seed: u32) -> u32 {
    seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223)
}

/// `(a * b + 2^14) >> 15` on 16-bit operands, as the bit-exact approximations use it.
#[inline]
fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + i32::from(a as i16) * i32::from(b as i16)) >> 15
}

/// A cosine approximation that is bit-exact everywhere, since it drives allocation.
/// `x` is an angle in [0, 16384] for [0, pi/2]; the result is Q15.
fn bitexact_cos(x: i32) -> i32 {
    let x2 = (4096 + x * x) >> 13;
    let y = (32767 - x2) + frac_mul16(x2, -7651 + frac_mul16(x2, 8277 + frac_mul16(-626, x2)));
    1 + y
}

/// `2048 * log2(sin / cos)`, bit-exact.
fn bitexact_log2tan(sin: i32, cos: i32) -> i32 {
    let lc = ilog(cos as u32) as i32;
    let ls = ilog(sin as u32) as i32;
    let cos = cos << (15 - lc);
    let sin = sin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(sin, frac_mul16(sin, -2597) + 7932)
        - frac_mul16(cos, frac_mul16(cos, -2597) + 7932)
}

/// One level of the Haar transform over `stride` interleaved signals.
fn haar(x: &mut [f32], n0: usize, stride: usize) {
    for pair in x[..n0 * stride].chunks_exact_mut(2 * stride) {
        let (a, b) = pair.split_at_mut(stride);
        for (a, b) in a.iter_mut().zip(b) {
            let (t1, t2) = (FRAC_SQRT_2 * *a, FRAC_SQRT_2 * *b);
            *a = t1 + t2;
            *b = t1 - t2;
        }
    }
}

/// Natural to sequency ("ordery") Hadamard order for 2, 4, 8 and 16 blocks, reversed
/// so that the DC block comes last.
const ORDERY: [usize; 30] = [
    1, 0, //
    3, 0, 2, 1, //
    7, 0, 4, 3, 6, 1, 5, 2, //
    15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

fn ordery(stride: usize) -> &'static [usize] {
    &ORDERY[stride - 2..2 * stride - 2]
}

/// Gather `stride` interleaved blocks of `n0` bins into consecutive blocks.
fn deinterleave(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let mut tmp = [0f32; MAX_BAND];
    let n = n0 * stride;
    for i in 0..stride {
        let to = if hadamard { ordery(stride)[i] } else { i } * n0;
        for j in 0..n0 {
            tmp[to + j] = x[j * stride + i];
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

/// The inverse of [`deinterleave`].
fn interleave(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let mut tmp = [0f32; MAX_BAND];
    let n = n0 * stride;
    for i in 0..stride {
        let from = if hadamard { ordery(stride)[i] } else { i } * n0;
        for j in 0..n0 {
            tmp[j * stride + i] = x[from + j];
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

/// Collapse masks when blocks are recombined (two bits per merged block pair) and the
/// inverse.
const BIT_INTERLEAVE: [u8; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];
const BIT_DEINTERLEAVE: [u8; 16] = [
    0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3, 0xFC, 0xFF,
];

/// The split of a band into two halves (mid/side or first/second half).
struct Split {
    /// Angle in [0, 16384] for [0, pi/2].
    itheta: i32,
    mid: f32,
    side: f32,
    /// Bits moved from mid to side.
    delta: i32,
    /// Bits the angle used.
    used: i32,
    /// Intensity stereo with the side inverted.
    invert: bool,
}

/// Frame-wide parameters of band decoding.
pub(super) struct BandParams<'a> {
    pub(super) bands: Range<usize>,
    pub(super) allocation: &'a Allocation,
    pub(super) tf_change: &'a [i32; BANDS],
    pub(super) spread: Spread,
    /// Short blocks of a transient frame, 1 otherwise.
    pub(super) blocks: usize,
    pub(super) lm: usize,
    /// Bits the bands may use in total (1/8 bit).
    pub(super) total_bits: i32,
}

/// Decoding state while walking the bands of one frame.
struct Bands<'a, 'r> {
    rc: &'a mut RangeDecoder<'r>,
    spread: Spread,
    intensity: usize,
    band: usize,
    tf_change: i32,
    remaining_bits: i32,
    seed: u32,
}

/// Decode the normalised shapes of all bands into `x` (and `y` for a stereo frame), with
/// the collapse masks of every band and channel (`quant_all_bands`). `norm` is scratch;
/// `seed` is the folding noise state.
pub(super) fn decode(
    rc: &mut RangeDecoder<'_>,
    params: &BandParams<'_>,
    x: &mut [f32],
    mut y: Option<&mut [f32]>,
    collapse: &mut [[u8; 2]; BANDS],
    norm: &mut [f32; 2 * NORM_LEN],
    seed: &mut u32,
) {
    let BandParams {
        ref bands,
        allocation,
        tf_change,
        spread,
        blocks,
        lm,
        total_bits,
    } = *params;
    let (start, end) = (bands.start, bands.end);
    let m = 1 << lm;
    let (norm_x, norm_y) = norm.split_at_mut(NORM_LEN);
    let mut lowband_scratch = [0f32; MAX_BAND];
    let mut state = Bands {
        rc,
        spread,
        intensity: allocation.intensity,
        band: start,
        tf_change: 0,
        remaining_bits: 0,
        seed: *seed,
    };
    let stereo = y.is_some();
    let mut dual_stereo = allocation.dual_stereo;
    let mut balance = allocation.balance;
    let mut lowband_offset = 0;
    let mut update_lowband = true;
    for i in start..end {
        let lo = m * EBANDS[i];
        let n = m * EBANDS[i + 1] - lo;
        let tell = state.rc.tell_frac();
        // The bits this band may use.
        if i != start {
            balance -= tell;
        }
        state.remaining_bits = total_bits - tell - 1;
        state.band = i;
        let b = if i < allocation.coded_bands {
            let share = balance / (allocation.coded_bands - i).min(3) as i32;
            (state.remaining_bits + 1)
                .min(allocation.pulses[i] + share)
                .clamp(0, 16383)
        } else {
            0
        };

        // The folding source: the bins just below this band, or for the second band of a
        // hybrid frame a copy that repeats the first band (RFC 8251 section 9).
        if (lo >= n + m * EBANDS[start] || i == start + 1)
            && (update_lowband || lowband_offset == 0)
        {
            lowband_offset = i;
        }
        if i == start + 1 {
            let n1 = m * (EBANDS[start + 1] - EBANDS[start]);
            let n2 = m * (EBANDS[start + 2] - EBANDS[start + 1]);
            let offset = m * EBANDS[start];
            if n2 > n1 {
                let from = offset + 2 * n1 - n2;
                norm_x.copy_within(from..from + n2 - n1, offset + n1);
                if stereo {
                    norm_y.copy_within(from..from + n2 - n1, offset + n1);
                }
            }
        }
        state.tf_change = tf_change[i];

        // Conservative collapse masks of the bands folded from.
        let mut fold_from = None;
        let (mut x_cm, mut y_cm);
        if lowband_offset != 0
            && (spread != Spread::Aggressive || blocks > 1 || state.tf_change < 0)
        {
            let effective = (m * EBANDS[lowband_offset])
                .saturating_sub(n)
                .max(m * EBANDS[start]);
            let mut fold_start = lowband_offset;
            loop {
                fold_start -= 1;
                if m * EBANDS[fold_start] <= effective {
                    break;
                }
            }
            let mut fold_end = lowband_offset;
            while fold_end < i && m * EBANDS[fold_end] < effective + n {
                fold_end += 1;
            }
            x_cm = 0;
            y_cm = 0;
            for masks in &collapse[fold_start..fold_end.max(fold_start + 1)] {
                x_cm |= u32::from(masks[0]);
                y_cm |= u32::from(masks[1]);
            }
            fold_from = Some(effective);
        } else {
            x_cm = (1 << blocks) - 1;
            y_cm = x_cm;
        }

        if dual_stereo && i == state.intensity {
            // Intensity stereo starts here: fold from the average of both channels.
            dual_stereo = false;
            for (a, b) in norm_x[m * EBANDS[start]..lo]
                .iter_mut()
                .zip(&norm_y[m * EBANDS[start]..lo])
            {
                *a = 0.5 * (*a + *b);
            }
        }

        let xb = &mut x[lo..lo + n];
        let out = lo..lo + n;
        match y.as_deref_mut() {
            Some(y) if dual_stereo => {
                let lowband = fold_source(&mut lowband_scratch, norm_x, fold_from, n);
                let out_x = Some(&mut norm_x[out.clone()]);
                x_cm = state.band(xb, b / 2, blocks, lowband, lm as i32, out_x, 1.0, x_cm);
                let lowband = fold_source(&mut lowband_scratch, norm_y, fold_from, n);
                let out_y = Some(&mut norm_y[out]);
                y_cm = state.band(
                    &mut y[lo..lo + n],
                    b / 2,
                    blocks,
                    lowband,
                    lm as i32,
                    out_y,
                    1.0,
                    y_cm,
                );
            }
            Some(y) => {
                let lowband = fold_source(&mut lowband_scratch, norm_x, fold_from, n);
                let yb = &mut y[lo..lo + n];
                x_cm = state.stereo(
                    xb,
                    yb,
                    b,
                    blocks,
                    lowband,
                    lm as i32,
                    &mut norm_x[out],
                    x_cm | y_cm,
                );
                y_cm = x_cm;
            }
            None => {
                let lowband = fold_source(&mut lowband_scratch, norm_x, fold_from, n);
                let out_x = Some(&mut norm_x[out]);
                x_cm = state.band(xb, b, blocks, lowband, lm as i32, out_x, 1.0, x_cm);
                y_cm = x_cm;
            }
        }
        collapse[i] = [x_cm as u8, y_cm as u8];
        balance += allocation.pulses[i] + tell;
        // Move the folding source only while bands have at least one bit per bin.
        update_lowband = b > (n << BITRES) as i32;
    }
    *seed = state.seed;
}

/// A private copy of the `n` folding source bins at `from`, which band decoding may
/// transform in place.
fn fold_source<'s>(
    scratch: &'s mut [f32; MAX_BAND],
    norm: &[f32],
    from: Option<usize>,
    n: usize,
) -> Option<&'s mut [f32]> {
    let from = from?;
    scratch[..n].copy_from_slice(&norm[from..from + n]);
    Some(&mut scratch[..n])
}

impl Bands<'_, '_> {
    /// A band of one bin: just signs (`quant_band_n1`).
    fn single(
        &mut self,
        x: &mut [f32],
        y: Option<&mut [f32]>,
        lowband_out: Option<&mut [f32]>,
    ) -> u32 {
        for channel in core::iter::once(&mut *x).chain(y) {
            let mut negative = false;
            if self.remaining_bits >= 1 << BITRES {
                negative = self.rc.bits(1) != 0;
                self.remaining_bits -= 1 << BITRES;
            }
            channel[0] = if negative { -1.0 } else { 1.0 };
        }
        if let Some(out) = lowband_out {
            out[0] = x[0];
        }
        1
    }

    /// Decode the split angle of a band of `n` bins per half (`compute_theta`).
    #[allow(clippy::too_many_arguments)] // The split state of the reference.
    fn theta(
        &mut self,
        n: usize,
        b: i32,
        blocks: usize,
        blocks0: usize,
        lm: i32,
        stereo: bool,
        fill: &mut u32,
    ) -> Split {
        let pulse_cap = LOG_N[self.band] + lm * (1 << BITRES);
        let offset = (pulse_cap >> 1) - if stereo && n == 2 { 16 } else { 4 };
        let mut qn = quantisation_steps(n, b, offset, pulse_cap, stereo);
        if stereo && self.band >= self.intensity {
            qn = 1;
        }
        let tell = self.rc.tell_frac();
        let mut itheta = 0;
        let mut invert = false;
        if qn != 1 {
            itheta = if stereo && n > 2 {
                self.step_angle(qn)
            } else if blocks0 > 1 || stereo {
                self.rc.uint(qn + 1)
            } else {
                self.triangular_angle(qn)
            } as i32;
            itheta = itheta * 16384 / qn as i32;
        } else if stereo {
            invert = b > 2 << BITRES && self.remaining_bits > 2 << BITRES && self.rc.bit_logp(2);
        }
        let used = self.rc.tell_frac() - tell;
        let mask = (1u32 << blocks) - 1;
        let (imid, iside, delta) = match itheta {
            0 => {
                *fill &= mask;
                (32767, 0, -16384)
            }
            16384 => {
                *fill &= mask << blocks;
                (0, 32767, 16384)
            }
            _ => {
                let imid = bitexact_cos(itheta);
                let iside = bitexact_cos(16384 - itheta);
                // The mid/side split that minimises the squared error.
                let delta = frac_mul16((n as i32 - 1) << 7, bitexact_log2tan(iside, imid));
                (imid, iside, delta)
            }
        };
        Split {
            itheta,
            mid: (1.0 / 32768.0) * imid as f32,
            side: (1.0 / 32768.0) * iside as f32,
            delta,
            used,
            invert,
        }
    }

    /// A stereo angle: probability 3 up to the half and 1 above.
    fn step_angle(&mut self, qn: u32) -> u32 {
        let x0 = qn / 2;
        let ft = 3 * (x0 + 1) + x0;
        let (fs, ext) = self.rc.decode(ft);
        let x = if fs < (x0 + 1) * 3 {
            fs / 3
        } else {
            x0 + 1 + (fs - (x0 + 1) * 3)
        };
        let (fl, fh) = if x <= x0 {
            (3 * x, 3 * (x + 1))
        } else {
            (x - 1 - x0 + (x0 + 1) * 3, x - x0 + (x0 + 1) * 3)
        };
        self.rc.update(fl, fh, ft, ext);
        x
    }

    /// A split angle with a triangular distribution peaking at the middle.
    fn triangular_angle(&mut self, qn: u32) -> u32 {
        let half = qn >> 1;
        let ft = (half + 1) * (half + 1);
        let (fm, ext) = self.rc.decode(ft);
        let (itheta, fl, fs);
        if fm < (half * (half + 1)) >> 1 {
            itheta = ((8 * fm + 1).isqrt() - 1) >> 1;
            fs = itheta + 1;
            fl = (itheta * (itheta + 1)) >> 1;
        } else {
            itheta = (2 * (qn + 1) - (8 * (ft - fm - 1) + 1).isqrt()) >> 1;
            fs = qn + 1 - itheta;
            fl = ft - (((qn + 1 - itheta) * (qn + 2 - itheta)) >> 1);
        }
        self.rc.update(fl, fl + fs, ft, ext);
        itheta
    }

    /// A mono band with its time-frequency change (`quant_band`): `lowband` is a private
    /// copy of the folding source, transformed alongside.
    #[allow(clippy::too_many_arguments)] // The band state of the reference.
    fn band(
        &mut self,
        x: &mut [f32],
        b: i32,
        blocks: usize,
        mut lowband: Option<&mut [f32]>,
        lm: i32,
        lowband_out: Option<&mut [f32]>,
        gain: f32,
        mut fill: u32,
    ) -> u32 {
        let n0 = x.len();
        if n0 == 1 {
            return self.single(x, None, lowband_out);
        }
        let long_blocks = blocks == 1;
        let mut tf_change = self.tf_change;
        let recombine = tf_change.max(0) as usize;
        let mut b_count = blocks;
        let mut n_b = n0 / blocks;

        // Recombine blocks to increase frequency resolution.
        for k in 0..recombine {
            if let Some(lowband) = lowband.as_deref_mut() {
                haar(lowband, n0 >> k, 1 << k);
            }
            fill = u32::from(BIT_INTERLEAVE[(fill & 0xf) as usize])
                | u32::from(BIT_INTERLEAVE[(fill >> 4 & 0xf) as usize]) << 2;
        }
        b_count >>= recombine;
        n_b <<= recombine;
        // Split blocks to increase time resolution.
        let mut time_divide = 0;
        while n_b & 1 == 0 && tf_change < 0 {
            if let Some(lowband) = lowband.as_deref_mut() {
                haar(lowband, n_b, b_count);
            }
            fill |= fill << b_count;
            b_count <<= 1;
            n_b >>= 1;
            time_divide += 1;
            tf_change += 1;
        }
        let blocks0 = b_count;
        let n_b0 = n_b;
        // Order the bins by time instead of frequency.
        if let Some(lowband) = lowband.as_deref_mut().filter(|_| blocks0 > 1) {
            deinterleave(lowband, n_b >> recombine, blocks0 << recombine, long_blocks);
        }

        let mut cm = self.partition(x, b, blocks0, lowband.as_deref(), lm, gain, fill);

        // Undo the reordering and the time-frequency changes.
        if blocks0 > 1 {
            interleave(x, n_b >> recombine, blocks0 << recombine, long_blocks);
        }
        n_b = n_b0;
        b_count = blocks0;
        for _ in 0..time_divide {
            b_count >>= 1;
            n_b <<= 1;
            cm |= cm >> b_count;
            haar(x, n_b, b_count);
        }
        for k in 0..recombine {
            cm = u32::from(BIT_DEINTERLEAVE[(cm & 0xf) as usize]);
            haar(x, n0 >> k, 1 << k);
        }
        b_count <<= recombine;

        // Keep the decoded shape, at unit gain per bin, as a later folding source.
        if let Some(out) = lowband_out {
            let scale = (n0 as f32).sqrt();
            for (out, &v) in out.iter_mut().zip(x.iter()) {
                *out = scale * v;
            }
        }
        cm & ((1 << b_count) - 1)
    }

    /// A mono band or half band: split while the bits exceed what one codebook can use,
    /// then decode pulses or fold (`quant_partition`).
    #[allow(clippy::too_many_arguments)] // The band state of the reference.
    fn partition(
        &mut self,
        x: &mut [f32],
        mut b: i32,
        blocks: usize,
        lowband: Option<&[f32]>,
        lm: i32,
        gain: f32,
        mut fill: u32,
    ) -> u32 {
        let n = x.len();
        if n == 1 {
            return self.single(x, None, None);
        }
        if lm != -1 && n > 2 && b > split_threshold(self.band, lm) {
            let half = n >> 1;
            let lm = lm - 1;
            if blocks == 1 {
                fill = (fill & 1) | (fill << 1);
            }
            let split_blocks = (blocks + 1) >> 1;
            let split = self.theta(half, b, split_blocks, blocks, lm, false, &mut fill);
            b -= split.used;
            let mut delta = split.delta;
            // More bits for low-energy blocks of transients.
            if blocks > 1 && split.itheta & 0x3fff != 0 {
                if split.itheta > 8192 {
                    // Pre-echo masking.
                    delta -= delta >> (4 - lm);
                } else {
                    // Forward masking of 1.5 dB per 10 ms.
                    delta = (delta + ((half as i32) << BITRES >> (5 - lm))).min(0);
                }
            }
            let mut mbits = ((b - delta) / 2).min(b).max(0);
            let mut sbits = b - mbits;
            self.remaining_bits -= split.used;
            let (first, second) = x.split_at_mut(half);
            let (low_first, low_second) = match lowband {
                Some(lowband) => (Some(&lowband[..half]), Some(&lowband[half..])),
                None => (None, None),
            };
            let shift = blocks >> 1;
            let rebalance = self.remaining_bits;
            let mut cm;
            if mbits >= sbits {
                cm = self.partition(
                    first,
                    mbits,
                    split_blocks,
                    low_first,
                    lm,
                    gain * split.mid,
                    fill,
                );
                let rebalance = mbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && split.itheta != 0 {
                    sbits += rebalance - (3 << BITRES);
                }
                cm |= self.partition(
                    second,
                    sbits,
                    split_blocks,
                    low_second,
                    lm,
                    gain * split.side,
                    fill >> split_blocks,
                ) << shift;
            } else {
                cm = self.partition(
                    second,
                    sbits,
                    split_blocks,
                    low_second,
                    lm,
                    gain * split.side,
                    fill >> split_blocks,
                ) << shift;
                let rebalance = sbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && split.itheta != 16384 {
                    mbits += rebalance - (3 << BITRES);
                }
                cm |= self.partition(
                    first,
                    mbits,
                    split_blocks,
                    low_first,
                    lm,
                    gain * split.mid,
                    fill,
                );
            }
            return cm;
        }

        // A single codebook.
        let mut q = bits_to_pulses(self.band, lm, b);
        let mut cost = pulses_to_bits(self.band, lm, q);
        self.remaining_bits -= cost;
        // Never exceed the budget.
        while self.remaining_bits < 0 && q > 0 {
            self.remaining_bits += cost;
            q -= 1;
            cost = pulses_to_bits(self.band, lm, q);
            self.remaining_bits -= cost;
        }
        if q != 0 {
            return pvq::decode_band(self.rc, x, pulse_count(q), self.spread, blocks, gain);
        }

        // No pulses: fold the source, or fill with noise.
        let mask = (1u32 << blocks) - 1;
        fill &= mask;
        if fill == 0 {
            x.fill(0.0);
            return 0;
        }
        let cm = match lowband {
            None => {
                for v in x.iter_mut() {
                    self.seed = lcg(self.seed);
                    *v = (self.seed as i32 >> 20) as f32;
                }
                mask
            }
            Some(lowband) => {
                // About 48 dB below the folding level.
                for (v, &l) in x.iter_mut().zip(lowband) {
                    self.seed = lcg(self.seed);
                    let noise = if self.seed & 0x8000 != 0 {
                        1.0 / 256.0
                    } else {
                        -1.0 / 256.0
                    };
                    *v = l + noise;
                }
                fill
            }
        };
        pvq::renormalise(x, gain);
        cm
    }

    /// A stereo band: mid and side, or intensity stereo (`quant_band_stereo`).
    #[allow(clippy::too_many_arguments)] // The band state of the reference.
    fn stereo(
        &mut self,
        x: &mut [f32],
        y: &mut [f32],
        mut b: i32,
        blocks: usize,
        lowband: Option<&mut [f32]>,
        lm: i32,
        lowband_out: &mut [f32],
        mut fill: u32,
    ) -> u32 {
        let n = x.len();
        if n == 1 {
            return self.single(x, Some(y), Some(lowband_out));
        }
        let orig_fill = fill;
        let split = self.theta(n, b, blocks, blocks, lm, true, &mut fill);
        b -= split.used;
        let cm;
        if n == 2 {
            // The side is orthogonal to the mid: one sign bit codes it.
            let sbits = if split.itheta != 0 && split.itheta != 16384 {
                1 << BITRES
            } else {
                0
            };
            let mbits = b - sbits;
            self.remaining_bits -= split.used + sbits;
            let swap = split.itheta > 8192;
            let negative = sbits != 0 && self.rc.bits(1) != 0;
            let sign = if negative { -1.0 } else { 1.0 };
            {
                let (x2, y2) = if swap {
                    (&mut *y, &mut *x)
                } else {
                    (&mut *x, &mut *y)
                };
                // The folding source is the side when it has the larger gain; the fill
                // before the angle's masking folds it.
                cm = self.band(
                    x2,
                    mbits,
                    blocks,
                    lowband,
                    lm,
                    Some(lowband_out),
                    1.0,
                    orig_fill,
                );
                y2[0] = -sign * x2[1];
                y2[1] = sign * x2[0];
            }
            for (l, r) in x.iter_mut().zip(y.iter_mut()) {
                let mid = split.mid * *l;
                let side = split.side * *r;
                *l = mid - side;
                *r = mid + side;
            }
        } else {
            let mut mbits = ((b - split.delta) / 2).min(b).max(0);
            let mut sbits = b - mbits;
            self.remaining_bits -= split.used;
            let rebalance = self.remaining_bits;
            // The mid keeps unit gain: it is the folding source of later bands.
            if mbits >= sbits {
                let mut c = self.band(x, mbits, blocks, lowband, lm, Some(lowband_out), 1.0, fill);
                let rebalance = mbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && split.itheta != 0 {
                    sbits += rebalance - (3 << BITRES);
                }
                c |= self.band(y, sbits, blocks, None, lm, None, split.side, fill >> blocks);
                cm = c;
            } else {
                let mut c = self.band(y, sbits, blocks, None, lm, None, split.side, fill >> blocks);
                let rebalance = sbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && split.itheta != 16384 {
                    mbits += rebalance - (3 << BITRES);
                }
                c |= self.band(x, mbits, blocks, lowband, lm, Some(lowband_out), 1.0, fill);
                cm = c;
            }
            merge(x, y, split.mid);
        }
        if split.invert {
            for v in y.iter_mut() {
                *v = -*v;
            }
        }
        cm
    }
}

/// Quantisation steps of a split angle (`compute_qn`).
fn quantisation_steps(n: usize, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> u32 {
    const EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n as i32 - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    // Leave enough bits to code one pulse in the side of a fully sided split.
    let qb = (b - pulse_cap - (4 << BITRES))
        .min((b + n2 * offset) / n2)
        .min(8 << BITRES);
    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let qn = EXP2_TABLE8[(qb & 7) as usize] >> (14 - (qb >> BITRES));
        ((qn + 1) >> 1 << 1) as u32
    }
}

/// Mid and side back to left and right, each renormalised (`stereo_merge`).
fn merge(x: &mut [f32], y: &mut [f32], mid: f32) {
    let (mut cross, mut side) = (0f32, 0f32);
    for (&l, &r) in x.iter().zip(y.iter()) {
        cross += l * r;
        side += r * r;
    }
    // The mid was decoded at unit norm.
    let cross = mid * cross;
    let el = mid * mid + side - 2.0 * cross;
    let er = mid * mid + side + 2.0 * cross;
    if er < 6e-4 || el < 6e-4 {
        y.copy_from_slice(x);
        return;
    }
    let lgain = 1.0 / el.sqrt();
    let rgain = 1.0 / er.sqrt();
    for (l, r) in x.iter_mut().zip(y.iter_mut()) {
        let m = mid * *l;
        let s = *r;
        *l = lgain * (m - s);
        *r = rgain * (m + s);
    }
}

/// Inputs of [`anti_collapse`].
pub(super) struct AntiCollapse<'a> {
    pub(super) bands: Range<usize>,
    pub(super) lm: usize,
    pub(super) channels: usize,
    pub(super) energy: &'a [[f32; BANDS]; 2],
    pub(super) prev1: &'a [[f32; BANDS]; 2],
    pub(super) prev2: &'a [[f32; BANDS]; 2],
    pub(super) pulses: &'a [i32; BANDS],
    pub(super) collapse: &'a [[u8; 2]; BANDS],
}

/// Fill the short blocks of transient bands that received no pulses with noise at the
/// level the energy history allows (RFC 6716 section 4.3.5). The noise is drawn for all
/// bands of the first channel, then the second, as FFmpeg does (the reference
/// alternates the channels band by band; the difference is within the conformance
/// tolerance).
pub(super) fn anti_collapse(p: &AntiCollapse<'_>, x: &mut [f32], frame: usize, mut seed: u32) {
    for c in 0..p.channels {
        for i in p.bands.clone() {
            let n0 = EBANDS[i + 1] - EBANDS[i];
            let n = n0 << p.lm;
            // Depth in 1/8 bits per bin.
            let depth = (1 + p.pulses[i]) / n as i32;
            let thresh = 0.5 * super::energy::exp2(-0.125 * depth as f32);
            let (mut prev1, mut prev2) = (p.prev1[c][i], p.prev2[c][i]);
            if p.channels == 1 {
                prev1 = prev1.max(p.prev1[1][i]);
                prev2 = prev2.max(p.prev2[1][i]);
            }
            let ediff = (p.energy[c][i] - prev1.min(prev2)).max(0.0);
            // Short blocks carry less energy than long ones.
            let mut r = 2.0 * super::energy::exp2(-ediff);
            if p.lm == 3 {
                r *= core::f32::consts::SQRT_2;
            }
            let r = r.min(thresh) * (1.0 / (n as f32).sqrt());
            let start = c * frame + (EBANDS[i] << p.lm);
            let band = &mut x[start..start + n];
            let mut filled = false;
            for k in 0..1 << p.lm {
                if p.collapse[i][c] & (1 << k) == 0 {
                    for j in 0..n0 {
                        seed = lcg(seed);
                        band[(j << p.lm) + k] = if seed & 0x8000 != 0 { r } else { -r };
                    }
                    filled = true;
                }
            }
            if filled {
                pvq::renormalise(band, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_exact_cosine_matches_its_endpoints_and_symmetry() {
        // Angles are multiples of 16384 / 256 strictly between 0 and 16384, the
        // endpoints being special cases of the callers.
        assert_eq!(bitexact_cos(64), 32767);
        assert!(bitexact_cos(16384 - 64) < 300);
        let mid = bitexact_cos(8192);
        assert!((mid - 23170).abs() < 8, "{mid}");
        // log2(tan) of 45 degrees is zero.
        assert_eq!(bitexact_log2tan(mid, mid), 0);
    }

    #[test]
    fn hadamard_interleaving_round_trips() {
        let original: Vec<f32> = (0..32).map(|v| v as f32).collect();
        for (n0, stride) in [(16, 2), (8, 4), (4, 8), (2, 16)] {
            for hadamard in [false, true] {
                let mut x = original.clone();
                deinterleave(&mut x, n0, stride, hadamard);
                interleave(&mut x, n0, stride, hadamard);
                assert_eq!(x, original);
            }
        }
    }

    #[test]
    fn split_steps_are_even_and_bounded() {
        for n in [2usize, 3, 4, 8, 22, 176] {
            for b in (0..2000).step_by(37) {
                let qn = quantisation_steps(n, b, 4, 24, false);
                assert!(
                    qn == 1 || (qn.is_multiple_of(2) && qn <= 256),
                    "{n} {b} {qn}"
                );
            }
        }
    }
}
