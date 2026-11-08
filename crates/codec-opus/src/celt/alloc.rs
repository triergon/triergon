//! Bit allocation (RFC 6716 section 4.3.3): band caps, the dynamic boosts' effect, the
//! interpolation between static allocation vectors, band skipping, intensity and dual
//! stereo signalling, and the split between fine energy and PVQ bits.
//!
//! Bit counts are in 1/8 bit units (`BITRES`) unless stated otherwise.

use super::energy::MAX_FINE_BITS;
use super::tables::{
    ALLOC_VECTORS, BANDS, CACHE_BITS, CACHE_CAPS, CACHE_INDEX, EBANDS, LOG_N, LOG2_FRAC,
};
use crate::range::RangeDecoder;
use core::ops::Range;

/// Fractional bits per bit.
pub(super) const BITRES: i32 = 3;
const ALLOC_STEPS: u32 = 6;
const FINE_OFFSET: i32 = 21;

/// The decoded allocation of one frame.
#[derive(Clone, Debug, Default)]
pub(super) struct Allocation {
    /// PVQ bits per band (1/8 bit, both channels).
    pub(super) pulses: [i32; BANDS],
    /// Fine energy bits per band and channel (whole bits).
    pub(super) fine_bits: [i32; BANDS],
    /// Bands that come first in the final energy refinement pass.
    pub(super) fine_priority: [bool; BANDS],
    /// Bands after the last coded band get no PVQ bits.
    pub(super) coded_bands: usize,
    /// Bits beyond the caps, rebalanced while decoding the bands.
    pub(super) balance: i32,
    /// First band coded with intensity stereo.
    pub(super) intensity: usize,
    pub(super) dual_stereo: bool,
}

/// Width of `band` in bins at frame size `lm`.
#[inline]
pub(super) fn band_width(band: usize, lm: usize) -> usize {
    (EBANDS[band + 1] - EBANDS[band]) << lm
}

/// The largest useful allocation of each band (`init_caps`).
pub(super) fn caps(lm: usize, channels: usize) -> [i32; BANDS] {
    let row = &CACHE_CAPS[BANDS * (2 * lm + channels - 1)..];
    core::array::from_fn(|band| {
        ((i32::from(row[band]) + 64) * channels as i32 * band_width(band, lm) as i32) >> 2
    })
}

/// The pulse cache of `band` for a (possibly split) block of size `lm`, which may be -1:
/// entry 0 is the largest pulse index, entry `q` the bits for index `q`, minus one.
#[inline]
fn pulse_cache(band: usize, lm: i32) -> &'static [u8] {
    let index = CACHE_INDEX[(lm + 1) as usize * BANDS + band];
    &CACHE_BITS[index.max(0) as usize..]
}

/// The largest bit count `bits` can buy in `band` without splitting, plus one bit's
/// margin (the split threshold of RFC 6716 section 4.3.4.4).
#[inline]
pub(super) fn split_threshold(band: usize, lm: i32) -> i32 {
    let cache = pulse_cache(band, lm);
    i32::from(cache[usize::from(cache[0])]) + 12
}

/// The pulse index whose cost is closest to `bits` (`bits2pulses`).
pub(super) fn bits_to_pulses(band: usize, lm: i32, bits: i32) -> usize {
    let cache = pulse_cache(band, lm);
    let bits = bits - 1;
    let (mut lo, mut hi) = (0usize, usize::from(cache[0]));
    for _ in 0..6 {
        let mid = (lo + hi + 1) >> 1;
        if i32::from(cache[mid]) >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let below = if lo == 0 { -1 } else { i32::from(cache[lo]) };
    if bits - below <= i32::from(cache[hi]) - bits {
        lo
    } else {
        hi
    }
}

/// Bits used by pulse index `q` in `band` (`pulses2bits`).
#[inline]
pub(super) fn pulses_to_bits(band: usize, lm: i32, q: usize) -> i32 {
    if q == 0 {
        0
    } else {
        i32::from(pulse_cache(band, lm)[q]) + 1
    }
}

/// Pulses for pulse index `q` (`get_pulses`): exact up to 8, then 8 per octave.
#[inline]
pub(super) fn pulse_count(q: usize) -> usize {
    if q < 8 {
        q
    } else {
        (8 + (q & 7)) << ((q >> 3) - 1)
    }
}

/// Inputs of [`allocate`] decoded from the frame header.
pub(super) struct AllocationRequest<'a> {
    pub(super) bands: Range<usize>,
    pub(super) boosts: &'a [i32; BANDS],
    pub(super) caps: &'a [i32; BANDS],
    pub(super) trim: i32,
    /// Bits available (1/8 bit).
    pub(super) total: i32,
    pub(super) channels: usize,
    pub(super) lm: usize,
}

/// Decode the band allocation (`compute_allocation`).
pub(super) fn allocate(rc: &mut RangeDecoder<'_>, request: &AllocationRequest<'_>) -> Allocation {
    let AllocationRequest {
        ref bands,
        boosts,
        caps,
        trim,
        total,
        channels,
        lm,
    } = *request;
    let (start, end) = (bands.start, bands.end);
    let c = channels as i32;
    let mut total = total.max(0);
    // Reserve a bit to signal the end of skipped bands.
    let skip_reserve = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_reserve;
    // Reserve bits for the intensity and dual stereo parameters.
    let (mut intensity_reserve, mut dual_reserve) = (0, 0);
    if channels == 2 {
        intensity_reserve = i32::from(LOG2_FRAC[end - start]);
        if intensity_reserve > total {
            intensity_reserve = 0;
        } else {
            total -= intensity_reserve;
            dual_reserve = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_reserve;
        }
    }

    let mut thresh = [0i32; BANDS];
    let mut trim_offset = [0i32; BANDS];
    for j in bands.clone() {
        let width = (EBANDS[j + 1] - EBANDS[j]) as i32;
        // Below this threshold no PVQ bits are allocated.
        thresh[j] = (c << BITRES).max((((3 * width) << lm) << BITRES) >> 4);
        // Tilt of the allocation curve.
        trim_offset[j] = (c
            * width
            * (trim - 5 - lm as i32)
            * (end - j - 1) as i32
            * (1 << (lm as i32 + BITRES)))
            >> 6;
        // Single-bin bands benefit more from coarse energy.
        if band_width(j, lm) == 1 {
            trim_offset[j] -= c << BITRES;
        }
    }
    // The trim tilts every nonzero allocation, the caps included.
    let tilt = |bits: i32, j: usize| {
        if bits > 0 {
            (bits + trim_offset[j]).max(0)
        } else {
            bits
        }
    };
    let vector_bits = |vector: usize, j: usize| {
        let width = (EBANDS[j + 1] - EBANDS[j]) as i32;
        tilt(
            ((c * width * i32::from(ALLOC_VECTORS[vector][j])) << lm) >> 2,
            j,
        )
    };

    // Find the two allocation vectors that bracket the budget.
    let (mut lo, mut hi) = (1usize, ALLOC_VECTORS.len() - 1);
    while lo <= hi {
        let mid = (lo + hi) >> 1;
        let mut done = false;
        let mut psum = 0;
        for j in bands.clone().rev() {
            let bits = vector_bits(mid, j) + boosts[j];
            if bits >= thresh[j] || done {
                done = true;
                psum += bits.min(caps[j]);
            } else if bits >= c << BITRES {
                psum += c << BITRES;
            }
        }
        // `mid >= 1`, so `hi` cannot wrap.
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
    }
    let hi = lo;
    let lo = lo - 1;
    let mut skip_start = start;
    let mut bits1 = [0i32; BANDS];
    let mut bits2 = [0i32; BANDS];
    for j in bands.clone() {
        let mut low = vector_bits(lo, j);
        let mut high = if hi >= ALLOC_VECTORS.len() {
            tilt(caps[j], j)
        } else {
            vector_bits(hi, j)
        };
        if lo > 0 {
            low += boosts[j];
        }
        high += boosts[j];
        if boosts[j] > 0 {
            skip_start = j;
        }
        bits1[j] = low;
        bits2[j] = (high - low).max(0);
    }

    let mut allocation = Allocation::default();
    interpolate(
        rc,
        &Interpolation {
            bands: bands.clone(),
            skip_start,
            bits1,
            bits2,
            thresh,
            caps,
            channels,
            lm,
        },
        total,
        skip_reserve,
        intensity_reserve,
        dual_reserve,
        &mut allocation,
    );
    allocation
}

struct Interpolation<'a> {
    bands: Range<usize>,
    skip_start: usize,
    bits1: [i32; BANDS],
    bits2: [i32; BANDS],
    thresh: [i32; BANDS],
    caps: &'a [i32; BANDS],
    channels: usize,
    lm: usize,
}

/// Interpolate between the bracketing vectors, decode skipping and the stereo
/// parameters, and split each band's bits between fine energy and PVQ
/// (`interp_bits2pulses`).
fn interpolate(
    rc: &mut RangeDecoder<'_>,
    p: &Interpolation<'_>,
    mut total: i32,
    skip_reserve: i32,
    mut intensity_reserve: i32,
    mut dual_reserve: i32,
    out: &mut Allocation,
) {
    let (start, end) = (p.bands.start, p.bands.end);
    let c = p.channels as i32;
    let stereo = i32::from(p.channels > 1);
    let alloc_floor = c << BITRES;
    let log_m = (p.lm as i32) << BITRES;

    let (mut lo, mut hi) = (0, 1 << ALLOC_STEPS);
    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0;
        let mut done = false;
        for j in p.bands.clone().rev() {
            let tmp = p.bits1[j] + ((mid * p.bits2[j]) >> ALLOC_STEPS);
            if tmp >= p.thresh[j] || done {
                done = true;
                psum += tmp.min(p.caps[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let bits = &mut out.pulses;
    let mut psum = 0;
    let mut done = false;
    for j in p.bands.clone().rev() {
        let mut tmp = p.bits1[j] + ((lo * p.bits2[j]) >> ALLOC_STEPS);
        if tmp < p.thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(p.caps[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end.
    let width_from_start = |band: usize| (EBANDS[band] - EBANDS[start]) as i32;
    let mut coded = end;
    loop {
        let j = coded - 1;
        // Never skip the first band, nor a boosted one.
        if j <= p.skip_start {
            total += skip_reserve;
            break;
        }
        // The left-over bits this band would get, including those of skipped bands.
        let mut left = total - psum;
        let per_coeff = left / width_from_start(coded);
        left -= width_from_start(coded) * per_coeff;
        let rem = (left - width_from_start(j)).max(0);
        let width = (EBANDS[coded] - EBANDS[j]) as i32;
        let mut band_bits = bits[j] + per_coeff * width + rem;
        // A skip decision is coded only above the threshold; otherwise it is forced.
        if band_bits >= p.thresh[j].max(alloc_floor + (1 << BITRES)) {
            if rc.bit_logp(1) {
                break;
            }
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        // Reclaim this band's bits.
        psum -= bits[j] + intensity_reserve;
        if intensity_reserve > 0 {
            intensity_reserve = i32::from(LOG2_FRAC[j - start]);
        }
        psum += intensity_reserve;
        if band_bits >= alloc_floor {
            // Enough for one fine energy bit per channel.
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            bits[j] = 0;
        }
        coded -= 1;
    }

    out.intensity = if intensity_reserve > 0 {
        start + rc.uint((coded + 1 - start) as u32) as usize
    } else {
        0
    };
    if out.intensity <= start {
        total += dual_reserve;
        dual_reserve = 0;
    }
    out.dual_stereo = dual_reserve > 0 && rc.bit_logp(1);

    // Spread the remaining bits evenly, then the remainder from the bottom.
    let mut left = total - psum;
    let per_coeff = left / width_from_start(coded);
    left -= width_from_start(coded) * per_coeff;
    for j in start..coded {
        let width = (EBANDS[j + 1] - EBANDS[j]) as i32;
        bits[j] += per_coeff * width;
        let extra = left.min(width);
        bits[j] += extra;
        left -= extra;
    }

    let mut balance = 0;
    for j in start..coded {
        let n = band_width(j, p.lm) as i32;
        bits[j] += balance;
        let mut excess;
        if n > 1 {
            excess = (bits[j] - p.caps[j]).max(0);
            bits[j] -= excess;
            // Compensate for the extra degree of freedom in stereo.
            let den = c * n
                + i32::from(p.channels == 2 && n > 2 && !out.dual_stereo && j < out.intensity);
            let nc_log_n = den * (LOG_N[j] + log_m);
            // Offset the fine bits by log2(N)/2 + FINE_OFFSET from their fair share.
            let mut offset = (nc_log_n >> 1) - den * FINE_OFFSET;
            // N = 2 is the only point off the curve.
            if n == 2 {
                offset += den << BITRES >> 2;
            }
            // The second and third fine energy bits.
            if bits[j] + offset < (den * 2) << BITRES {
                offset += nc_log_n >> 2;
            } else if bits[j] + offset < (den * 3) << BITRES {
                offset += nc_log_n >> 3;
            }
            let mut fine = ((bits[j] + offset + (den << (BITRES - 1))) / (den << BITRES)).max(0);
            if c * fine > bits[j] >> BITRES {
                fine = bits[j] >> stereo >> BITRES;
            }
            fine = fine.min(MAX_FINE_BITS);
            out.fine_priority[j] = fine * (den << BITRES) >= bits[j] + offset;
            out.fine_bits[j] = fine;
            bits[j] -= (c * fine) << BITRES;
        } else {
            // A single bin: everything but a sign bit goes to fine energy.
            excess = (bits[j] - (c << BITRES)).max(0);
            bits[j] -= excess;
            out.fine_bits[j] = 0;
            out.fine_priority[j] = true;
        }
        // Fine energy cannot use the rebalancing of the band loop; do it here.
        if excess > 0 {
            let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - out.fine_bits[j]);
            out.fine_bits[j] += extra_fine;
            let extra_bits = (extra_fine * c) << BITRES;
            out.fine_priority[j] = extra_bits >= excess - balance;
            excess -= extra_bits;
        }
        balance = excess;
    }
    out.balance = balance;
    // Skipped bands use all their bits for fine energy.
    for j in coded..end {
        out.fine_bits[j] = bits[j] >> stereo >> BITRES;
        bits[j] = 0;
        out.fine_priority[j] = out.fine_bits[j] < 1;
    }
    out.coded_bands = coded;
}
