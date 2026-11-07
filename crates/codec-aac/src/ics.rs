//! individual_channel_stream() parsing, inverse quantisation and the spectral tools
//! (PNS, M/S, intensity, TNS) of ISO/IEC 14496-3 subpart 4.

use crate::error::{DecodeError, Result};
use crate::huffman::{FAST_BITS, SpectralBook, books, fast_field};
use crate::tables::*;
use std::sync::OnceLock;
use triergon_bitstream::CachedBitReader;

// window_sequence 0 of the syntax; the decoder only tests for the other three
#[allow(dead_code)]
pub(crate) const ONLY_LONG: u8 = 0;
pub(crate) const LONG_START: u8 = 1;
pub(crate) const EIGHT_SHORT: u8 = 2;
pub(crate) const LONG_STOP: u8 = 3;

pub(crate) const ZERO_HCB: u8 = 0;
pub(crate) const ESC_HCB: u8 = 11;
pub(crate) const NOISE_HCB: u8 = 13;
pub(crate) const INTENSITY_HCB2: u8 = 14;
pub(crate) const INTENSITY_HCB: u8 = 15;

/// Audio object types handled by the core.
pub(crate) const AOT_MAIN: u8 = 1;
pub(crate) const AOT_LC: u8 = 2;
pub(crate) const AOT_LTP: u8 = 4;

pub(crate) const MAX_SFB: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Ltp {
    pub present: bool,
    pub lag: u16,
    pub coef: u8,
    pub long_used: [bool; MAX_LTP_LONG_SFB],
}

impl Default for Ltp {
    fn default() -> Self {
        Ltp {
            present: false,
            lag: 0,
            coef: 0,
            long_used: [false; MAX_LTP_LONG_SFB],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IcsInfo {
    pub window_sequence: u8,
    pub window_shape: u8,
    pub max_sfb: usize,
    pub num_windows: usize,
    pub num_window_groups: usize,
    pub window_group_len: [u8; 8],
    pub predictor_data_present: bool,
    /// 0 = no reset, else predictor_reset_group_number.
    pub predictor_reset_group: u8,
    pub prediction_used: [bool; 41],
    pub ltp: [Ltp; 2],
    pub swb: &'static [u16],
    pub num_swb: usize,
}

impl Default for IcsInfo {
    fn default() -> Self {
        IcsInfo {
            window_sequence: 0,
            window_shape: 0,
            max_sfb: 0,
            num_windows: 1,
            num_window_groups: 1,
            window_group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            predictor_data_present: false,
            predictor_reset_group: 0,
            prediction_used: [false; 41],
            ltp: [Ltp::default(); 2],
            swb: swb_long(4),
            num_swb: 49,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TnsFilter {
    pub length: u8,
    pub order: u8,
    pub direction: bool,
    pub lpc: [f32; 21],
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Tns {
    pub present: bool,
    pub n_filt: [u8; 8],
    pub filt: [[TnsFilter; 3]; 8],
}

/// One decoded channel of a syntactic element for the current frame.
#[derive(Debug)]
pub(crate) struct ChannelData {
    /// bitmask of codebooks used in this frame (debug statistics)
    pub cb_mask: u32,
    pub pulse_used: bool,
    pub info: IcsInfo,
    pub global_gain: u8,
    /// band_type[g * MAX_SFB + sfb]
    pub band_type: [u8; 8 * MAX_SFB],
    /// scalefactor / intensity position / noise energy per band
    pub sf: [i32; 8 * MAX_SFB],
    pub tns: Tns,
    pub coef: [f32; 1024],
    /// quantised values, only kept for frames with pulse data
    pub qbuf: [i32; 1024],
}

impl ChannelData {
    pub(crate) fn new() -> ChannelData {
        ChannelData {
            cb_mask: 0,
            pulse_used: false,
            info: IcsInfo::default(),
            global_gain: 0,
            band_type: [0; 8 * MAX_SFB],
            sf: [0; 8 * MAX_SFB],
            tns: Tns::default(),
            coef: [0.0; 1024],
            qbuf: [0; 1024],
        }
    }
}

/// Shared decode context for the core.
pub(crate) struct CoreCfg {
    pub aot: u8,
    pub sf_index: usize,
}

fn pow43_table() -> &'static [f32] {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(|| {
        (0..8192)
            .map(|i| (i as f64).powf(4.0 / 3.0) as f32)
            .collect()
    })
}

/// 2^((i - 300) / 4) for i in 0..512: covers scalefactors, noise energies and IS positions.
pub(crate) fn pow2_quarter(i: i32) -> f32 {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    let t = T.get_or_init(|| {
        (0..600)
            .map(|i| 2f64.powf((i as f64 - 300.0) / 4.0) as f32)
            .collect()
    });
    t[(i + 300).clamp(0, 599) as usize]
}

pub(crate) fn parse_ics_info(
    br: &mut CachedBitReader<'_>,
    cfg: &CoreCfg,
    common_window: bool,
    info: &mut IcsInfo,
) -> Result<()> {
    if br.bit() {
        // ics_reserved_bit: FFmpeg reports and continues; we treat as invalid like the spec.
        return Err(DecodeError::InvalidData("ics_reserved_bit"));
    }
    info.window_sequence = br.read(2) as u8;
    info.window_shape = br.read(1) as u8;
    info.predictor_data_present = false;
    info.predictor_reset_group = 0;
    info.ltp[0].present = false;
    info.ltp[1].present = false;
    if info.window_sequence == EIGHT_SHORT {
        info.max_sfb = br.read(4) as usize;
        let grouping = br.read(7);
        info.num_windows = 8;
        info.num_window_groups = 1;
        info.window_group_len = [1, 0, 0, 0, 0, 0, 0, 0];
        for i in 0..7 {
            if grouping & (1 << (6 - i)) != 0 {
                info.window_group_len[info.num_window_groups - 1] += 1;
            } else {
                info.num_window_groups += 1;
                info.window_group_len[info.num_window_groups - 1] = 1;
            }
        }
        info.swb = swb_short(cfg.sf_index);
    } else {
        info.max_sfb = br.read(6) as usize;
        info.num_windows = 1;
        info.num_window_groups = 1;
        info.window_group_len = [1, 0, 0, 0, 0, 0, 0, 0];
        info.swb = swb_long(cfg.sf_index);
        info.predictor_data_present = br.bit();
        if info.predictor_data_present {
            match cfg.aot {
                AOT_MAIN => {
                    if br.bit() {
                        info.predictor_reset_group = br.read(5) as u8;
                        if info.predictor_reset_group == 0 || info.predictor_reset_group > 30 {
                            return Err(DecodeError::InvalidData("predictor_reset_group_number"));
                        }
                    }
                    let n = info.max_sfb.min(PRED_SFB_MAX[cfg.sf_index] as usize);
                    let (used, rest) = info.prediction_used.split_at_mut(n);
                    used.iter_mut().for_each(|u| *u = br.bit());
                    rest.fill(false);
                }
                AOT_LTP => {
                    info.ltp[0].present = br.bit();
                    if info.ltp[0].present {
                        parse_ltp(br, info.max_sfb, &mut info.ltp[0]);
                    }
                    if common_window {
                        info.ltp[1].present = br.bit();
                        if info.ltp[1].present {
                            parse_ltp(br, info.max_sfb, &mut info.ltp[1]);
                        }
                    }
                }
                _ => return Err(DecodeError::InvalidData("predictor_data_present in LC")),
            }
        }
    }
    info.num_swb = info.swb.len() - 1;
    if info.max_sfb > info.num_swb {
        return Err(DecodeError::InvalidData("max_sfb"));
    }
    Ok(())
}

fn parse_ltp(br: &mut CachedBitReader<'_>, max_sfb: usize, ltp: &mut Ltp) {
    ltp.lag = br.read(11) as u16;
    ltp.coef = br.read(3) as u8;
    let n = max_sfb.min(MAX_LTP_LONG_SFB);
    let (used, rest) = ltp.long_used.split_at_mut(n);
    used.iter_mut().for_each(|u| *u = br.bit());
    rest.fill(false);
}

/// individual_channel_stream(common_window, scale_flag) after ics_info.
pub(crate) fn parse_ics(
    br: &mut CachedBitReader<'_>,
    cfg: &CoreCfg,
    common_window: bool,
    scale_flag: bool,
    ch: &mut ChannelData,
    random: &mut u32,
) -> Result<()> {
    ch.global_gain = br.read(8) as u8;
    if !common_window {
        let mut info = ch.info;
        parse_ics_info(br, cfg, false, &mut info)?;
        ch.info = info;
    }
    parse_section_data(br, ch)?;
    parse_scalefactors(br, ch)?;
    let mut pulse = [(0usize, 0i32); 4];
    let mut npulse = 0;
    ch.tns.present = false;
    if !scale_flag {
        if br.bit() {
            if ch.info.window_sequence == EIGHT_SHORT {
                return Err(DecodeError::InvalidData("pulse data in short window"));
            }
            npulse = br.read(2) as usize + 1;
            let start = br.read(6) as usize;
            if start >= ch.info.num_swb {
                return Err(DecodeError::InvalidData("pulse_start_sfb"));
            }
            let mut k = ch.info.swb[start] as usize;
            for p in pulse.iter_mut().take(npulse) {
                k += br.read(5) as usize;
                if k >= 1024 {
                    return Err(DecodeError::InvalidData("pulse offset"));
                }
                *p = (k, br.read(4) as i32);
            }
        }
        if br.bit() {
            parse_tns(br, cfg, ch)?;
        }
        if br.bit() {
            return Err(DecodeError::Unsupported(
                crate::error::Feature::SsrGainControl,
            ));
        }
    }
    ch.pulse_used = npulse > 0;
    ch.cb_mask = 0;
    for g in 0..ch.info.num_window_groups {
        for sfb in 0..ch.info.max_sfb {
            ch.cb_mask |= 1 << ch.band_type[g * MAX_SFB + sfb];
        }
    }
    decode_spectrum(br, ch, &pulse[..npulse], random)?;
    if br.overrun() {
        return Err(DecodeError::InvalidData("channel stream overrun"));
    }
    Ok(())
}

fn parse_section_data(br: &mut CachedBitReader<'_>, ch: &mut ChannelData) -> Result<()> {
    let info = &ch.info;
    let (bits, esc) = if info.window_sequence == EIGHT_SHORT {
        (3, 7)
    } else {
        (5, 31)
    };
    for g in 0..info.num_window_groups {
        let mut k = 0usize;
        while k < info.max_sfb {
            let cb = br.read(4) as u8;
            if cb == 12 {
                return Err(DecodeError::InvalidData("reserved codebook 12"));
            }
            let mut len = 0usize;
            loop {
                let inc = br.read(bits) as usize;
                len += inc;
                if inc != esc {
                    break;
                }
                if br.overrun() {
                    return Err(DecodeError::InvalidData("section data overrun"));
                }
            }
            if k + len > info.max_sfb {
                return Err(DecodeError::InvalidData("section length"));
            }
            for sfb in k..k + len {
                ch.band_type[g * MAX_SFB + sfb] = cb;
            }
            k += len;
            if br.overrun() {
                return Err(DecodeError::InvalidData("section data overrun"));
            }
        }
        for sfb in info.max_sfb..MAX_SFB {
            ch.band_type[g * MAX_SFB + sfb] = ZERO_HCB;
        }
    }
    Ok(())
}

fn parse_scalefactors(br: &mut CachedBitReader<'_>, ch: &mut ChannelData) -> Result<()> {
    let vlc = &books().scalefactor;
    let mut sf = ch.global_gain as i32;
    let mut is_pos = 0i32;
    let mut noise = ch.global_gain as i32 - 90;
    let mut noise_pcm = true;
    let info = &ch.info;
    for g in 0..info.num_window_groups {
        for sfb in 0..info.max_sfb {
            let i = g * MAX_SFB + sfb;
            match ch.band_type[i] {
                ZERO_HCB => ch.sf[i] = 0,
                INTENSITY_HCB | INTENSITY_HCB2 => {
                    let d = vlc
                        .decode(br)
                        .ok_or(DecodeError::InvalidData("scalefactor code"))?
                        as i32
                        - 60;
                    is_pos += d;
                    ch.sf[i] = is_pos;
                }
                NOISE_HCB => {
                    if noise_pcm {
                        noise_pcm = false;
                        noise += br.read(9) as i32 - 256;
                    } else {
                        let d = vlc
                            .decode(br)
                            .ok_or(DecodeError::InvalidData("scalefactor code"))?
                            as i32
                            - 60;
                        noise += d;
                    }
                    ch.sf[i] = noise;
                }
                _ => {
                    let d = vlc
                        .decode(br)
                        .ok_or(DecodeError::InvalidData("scalefactor code"))?
                        as i32
                        - 60;
                    sf += d;
                    if !(0..=255).contains(&sf) {
                        return Err(DecodeError::InvalidData("scalefactor out of range"));
                    }
                    ch.sf[i] = sf;
                }
            }
        }
    }
    Ok(())
}

fn parse_tns(br: &mut CachedBitReader<'_>, cfg: &CoreCfg, ch: &mut ChannelData) -> Result<()> {
    let info = &ch.info;
    let short = info.window_sequence == EIGHT_SHORT;
    let (nf_bits, len_bits, ord_bits, max_order) = if short {
        (1, 4, 3, 7u8)
    } else {
        (2, 6, 5, if cfg.aot == AOT_MAIN { 20 } else { 12 })
    };
    ch.tns.present = true;
    for w in 0..info.num_windows {
        let n = br.read(nf_bits) as u8;
        ch.tns.n_filt[w] = n;
        if n == 0 {
            continue;
        }
        let coef_res = br.read(1);
        for flt in &mut ch.tns.filt[w][..n as usize] {
            flt.length = br.read(len_bits) as u8;
            flt.order = br.read(ord_bits) as u8;
            if flt.order > max_order {
                return Err(DecodeError::InvalidData("TNS order"));
            }
            if flt.order > 0 {
                flt.direction = br.bit();
                let compress = br.read(1);
                let coef_bits = coef_res + 3 - compress;
                let mut tmp = [0f32; 20];
                let res_bits = coef_res + 3;
                let iqfac = ((1u32 << (res_bits - 1)) as f64 - 0.5) / (std::f64::consts::PI / 2.0);
                let iqfac_m =
                    ((1u32 << (res_bits - 1)) as f64 + 0.5) / (std::f64::consts::PI / 2.0);
                for t in tmp.iter_mut().take(flt.order as usize) {
                    let raw = br.read(coef_bits) as i32;
                    let v = (raw << (32 - coef_bits)) >> (32 - coef_bits);
                    let v = v as f64;
                    *t = (v / if v >= 0.0 { iqfac } else { iqfac_m }).sin() as f32;
                }
                // reflection coefficients -> LPC (step-up)
                let ord = flt.order as usize;
                let mut a = [0f32; 21];
                a[0] = 1.0;
                for m in 1..=ord {
                    let mut b = [0f32; 21];
                    for i in 1..m {
                        // fused multiply-add: matches the FMA-contracted reference build
                        b[i] = tmp[m - 1].mul_add(a[m - i], a[i]);
                    }
                    a[1..m].copy_from_slice(&b[1..m]);
                    a[m] = tmp[m - 1];
                }
                flt.lpc = a;
            }
        }
    }
    Ok(())
}

/// Next PNS random value (FFmpeg-compatible LCG; observed behaviour).
#[inline(always)]
pub(crate) fn lcg(state: &mut u32) -> i32 {
    *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
    *state as i32
}

/// Negate the non-zero entries of `v` whose sign bit (MSB first, one per non-zero value,
/// following the codeword) is set. Branch-free: sign bits are data, not control flow.
#[inline(always)]
fn read_signs_f32(br: &mut CachedBitReader<'_>, v: &mut [f32]) {
    let bits = br.peek32();
    let mut used = 0u32;
    for x in v.iter_mut() {
        // unsigned codebooks: values are +0.0 or positive
        let nz = (x.to_bits() != 0) as u32;
        let sign = ((bits << used) >> 31) & nz;
        *x = f32::from_bits(x.to_bits() ^ (sign << 31));
        used += nz;
    }
    br.skip(used as usize);
}

#[inline(always)]
fn read_signs_i32(br: &mut CachedBitReader<'_>, v: &mut [i32]) {
    let bits = br.peek32();
    let mut used = 0u32;
    for x in v.iter_mut() {
        let nz = (*x != 0) as u32;
        let sign = (((bits << used) >> 31) & nz) as i32;
        *x = (*x ^ -sign) + sign;
        used += nz;
    }
    br.skip(used as usize);
}

/// sign(v) |v|^(4/3) for v in -16..=16 (index v + 16).
fn pow43_signed() -> &'static [f32; 33] {
    static T: OnceLock<[f32; 33]> = OnceLock::new();
    T.get_or_init(|| {
        let p = pow43_table();
        std::array::from_fn(|i| if i < 16 { -p[16 - i] } else { p[i - 16] })
    })
}

/// One band of an N-tuple codebook without escapes, decoded and inverse quantised.
#[inline(always)]
fn band<const N: usize, const UNSIGNED: bool>(
    br: &mut CachedBitReader<'_>,
    book: &SpectralBook,
    gain: f32,
    out: &mut [f32],
) -> Result<()> {
    let p43 = pow43_signed();
    for o in out.chunks_exact_mut(N) {
        let e = book.fast[(br.peek32() >> (32 - FAST_BITS)) as usize];
        if e != 0 {
            br.skip((e & 63) as usize);
            for i in 0..N {
                o[i] = p43[(fast_field(e, i) + 16) as usize & 31] * gain;
            }
            continue;
        }
        let sym = book
            .vlc
            .decode(br)
            .ok_or(DecodeError::InvalidData("spectral code"))? as usize;
        let mut v = [0f32; N];
        v.copy_from_slice(&book.fvalues[sym][..N]);
        if UNSIGNED {
            read_signs_f32(br, &mut v);
        }
        for i in 0..N {
            o[i] = v[i] * gain;
        }
    }
    Ok(())
}

/// Escape sequence of codebook 11 for a value of magnitude 16.
#[inline(always)]
fn escape(br: &mut CachedBitReader<'_>, v: i32) -> Result<i32> {
    if v != 16 && v != -16 {
        return Ok(v);
    }
    let mut nbits = 4u32;
    while br.bit() {
        nbits += 1;
        if nbits > 12 {
            return Err(DecodeError::InvalidData("escape sequence"));
        }
    }
    let e = (1i32 << nbits) + br.read(nbits) as i32;
    Ok(if v < 0 { -e } else { e })
}

#[inline(always)]
fn dequant(q: i32, pow43: &[f32], gain: f32) -> f32 {
    let a = q.unsigned_abs() as usize;
    let v = if a < 8192 {
        pow43[a]
    } else {
        (a as f32).powf(4.0 / 3.0)
    };
    if q < 0 { -v * gain } else { v * gain }
}

/// One band of codebook 11 (pairs, unsigned, escapes), decoded and inverse quantised.
fn band_esc(
    br: &mut CachedBitReader<'_>,
    book: &SpectralBook,
    gain: f32,
    pow43: &[f32],
    out: &mut [f32],
) -> Result<()> {
    for o in out.chunks_exact_mut(2) {
        let e = book.fast[(br.peek32() >> (32 - FAST_BITS)) as usize];
        let mut q = if e != 0 {
            br.skip((e & 63) as usize);
            [fast_field(e, 0), fast_field(e, 1)]
        } else {
            let sym = book
                .vlc
                .decode(br)
                .ok_or(DecodeError::InvalidData("spectral code"))? as usize;
            let vals = book.values[sym];
            let mut q = [vals[0] as i32, vals[1] as i32];
            read_signs_i32(br, &mut q);
            q
        };
        q[0] = escape(br, q[0])?;
        q[1] = escape(br, q[1])?;
        o[0] = dequant(q[0], pow43, gain);
        o[1] = dequant(q[1], pow43, gain);
    }
    Ok(())
}

/// One band decoded to quantised integers (used when pulse data follows).
fn band_int(
    br: &mut CachedBitReader<'_>,
    book: &SpectralBook,
    esc: bool,
    out: &mut [i32],
) -> Result<()> {
    let n = if book.quad { 4 } else { 2 };
    for o in out.chunks_exact_mut(n) {
        let sym = book
            .vlc
            .decode(br)
            .ok_or(DecodeError::InvalidData("spectral code"))? as usize;
        let vals = book.values[sym];
        for i in 0..n {
            o[i] = vals[i] as i32;
        }
        // sign bits for all non-zero values follow the codeword, then the escapes
        if book.unsigned {
            read_signs_i32(br, o);
        }
        if esc {
            o[0] = escape(br, o[0])?;
            o[1] = escape(br, o[1])?;
        }
    }
    Ok(())
}

fn decode_spectrum(
    br: &mut CachedBitReader<'_>,
    ch: &mut ChannelData,
    pulse: &[(usize, i32)],
    random: &mut u32,
) -> Result<()> {
    let bk = books();
    let pow43 = pow43_table();
    let info = &ch.info;
    let swb = info.swb;
    let coef = &mut ch.coef[..];
    for c in coef.iter_mut() {
        *c = 0.0;
    }
    // pulse data modifies the quantised integers: only then keep them before dequantisation
    // (only bands with a spectral codebook are read back, so no clearing is needed)
    let need_int = !pulse.is_empty();
    let qbuf = &mut ch.qbuf[..];
    let mut win = 0usize;
    for g in 0..info.num_window_groups {
        let glen = info.window_group_len[g] as usize;
        for sfb in 0..info.max_sfb {
            let bi = g * MAX_SFB + sfb;
            let cb = ch.band_type[bi];
            let start = swb[sfb] as usize;
            let end = swb[sfb + 1] as usize;
            let width = end - start;
            match cb {
                ZERO_HCB | INTENSITY_HCB | INTENSITY_HCB2 => {}
                NOISE_HCB => {
                    let scale0 = pow2_quarter(ch.sf[bi]);
                    for w in win..win + glen {
                        let off = w * 128 + start;
                        let band = &mut coef[off..off + width];
                        let mut energy = 0f32;
                        for v in band.iter_mut() {
                            let r = lcg(random) as f32;
                            *v = r;
                            energy += r * r;
                        }
                        let scale = scale0 / energy.sqrt();
                        for v in band.iter_mut() {
                            *v *= scale;
                        }
                    }
                }
                1..=11 => {
                    let book = &bk.spectral[cb as usize - 1];
                    let gain = pow2_quarter(ch.sf[bi] - 100);
                    for w in win..win + glen {
                        let off = w * 128 + start;
                        if need_int {
                            band_int(br, book, cb == ESC_HCB, &mut qbuf[off..off + width])?;
                            continue;
                        }
                        let out = &mut coef[off..off + width];
                        match cb {
                            1 | 2 => band::<4, false>(br, book, gain, out)?,
                            3 | 4 => band::<4, true>(br, book, gain, out)?,
                            5 | 6 => band::<2, false>(br, book, gain, out)?,
                            7..=10 => band::<2, true>(br, book, gain, out)?,
                            _ => band_esc(br, book, gain, pow43, out)?,
                        }
                    }
                }
                _ => return Err(DecodeError::InvalidData("band type")),
            }
            if br.overrun() {
                return Err(DecodeError::InvalidData("spectral data overrun"));
            }
        }
        win += glen;
    }
    if need_int {
        for &(k, amp) in pulse {
            let q = &mut qbuf[k];
            if *q > 0 {
                *q += amp;
            } else {
                *q -= amp;
            }
        }
        for sfb in 0..info.max_sfb {
            let cb = ch.band_type[sfb];
            if (1..=11).contains(&cb) {
                let gain = pow2_quarter(ch.sf[sfb] - 100);
                for k in swb[sfb] as usize..swb[sfb + 1] as usize {
                    coef[k] = dequant(qbuf[k], pow43, gain);
                }
            }
        }
    }
    Ok(())
}

/// M/S stereo (ISO/IEC 14496-3 4.6.8.1). `mask[g*MAX_SFB+sfb]`.
pub(crate) fn apply_ms(l: &mut ChannelData, r: &mut ChannelData, mask: &[bool]) {
    let info = &l.info;
    let mut win = 0;
    for g in 0..info.num_window_groups {
        let glen = info.window_group_len[g] as usize;
        for sfb in 0..info.max_sfb {
            let i = g * MAX_SFB + sfb;
            if !mask[i] || l.band_type[i] >= NOISE_HCB || r.band_type[i] >= NOISE_HCB {
                continue;
            }
            let s = info.swb[sfb] as usize;
            let e = info.swb[sfb + 1] as usize;
            for w in win..win + glen {
                let (a, b) = (
                    &mut l.coef[w * 128 + s..w * 128 + e],
                    &mut r.coef[w * 128 + s..w * 128 + e],
                );
                for (x, y) in a.iter_mut().zip(b.iter_mut()) {
                    let m = *x;
                    let sd = *y;
                    *x = m + sd;
                    *y = m - sd;
                }
            }
        }
        win += glen;
    }
}

/// Intensity stereo (ISO/IEC 14496-3 4.6.8.2.3).
pub(crate) fn apply_intensity(l: &ChannelData, r: &mut ChannelData, ms_present: u8, mask: &[bool]) {
    let info = &r.info;
    let mut win = 0;
    for g in 0..info.num_window_groups {
        let glen = info.window_group_len[g] as usize;
        for sfb in 0..info.max_sfb {
            let i = g * MAX_SFB + sfb;
            let bt = r.band_type[i];
            if bt != INTENSITY_HCB && bt != INTENSITY_HCB2 {
                continue;
            }
            let mut c = if bt == INTENSITY_HCB { 1.0f32 } else { -1.0 };
            if ms_present != 0 && mask[i] {
                c = -c;
            }
            let scale = c * pow2_quarter(-r.sf[i]);
            let s = info.swb[sfb] as usize;
            let e = info.swb[sfb + 1] as usize;
            for w in win..win + glen {
                for k in w * 128 + s..w * 128 + e {
                    r.coef[k] = l.coef[k] * scale;
                }
            }
        }
        win += glen;
    }
}

pub(crate) const TNS_SYNTHESIS: bool = true;
pub(crate) const TNS_ANALYSIS: bool = false;

/// TNS synthesis filtering (ISO/IEC 14496-3 4.6.9.3) of the channel spectrum.
pub(crate) fn apply_tns(ch: &mut ChannelData, cfg: &CoreCfg) {
    if !ch.tns.present {
        return;
    }
    tns_filter(&ch.tns, &ch.info, cfg, &mut ch.coef, TNS_SYNTHESIS);
}

/// All-pole (synthesis) or all-zero (analysis, used by LTP) TNS filtering of `coef`.
/// Products are fused (`mul_add`) like the FMA-contracted reference build.
pub(crate) fn tns_filter(
    tns: &Tns,
    info: &IcsInfo,
    cfg: &CoreCfg,
    coef: &mut [f32],
    synthesis: bool,
) {
    let short = info.window_sequence == EIGHT_SHORT;
    let max_bands = if short {
        TNS_MAX_BANDS_SHORT[cfg.sf_index]
    } else {
        TNS_MAX_BANDS_LONG[cfg.sf_index]
    } as usize;
    let mb = max_bands.min(info.max_sfb);
    for w in 0..info.num_windows {
        let mut bottom = info.num_swb;
        for flt in &tns.filt[w][..tns.n_filt[w] as usize] {
            let top = bottom;
            bottom = top.saturating_sub(flt.length as usize);
            let order = flt.order as usize;
            if order == 0 {
                continue;
            }
            let start = info.swb[bottom.min(mb)] as usize;
            let end = info.swb[top.min(mb)] as usize;
            if end <= start {
                continue;
            }
            let spec = &mut coef[w * 128..w * 128 + if short { 128 } else { 1024 }];
            let mut state = [0f32; 20];
            let size = end - start;
            let lpc = &flt.lpc;
            for i in 0..size {
                let k = if flt.direction {
                    end - 1 - i
                } else {
                    start + i
                };
                let x = spec[k];
                let mut y = x;
                if synthesis {
                    for j in 0..order {
                        y = (-state[j]).mul_add(lpc[j + 1], y);
                    }
                    for j in (1..order).rev() {
                        state[j] = state[j - 1];
                    }
                    state[0] = y;
                } else {
                    for j in 0..order {
                        y = state[j].mul_add(lpc[j + 1], y);
                    }
                    for j in (1..order).rev() {
                        state[j] = state[j - 1];
                    }
                    state[0] = x;
                }
                spec[k] = y;
            }
        }
    }
}
