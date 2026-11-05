//! SBR frequency band tables (ISO/IEC 14496-3 4.6.18.3.2), patch construction
//! (4.6.18.6.3) and limiter band table (4.6.18.7.5? "limiter frequency band table").

use super::Header;
use crate::error::{DecodeError, Result};

#[derive(Clone, Default, Debug)]
#[allow(dead_code)] // k0 / k2 / master kept for diagnostics
pub(crate) struct FreqTables {
    pub k0: usize,
    pub k2: usize,
    pub kx: usize,
    pub m: usize,
    pub master: Vec<usize>,
    pub high: Vec<usize>,
    pub low: Vec<usize>,
    pub noise: Vec<usize>,
    pub lim: Vec<usize>,
    pub n_high: usize,
    pub n_low: usize,
    pub n_q: usize,
    pub n_lim: usize,
    pub patch_num: Vec<usize>,
    pub patch_start: Vec<usize>,
}

/// startMin / stopMin offset table (ISO/IEC 14496-3 Table 4.82) rows by fs_SBR.
const START_OFFSET: [[i32; 16]; 6] = [
    [-8, -7, -6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7],
    [-5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13],
    [-5, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16],
    [-6, -4, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16],
    [-4, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20],
    [-2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20, 24],
];

fn nint(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// Band borders between `start` and `stop` with `n` geometrically spaced bands, as
/// integer widths sorted ascending (4.6.18.3.2.1 vDk computation).
fn geometric_dk(start: usize, stop: usize, n: usize) -> Vec<i64> {
    let mut vk = Vec::with_capacity(n + 1);
    for k in 0..=n {
        vk.push(nint(
            start as f64 * (stop as f64 / start as f64).powf(k as f64 / n as f64),
        ));
    }
    let mut dk: Vec<i64> = (0..n).map(|k| vk[k + 1] - vk[k]).collect();
    dk.sort_unstable();
    dk
}

pub(crate) fn compute(h: &Header, fs: u32) -> Result<FreqTables> {
    let fs_i = fs as i64;
    let row = match fs {
        16000 => 0,
        22050 => 1,
        24000 => 2,
        32000 => 3,
        44100..=64000 => 4,
        _ if fs > 64000 => 5,
        _ => return Err(DecodeError::InvalidData("SBR sample rate")),
    };
    let temp = if fs < 32000 {
        3000
    } else if fs < 64000 {
        4000
    } else {
        5000
    };
    let start_min = ((temp * 128) + fs_i / 2) / fs_i;
    // stopMin uses twice the startMin frequency (6 / 8 / 10 kHz by fs_SBR range)
    let stop_min = ((2 * temp * 128) + fs_i / 2) / fs_i;
    let k0 = (start_min + START_OFFSET[row][h.start_freq as usize] as i64) as usize;
    let k2 = if h.stop_freq < 14 {
        let mut vk = Vec::with_capacity(14);
        for i in 0..14 {
            vk.push(nint(
                stop_min as f64 * (64.0 / stop_min as f64).powf(i as f64 / 13.0),
            ));
        }
        let mut dk: Vec<i64> = (0..13).map(|i| vk[i + 1] - vk[i]).collect();
        dk.sort_unstable();
        let s: i64 = dk[..h.stop_freq as usize].iter().sum();
        (stop_min + s).min(64) as usize
    } else if h.stop_freq == 14 {
        (2 * k0).min(64)
    } else {
        (3 * k0).min(64)
    };
    if k0 >= k2 || k0 == 0 {
        return Err(DecodeError::InvalidData("SBR k0/k2"));
    }
    let max_diff = if fs <= 32000 {
        48
    } else if fs == 44100 {
        35
    } else {
        32
    };
    if k2 - k0 > max_diff {
        return Err(DecodeError::InvalidData("SBR k2 - k0 too large"));
    }
    // master table
    let mut master: Vec<usize> = Vec::new();
    if h.freq_scale == 0 {
        let dk: i64 = if h.alter_scale { 2 } else { 1 };
        let num_bands = 2 * nint((k2 - k0) as f64 / (dk as f64 * 2.0));
        let num_bands = if h.alter_scale {
            num_bands
        } else {
            2 * (((k2 - k0) as i64) / 2)
        };
        if num_bands <= 0 {
            return Err(DecodeError::InvalidData("SBR master bands"));
        }
        let mut vdk = vec![dk; num_bands as usize];
        let mut k2_diff = k2 as i64 - (k0 as i64 + num_bands * dk);
        let (incr, mut k): (i64, i64) = if k2_diff < 0 {
            (1, 0)
        } else {
            (-1, num_bands - 1)
        };
        while k2_diff != 0 {
            vdk[k as usize] -= incr;
            k += incr;
            k2_diff += incr;
        }
        master.push(k0);
        for d in vdk {
            let last = *master.last().unwrap() as i64;
            master.push((last + d) as usize);
        }
    } else {
        let bands = [12.0f64, 10.0, 8.0][h.freq_scale as usize - 1];
        let warp = if h.alter_scale { 1.3 } else { 1.0 };
        let two_regions = (k2 as f64 / k0 as f64) > 2.2449;
        let k1 = if two_regions { 2 * k0 } else { k2 };
        let n0 = 2 * nint(bands * (k1 as f64 / k0 as f64).ln() / (2.0 * 2f64.ln()));
        if n0 <= 0 {
            return Err(DecodeError::InvalidData("SBR master bands"));
        }
        let dk0 = geometric_dk(k0, k1, n0 as usize);
        if dk0.iter().any(|&d| d <= 0) {
            return Err(DecodeError::InvalidData("SBR master band width"));
        }
        master.push(k0);
        for d in &dk0 {
            let last = *master.last().unwrap() as i64;
            master.push((last + d) as usize);
        }
        if two_regions {
            let n1 = 2 * nint(bands * (k2 as f64 / k1 as f64).ln() / (2.0 * 2f64.ln() * warp));
            if n1 > 0 {
                let mut dk1 = geometric_dk(k1, k2, n1 as usize);
                if dk1[0] < dk0[dk0.len() - 1] {
                    let change = dk0[dk0.len() - 1] - dk1[0];
                    dk1[0] += change;
                    let l = dk1.len() - 1;
                    dk1[l] -= change;
                    dk1.sort_unstable();
                }
                if dk1.iter().any(|&d| d <= 0) {
                    return Err(DecodeError::InvalidData("SBR master band width"));
                }
                for d in &dk1 {
                    let last = *master.last().unwrap() as i64;
                    master.push((last + d) as usize);
                }
            }
        }
    }
    let n_master = master.len() - 1;
    if (h.xover_band as usize) >= n_master {
        return Err(DecodeError::InvalidData("SBR xover band"));
    }
    let high: Vec<usize> = master[h.xover_band as usize..].to_vec();
    let n_high = high.len() - 1;
    let n_low = (n_high >> 1) + (n_high - ((n_high >> 1) << 1));
    let odd = n_high & 1;
    let mut low = Vec::with_capacity(n_low + 1);
    low.push(high[0]);
    for k in 1..=n_low {
        low.push(high[2 * k - odd]);
    }
    let kx = high[0];
    let m = high[n_high] - high[0];
    if kx > 32 || kx + m > 64 || m == 0 {
        return Err(DecodeError::InvalidData("SBR kx / M"));
    }
    let nq = (h.noise_bands as f64 * (k2 as f64 / kx as f64).ln() / 2f64.ln()).max(0.0);
    let n_q = nint(nq).max(1) as usize;
    if n_q > 5 {
        return Err(DecodeError::InvalidData("SBR noise bands"));
    }
    let mut noise = Vec::with_capacity(n_q + 1);
    let mut i = 0usize;
    noise.push(low[0]);
    for k in 1..=n_q {
        i += (n_low - i) / (n_q + 1 - k);
        noise.push(low[i]);
    }
    // patches
    let goal_sb = nint(2.048e6 / fs as f64) as usize;
    let mut msb = k0;
    let mut usb = kx;
    let mut patch_num = Vec::new();
    let mut patch_start = Vec::new();
    let mut k = if goal_sb < kx + m {
        let mut i = 0;
        while master[i] < goal_sb {
            i += 1;
        }
        i
    } else {
        n_master
    };
    let mut guard = 0;
    loop {
        let mut j = k + 1;
        let mut sb;
        let mut odd;
        loop {
            j -= 1;
            sb = master[j];
            odd = (sb + k0 - 2) % 2;
            if sb <= k0 - 1 + msb - odd || j == 0 {
                break;
            }
        }
        let num = sb.saturating_sub(usb);
        patch_num.push(num);
        patch_start.push((k0 as isize - odd as isize - num as isize).max(0) as usize);
        if num > 0 {
            usb = sb;
            msb = sb;
        } else {
            patch_num.pop();
            patch_start.pop();
            msb = kx;
        }
        if master[k] - sb < 3 {
            k = n_master;
        }
        guard += 1;
        if sb == kx + m || guard > 64 {
            break;
        }
    }
    if patch_num.len() > 1 && *patch_num.last().unwrap() < 3 {
        patch_num.pop();
        patch_start.pop();
    }
    // The standard limits the patch count to 5, but the Coding Technologies decoder check
    // streams (CT_DecoderCheck) need 6 and the reference decoders accept them.
    if patch_num.is_empty() || patch_num.len() > 6 {
        return Err(DecodeError::InvalidData("SBR patches"));
    }
    // limiter table
    let lim = limiter_table(h.limiter_bands, &low, kx, &patch_num);
    let n_lim = lim.len() - 1;
    Ok(FreqTables {
        k0,
        k2,
        kx,
        m,
        master,
        high,
        low,
        noise,
        lim,
        n_high,
        n_low,
        n_q,
        n_lim,
        patch_num,
        patch_start,
    })
}

pub(crate) fn limiter_table(
    limiter_bands: u8,
    low: &[usize],
    kx: usize,
    patch_num: &[usize],
) -> Vec<usize> {
    let n_low = low.len() - 1;
    if limiter_bands == 0 {
        return vec![low[0], low[n_low]];
    }
    let lim_bands = [1.2f64, 2.0, 3.0][limiter_bands as usize - 1];
    let mut borders = vec![kx];
    for &n in patch_num {
        let l = *borders.last().unwrap();
        borders.push(l + n);
    }
    let mut t: Vec<usize> = low.to_vec();
    t.extend_from_slice(&borders[1..borders.len() - 1]);
    t.sort_unstable();
    let is_border = |v: usize| borders.contains(&v);
    let mut k = 1;
    while k < t.len() {
        let n_oct = (t[k] as f64 / t[k - 1] as f64).log2();
        if n_oct * lim_bands < 0.49 {
            if t[k] == t[k - 1] {
                t.remove(k);
            } else if is_border(t[k]) {
                if is_border(t[k - 1]) {
                    k += 1;
                } else {
                    t.remove(k - 1);
                }
            } else {
                t.remove(k);
            }
        } else {
            k += 1;
        }
    }
    t
}
