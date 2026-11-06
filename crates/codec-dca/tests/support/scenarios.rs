//! Synthetic DTS core stream scenarios (development tooling and tests; not production
//! code). Each scenario is a deterministic stream used to compare this decoder with the
//! FFmpeg oracle (`harness/codecs/dca/run_all.sh`) or to recover codebooks
//! (`harness/codecs/dca/recover_vq.py`, `recover_pr.py`).
#![allow(dead_code, clippy::needless_range_loop, clippy::too_many_arguments)]

use super::synth::*;
use triergon_codec_dca::Packing;

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % ((hi - lo + 1) as u64)) as i32
    }
}

fn amode_channels(amode: u8) -> usize {
    [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8][amode as usize]
}

#[derive(Clone, Copy)]
struct Opts {
    amode: u8,
    lff: u8,
    filts: bool,
    blocks: usize,
    nssc: &'static [usize],
    huff: bool,
    transients: bool,
    block_codes: bool,
    big_abits: bool,
    aspf: bool,
    seed: u64,
    post: fn(&mut Frame, &mut Rng),
}

fn no_post(_: &mut Frame, _: &mut Rng) {}

/// Impulse response of an ADPCM codebook entry stays bounded.
fn stable(pvq: usize) -> bool {
    let c = triergon_codec_dca::vq::ADPCM_VQ[pvq].map(|v| f64::from(v) / 8192.0);
    let mut x = [0f64; 96];
    x[4] = 1.0;
    for t in 5..96 {
        x[t] = c[0] * x[t - 1] + c[1] * x[t - 2] + c[2] * x[t - 3] + c[3] * x[t - 4];
    }
    x.iter().all(|v| v.abs() < 4.0) && x[80..].iter().all(|v| v.abs() < 0.5)
}

fn post_adpcm(f: &mut Frame, rng: &mut Rng) {
    for sf in &mut f.subframes {
        for (ch, cs) in sf.chans.iter_mut().enumerate() {
            let nsubs = f.chans[ch].nvqsub as usize;
            for b in cs.bands.iter_mut().take(nsubs) {
                if rng.range(0, 2) == 0 {
                    loop {
                        let v = rng.range(0, 4095) as usize;
                        if stable(v) {
                            b.pmode = true;
                            b.pvq = v as u16;
                            break;
                        }
                    }
                    // keep the predicted level modest
                    for sc in &mut b.scale {
                        *sc = (*sc - 12).max(1);
                    }
                }
            }
        }
    }
}

fn post_hfvq(f: &mut Frame, rng: &mut Rng) {
    let nch = f.chans.len();
    for ch in 0..nch {
        let c = &mut f.chans[ch];
        c.nvqsub = c.nsubs - rng.range(1, 8) as u8;
    }
    for sf in &mut f.subframes {
        for ch in 0..nch {
            let (nv, ns, shuff) = (
                f.chans[ch].nvqsub as usize,
                f.chans[ch].nsubs as usize,
                f.chans[ch].shuff,
            );
            for n in nv..ns {
                let b = &mut sf.chans[ch].bands[n];
                b.vq = rng.range(0, 1023) as u16;
                b.scale[0] = if shuff == 6 {
                    rng.range(40, 100)
                } else {
                    rng.range(20, 50)
                };
            }
        }
    }
}

fn post_joint(f: &mut Frame, rng: &mut Rng) {
    // channel 2 copies from channel 1, channel 4 from channel 3 (core coded order)
    let nch = f.chans.len();
    let pairs: &[(usize, usize)] = if nch >= 5 {
        &[(2, 1), (4, 3)]
    } else {
        &[(1, 0)]
    };
    for &(ch, src) in pairs {
        f.chans[src].nsubs = 32;
        f.chans[src].nvqsub = 32;
        let n = rng.range(8, 24) as u8;
        f.chans[ch].nsubs = n;
        f.chans[ch].nvqsub = n;
        f.chans[ch].joinx = src as u8 + 1;
    }
    for sf in &mut f.subframes {
        for &(ch, src) in pairs {
            sf.chans[ch].join_shuff = rng.range(0, 4) as u8;
            let _ = src;
            for b in &mut sf.chans[ch].bands {
                b.jscale = rng.range(-20, 20);
            }
        }
        // newly active bands need content
        for &(ch, src) in pairs {
            let nssc = sf.nssc;
            let nj = f.chans[ch].nsubs as usize;
            let mut fill = |b: &mut Band| {
                if b.q.len() != nssc * 8 {
                    b.q = vec![0; nssc * 8];
                }
                if b.abits == 0 {
                    b.abits = 3;
                    b.scale[0] = 60;
                    for v in &mut b.q {
                        *v = rng.range(-3, 3);
                    }
                }
            };
            for b in &mut sf.chans[src].bands {
                fill(b);
            }
            for b in sf.chans[ch].bands.iter_mut().take(nj) {
                fill(b);
            }
        }
    }
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            amode: 9,
            lff: 2,
            filts: false,
            blocks: 16,
            nssc: &[2],
            huff: false,
            transients: false,
            block_codes: false,
            big_abits: false,
            aspf: false,
            seed: 1,
            post: no_post,
        }
    }
}

/// Random but valid frame content.
fn random_frame(o: &Opts, rng: &mut Rng) -> Frame {
    let nch = amode_channels(o.amode).min(5);
    let mut f = Frame::new(o.amode, nch, o.blocks);
    f.lff = o.lff;
    f.filts = o.filts;
    f.aspf = o.aspf;
    for c in &mut f.chans {
        if o.huff {
            c.bhuff = rng.range(0, 4) as u8;
            c.shuff = rng.range(0, 4) as u8;
            c.thuff = rng.range(0, 3) as u8;
            for n in 0..10 {
                let a = n as u32 + 1;
                c.sel[n] = rng.range(0, huffman_sel_count(a) as i32 - 1) as u8;
                c.adj[n] = rng.range(0, 3) as u8;
            }
        } else if o.block_codes {
            c.bhuff = 5;
            c.shuff = 5;
        }
        c.nsubs = rng.range(20, 32) as u8;
        c.nvqsub = c.nsubs;
    }
    for &nssc in o.nssc {
        let lff = f.lff;
        let chans = f.chans.clone();
        let sf = f.silent_subframe(nssc);
        for (ch, cs) in sf.chans.iter_mut().enumerate() {
            let c = &chans[ch];
            let mut prev = 20;
            for n in 0..c.nsubs as usize {
                let b = &mut cs.bands[n];
                let a = if o.huff {
                    rng.range(1, 10)
                } else if o.block_codes {
                    rng.range(0, 7)
                } else if o.big_abits {
                    rng.range(8, 26)
                } else {
                    rng.range(0, 14)
                };
                b.abits = a as u8;
                if o.transients && nssc > 1 && a > 0 {
                    b.tmode = rng.range(0, nssc as i32 - 1) as u8;
                }
                // Keep the reconstructed level moderate: lower scale for higher ABITS.
                let table_max = if c.shuff == 6 { 124 } else { 62 };
                let pick = |rng: &mut Rng, prev: i32| -> i32 {
                    let lo = if c.shuff == 6 { 30 } else { 15 };
                    let hi = if c.shuff == 6 { 110 } else { 55 };
                    let mut v = prev + rng.range(-8, 8);
                    if v < lo {
                        v = lo;
                    }
                    if v > hi.min(table_max) {
                        v = hi.min(table_max);
                    }
                    v
                };
                b.scale[0] = pick(rng, prev);
                prev = b.scale[0];
                if b.tmode > 0 {
                    b.scale[1] = pick(rng, prev);
                    prev = b.scale[1];
                }
                let a = a as u32;
                let maxq: i32 = if a == 0 {
                    0
                } else if a <= 7 {
                    (levels(a) - 1) / 2
                } else if a <= 10 && o.huff {
                    [16, 32, 64][a as usize - 8]
                } else {
                    (1 << (a - 4)) - 1
                };
                let maxq = if a >= 11 { maxq.min(1 << 12) } else { maxq };
                for v in &mut b.q {
                    *v = rng.range(-maxq, maxq);
                }
            }
        }
        for v in &mut sf.lfe {
            *v = rng.range(-120, 120);
        }
        sf.lfe_scale = rng.range(40, 100) as u8;
        let _ = lff;
    }
    (o.post)(&mut f, rng);
    // Scale magnitudes: keep large-ABITS amplitudes moderate.
    for sf in &mut f.subframes {
        for (ch, cs) in sf.chans.iter_mut().enumerate() {
            let shuff = f.chans[ch].shuff;
            for b in &mut cs.bands {
                if b.abits >= 11 {
                    for s in &mut b.scale {
                        *s = (*s - if shuff == 6 { 30 } else { 15 }).max(1);
                    }
                }
            }
        }
    }
    f
}

#[derive(Clone, Copy)]
struct X96Opts {
    revno: u8,
    highres: bool,
    huff: bool,
    vq: bool,
    zero: bool,
    adpcm: bool,
    joint: bool,
    loud: bool,
    fixed_scale: i32,
}

impl Default for X96Opts {
    fn default() -> Self {
        Self {
            revno: 1,
            highres: false,
            huff: true,
            vq: false,
            zero: false,
            adpcm: false,
            joint: false,
            loud: false,
            fixed_scale: 0,
        }
    }
}

/// Random core frame (AMODE 9) carrying a random DTS_BCCORE_X96 extension.
fn x96_frame(o: &Opts, xo: &X96Opts, rng: &mut Rng) -> Frame {
    let mut f = random_frame(o, rng);
    let nch = f.chans.len();
    let maxa: i32 = if xo.highres { 15 } else { 7 };
    let sbs = if xo.revno < 8 {
        rng.range(0, 27) as u8
    } else {
        32
    };
    let mut chans = Vec::new();
    for _ in 0..nch {
        let mut c = X96Chan {
            sbe: rng.range(sbs.max(32) as i32, 64) as u8,
            joinx: 0,
            shuff: 5,
            bhuff: 7,
            sel: [1, 3, 3, 3, 3, 7, 7, 7, 7, 7],
        };
        if xo.huff {
            c.shuff = rng.range(0, 4) as u8;
            c.bhuff = rng.range(0, 6) as u8;
            for n in 0..10 {
                let g = n as u32 + 1;
                c.sel[n] = rng.range(0, huffman_sel_count(g) as i32 - 1) as u8;
            }
        }
        chans.push(c);
    }
    if xo.joint {
        chans[0].sbe = 64;
        chans[2].joinx = 1;
        chans[2].sbe = 40;
    }
    let nsscs: Vec<usize> = f.subframes.iter().map(|s| s.nssc).collect();
    let mut subs = Vec::new();
    for &ns in &nsscs {
        let mut bands = vec![vec![X96Band::default(); 64]; nch];
        let mut join_shuff = vec![0u8; nch];
        for ch in 0..nch {
            join_shuff[ch] = rng.range(0, 4) as u8;
            for n in 0..64 {
                let b = &mut bands[ch][n];
                b.jscale = rng.range(-10, 10);
                if n < sbs as usize || n >= chans[ch].sbe as usize {
                    continue;
                }
                let lo = if xo.zero {
                    0
                } else if xo.vq {
                    1
                } else {
                    2
                };
                let a = rng.range(lo, maxa);
                b.abits = a as u8;
                b.scale = if xo.fixed_scale > 0 {
                    xo.fixed_scale
                } else if xo.loud {
                    rng.range(50, 62)
                } else {
                    rng.range(5, 35)
                };
                let g = (a - 1).max(0) as u32;
                let maxq: i32 = if a < 2 {
                    0
                } else if g <= 7 {
                    (levels(g) - 1) / 2
                } else if g <= 10 && xo.huff {
                    [16, 32, 64][g as usize - 8]
                } else {
                    (1 << (g - 4)) - 1
                };
                b.q = (0..ns * 8).map(|_| rng.range(-maxq, maxq)).collect();
                b.vq = (0..(ns * 8).div_ceil(16))
                    .map(|_| rng.range(0, 1023) as u16)
                    .collect();
                if xo.adpcm && rng.range(0, 2) == 0 {
                    loop {
                        let v = rng.range(0, 4095) as usize;
                        if stable(v) {
                            b.pmode = true;
                            b.pvq = v as u16;
                            break;
                        }
                    }
                }
            }
        }
        subs.push(X96Sub { bands, join_shuff });
    }
    let x = X96 {
        revno: xo.revno,
        highres: xo.highres,
        sbs,
        chans,
        subs,
    };
    f.ext = write_x96(&x, &nsscs, f.aspf, f.cpf);
    assert!(
        f.ext.len() <= 4096,
        "X96 payload {} exceeds FSIZE96",
        f.ext.len()
    );
    f.ext_audio = true;
    f.ext_audio_id = 2;
    f
}

fn x96_stream(o: Opts, xo: X96Opts, count: usize) -> Vec<u8> {
    let mut rng = Rng(0xABCD_EF01 ^ o.seed);
    let frames: Vec<Frame> = (0..count).map(|_| x96_frame(&o, &xo, &mut rng)).collect();
    stream(&frames)
}

fn stream(frames: &[Frame]) -> Vec<u8> {
    frames.iter().flat_map(write_frame).collect()
}

fn random_stream(o: Opts, count: usize) -> Vec<u8> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ o.seed);
    let frames: Vec<Frame> = (0..count).map(|_| random_frame(&o, &mut rng)).collect();
    stream(&frames)
}

fn silent(amode: u8, blocks: usize, nssc: usize, lff: u8) -> Frame {
    let nch = amode_channels(amode).min(5);
    let mut f = Frame::new(amode, nch, blocks);
    f.lff = lff;
    for c in &mut f.chans {
        c.nsubs = 32;
        c.nvqsub = 32;
    }
    for _ in 0..blocks / (nssc * 8) {
        f.silent_subframe(nssc);
    }
    f
}

/// Bands used per channel for codebook recovery.
const PROBE_BANDS: [usize; 4] = [2, 10, 18, 26];
const VQ_BANDS: [usize; 4] = [28, 29, 30, 31];
const PROBE_SCALE: i32 = 100; // 7-bit index
const PROBE_ABITS: u8 = 12; // linear 9 bits
const PROBE_Q: i32 = 200;
const ADPCM_Q: i32 = 100;

/// Codebook recovery streams. Layout: for each probe band a calibration episode (an
/// impulse q at t = 0 through the ordinary quantizer, no prediction), then test episodes.
/// Every episode is one test frame followed by `episode - 1` silent frames, so the
/// 512-tap synthesis tail never overlaps the next episode.
fn probe_stream(
    meta: &mut String,
    kind: &str,
    blocks: usize,
    nssc: usize,
    bands: &[usize],
    episode: usize,
    count: usize,
    q: i32,
    mut fill: impl FnMut(&mut Frame, usize),
) -> Vec<u8> {
    let sil = || {
        let mut f = silent(9, blocks, nssc, 0);
        f.hflag = false;
        f
    };
    let mut frames = vec![sil(), sil()];
    let first_cal = frames.len();
    for &band in bands {
        let mut f = sil();
        for ch in 0..5 {
            let b = &mut f.subframes[0].chans[ch].bands[band];
            b.abits = PROBE_ABITS;
            b.scale[0] = PROBE_SCALE;
            b.q[0] = q;
        }
        frames.push(f);
        for _ in 1..episode {
            frames.push(sil());
        }
    }
    let first_test = frames.len();
    let per_frame = 5 * bands.len();
    let mut idx = 0;
    while idx < count {
        let mut f = sil();
        fill(&mut f, idx);
        frames.push(f);
        for _ in 1..episode {
            frames.push(sil());
        }
        idx += per_frame;
    }
    meta.push_str(&format!(
        "kind={kind}\nframe_samples={}\nfirst_cal={first_cal}\nfirst_test={first_test}\nepisode={episode}\nper_frame={per_frame}\ncount={count}\nbands={}\nscale7={PROBE_SCALE}\nabits={PROBE_ABITS}\nq={q}\nsamples={}\n",
        blocks * 32,
        bands.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        nssc * 8,
    ));
    stream(&frames)
}

/// ADPCM codebook recovery: an impulse with prediction enabled in PROBE_BANDS of 5 channels.
fn adpcm_probe(meta: &mut String) -> Vec<u8> {
    // Small impulse: FFmpeg saturates reconstructed subband samples at 2^23, which must
    // not happen within the first five samples used for recovery.
    probe_stream(
        meta,
        "adpcm",
        8,
        1,
        &PROBE_BANDS,
        3,
        4096,
        ADPCM_Q,
        |f, start| {
            for ch in 0..5 {
                for (k, &band) in PROBE_BANDS.iter().enumerate() {
                    let v = start + ch * PROBE_BANDS.len() + k;
                    let b = &mut f.subframes[0].chans[ch].bands[band];
                    b.abits = PROBE_ABITS;
                    b.scale[0] = PROBE_SCALE;
                    b.q[0] = ADPCM_Q;
                    if v < 4096 {
                        b.pmode = true;
                        b.pvq = v as u16;
                    }
                }
            }
        },
    )
}

/// HF VQ codebook recovery: 32-block frames, nssc = 4, VQ in bands 28..31 of 5 channels.
fn hfvq_probe(meta: &mut String) -> Vec<u8> {
    probe_stream(
        meta,
        "hfvq",
        32,
        4,
        &VQ_BANDS,
        2,
        1024,
        PROBE_Q,
        |f, start| {
            for c in &mut f.chans {
                c.nvqsub = VQ_BANDS[0] as u8;
            }
            for ch in 0..5 {
                for (k, &band) in VQ_BANDS.iter().enumerate() {
                    let b = &mut f.subframes[0].chans[ch].bands[band];
                    b.scale[0] = PROBE_SCALE;
                    b.vq = ((start + ch * VQ_BANDS.len() + k) % 1024) as u16;
                }
            }
        },
    )
}

pub fn scenarios(name: &str, meta: &mut String) -> Option<Vec<u8>> {
    let d = Opts::default();
    Some(match name {
        "basic" => random_stream(d, 12),
        "perfect" => random_stream(Opts { filts: true, ..d }, 12),
        "lfe128" => random_stream(Opts { lff: 1, ..d }, 12),
        "nolfe" => random_stream(Opts { lff: 0, ..d }, 12),
        "huff" => random_stream(
            Opts {
                huff: true,
                nssc: &[4, 2, 1, 1],
                blocks: 64,
                transients: true,
                ..d
            },
            12,
        ),
        "huff_aspf" => random_stream(
            Opts {
                huff: true,
                nssc: &[3, 1],
                blocks: 32,
                transients: true,
                aspf: true,
                ..d
            },
            12,
        ),
        "block" => random_stream(
            Opts {
                block_codes: true,
                nssc: &[4, 4],
                blocks: 64,
                transients: true,
                ..d
            },
            12,
        ),
        "bigabits" => random_stream(
            Opts {
                big_abits: true,
                ..d
            },
            12,
        ),
        "blocks128" => random_stream(
            Opts {
                blocks: 128,
                nssc: &[4, 4, 4, 4],
                transients: true,
                ..d
            },
            6,
        ),
        "lfe128_short" => random_stream(
            Opts {
                blocks: 8,
                nssc: &[1],
                lff: 1,
                ..d
            },
            12,
        ),
        "lfe_mixed" => {
            let mut rng = Rng(77);
            let mut frames = Vec::new();
            for (i, &(blocks, nssc)) in [
                (16, &[2usize][..]),
                (8, &[1][..]),
                (8, &[1][..]),
                (32, &[4][..]),
                (8, &[1][..]),
                (24, &[3][..]),
                (16, &[1, 1][..]),
            ]
            .iter()
            .enumerate()
            {
                let _ = i;
                frames.push(random_frame(&Opts { blocks, nssc, ..d }, &mut rng));
            }
            stream(&frames)
        }
        "size_changes_adpcm" => {
            let mut rng = Rng(99);
            let mut frames = Vec::new();
            for &(blocks, nssc) in &[
                (16, &[2usize][..]),
                (8, &[1][..]),
                (8, &[1][..]),
                (32, &[4][..]),
                (8, &[1][..]),
                (24, &[3][..]),
                (16, &[1, 1][..]),
                (48, &[4, 2][..]),
                (16, &[2][..]),
            ] {
                frames.push(random_frame(
                    &Opts {
                        blocks,
                        nssc,
                        post: post_adpcm,
                        lff: 0,
                        ..d
                    },
                    &mut rng,
                ));
            }
            stream(&frames)
        }
        "x96_loud" => x96_stream(
            Opts {
                post: |f, _| {
                    for sf in &mut f.subframes {
                        for cs in &mut sf.chans {
                            for b in &mut cs.bands {
                                b.scale[0] = (b.scale[0] + 20).min(124);
                                b.scale[1] = (b.scale[1] + 20).min(124);
                            }
                        }
                    }
                },
                ..d
            },
            X96Opts {
                loud: true,
                revno: 1,
                ..Default::default()
            },
            8,
        ),
        "x96_loudcore" => x96_stream(
            Opts {
                post: |f, _| {
                    for sf in &mut f.subframes {
                        for cs in &mut sf.chans {
                            for b in &mut cs.bands {
                                b.scale[0] = (b.scale[0] + 20).min(124);
                                b.scale[1] = (b.scale[1] + 20).min(124);
                            }
                        }
                    }
                },
                ..d
            },
            X96Opts::default(),
            8,
        ),
        "x96_loudres" => x96_stream(
            d,
            X96Opts {
                loud: true,
                ..Default::default()
            },
            8,
        ),
        "x96_s40" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 40,
                ..Default::default()
            },
            4,
        ),
        "x96_s45" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 45,
                ..Default::default()
            },
            4,
        ),
        "x96_s50" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 50,
                ..Default::default()
            },
            4,
        ),
        "x96_s55" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 55,
                ..Default::default()
            },
            4,
        ),
        "x96_s60" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 60,
                ..Default::default()
            },
            4,
        ),
        "x96_s58" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 58,
                ..Default::default()
            },
            4,
        ),
        "x96_s61" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 61,
                ..Default::default()
            },
            4,
        ),
        "x96_s62" => x96_stream(
            d,
            X96Opts {
                fixed_scale: 62,
                ..Default::default()
            },
            4,
        ),
        "x96_rev9" => x96_stream(
            d,
            X96Opts {
                revno: 9,
                ..Default::default()
            },
            4,
        ),
        "x96_rev0" => x96_stream(
            d,
            X96Opts {
                revno: 0,
                ..Default::default()
            },
            4,
        ),
        "x96_basic" => x96_stream(d, X96Opts::default(), 8),
        "x96_lin" => x96_stream(
            d,
            X96Opts {
                huff: false,
                ..Default::default()
            },
            8,
        ),
        "x96_hi" => x96_stream(
            d,
            X96Opts {
                highres: true,
                ..Default::default()
            },
            8,
        ),
        "x96_hi_lin" => x96_stream(
            d,
            X96Opts {
                highres: true,
                huff: false,
                ..Default::default()
            },
            8,
        ),
        "x96_rev8" => x96_stream(
            d,
            X96Opts {
                revno: 8,
                ..Default::default()
            },
            8,
        ),
        "x96_vq" => x96_stream(
            Opts {
                nssc: &[4, 3, 1],
                blocks: 64,
                amode: 2,
                ..d
            },
            X96Opts {
                vq: true,
                ..Default::default()
            },
            8,
        ),
        "x96_zero" => x96_stream(
            d,
            X96Opts {
                zero: true,
                ..Default::default()
            },
            8,
        ),
        "x96_adpcm" => x96_stream(
            Opts {
                nssc: &[2, 2],
                blocks: 32,
                amode: 5,
                ..d
            },
            X96Opts {
                adpcm: true,
                ..Default::default()
            },
            8,
        ),
        "x96_joint" => x96_stream(
            d,
            X96Opts {
                joint: true,
                ..Default::default()
            },
            8,
        ),
        "x96_aspf" => x96_stream(
            Opts {
                aspf: true,
                nssc: &[3, 1],
                blocks: 32,
                ..d
            },
            X96Opts::default(),
            8,
        ),
        "x96_lfe128" => x96_stream(Opts { lff: 1, ..d }, X96Opts::default(), 8),
        "x96_scale_probe" => {
            // mono, residual band 20 impulse (ABITS96 13, q = 20) with 6-bit scale index k
            let mk = |k: i32, on: bool| {
                let mut f = silent(0, 16, 2, 0);
                let mut bands = vec![vec![X96Band::default(); 64]];
                for n in 0..32 {
                    bands[0][n].q = vec![0; 16];
                }
                let b = &mut bands[0][20];
                b.abits = 13;
                b.scale = k;
                if on {
                    b.q[0] = 20;
                }
                let x = X96 {
                    revno: 1,
                    highres: true,
                    sbs: 0,
                    chans: vec![X96Chan {
                        sbe: 32,
                        joinx: 0,
                        shuff: 5,
                        bhuff: 7,
                        sel: [0; 10],
                    }],
                    subs: vec![X96Sub {
                        bands,
                        join_shuff: vec![0],
                    }],
                };
                f.ext = write_x96(&x, &[2], false, false);
                f.ext_audio = true;
                f.ext_audio_id = 2;
                f
            };
            let mut frames = vec![mk(0, false)];
            for k in 50..63 {
                frames.push(mk(k, true));
                frames.push(mk(0, false));
            }
            stream(&frames)
        }
        "x96_noise_probe" => {
            // mono; X96 residual bands 0..32: all ABIT 0 with scale index 0 except band 20.
            // frame 1: calibration impulse (ABIT 13 linear) in band 20; frames 3..: noise.
            let mk = |cal: bool| {
                let mut f = silent(0, 16, 2, 0);
                f.hflag = false;
                let nss = [2usize];
                let mut bands = vec![vec![X96Band::default(); 64]];
                for n in 0..32 {
                    let b = &mut bands[0][n];
                    b.abits = 0;
                    b.scale = 0;
                    b.q = vec![0; 16];
                }
                let b = &mut bands[0][20];
                if cal {
                    b.abits = 13; // group 12: 9-bit linear
                    b.scale = 40;
                    b.q[0] = 200;
                } else {
                    b.scale = 40;
                }
                let x = X96 {
                    revno: 1,
                    highres: true,
                    sbs: 0,
                    chans: vec![X96Chan {
                        sbe: 32,
                        joinx: 0,
                        shuff: 5,
                        bhuff: 7,
                        sel: [0; 10],
                    }],
                    subs: vec![X96Sub {
                        bands,
                        join_shuff: vec![0],
                    }],
                };
                f.ext = write_x96(&x, &nss, false, false);
                f.ext_audio = true;
                f.ext_audio_id = 2;
                f
            };
            let mut frames = vec![mk(false), mk(true), mk(false)];
            for _ in 0..16 {
                frames.push(mk(false));
            }
            stream(&frames)
        }
        "blocks256s" => random_stream(
            Opts {
                blocks: 8,
                nssc: &[1],
                ..d
            },
            12,
        ),
        "adpcm_probe" => adpcm_probe(meta),
        "adpcm" => random_stream(
            Opts {
                post: post_adpcm,
                nssc: &[2, 1, 1],
                blocks: 32,
                ..d
            },
            12,
        ),
        "adpcm_h0" => random_stream(
            Opts {
                post: |f, r| {
                    post_adpcm(f, r);
                    f.hflag = false;
                },
                ..d
            },
            12,
        ),
        "hfvq" => random_stream(
            Opts {
                post: post_hfvq,
                nssc: &[4, 4],
                blocks: 64,
                ..d
            },
            12,
        ),
        "hfvq_loud" => random_stream(
            Opts {
                post: |f, r| {
                    post_hfvq(f, r);
                    for sf in &mut f.subframes {
                        for (ch, cs) in sf.chans.iter_mut().enumerate() {
                            let _ = ch;
                            for b in &mut cs.bands {
                                if b.vq != 0 {
                                    b.scale[0] = r.range(110, 124);
                                }
                            }
                        }
                    }
                },
                nssc: &[4],
                blocks: 32,
                ..d
            },
            12,
        ),
        "hfvq_short" => random_stream(
            Opts {
                post: post_hfvq,
                nssc: &[1, 3],
                blocks: 32,
                huff: true,
                ..d
            },
            12,
        ),
        "joint" => random_stream(
            Opts {
                post: post_joint,
                ..d
            },
            12,
        ),
        "joint_huff" => random_stream(
            Opts {
                post: post_joint,
                huff: true,
                ..d
            },
            12,
        ),
        "joint_h_nosel" => random_stream(
            Opts {
                post: |f, r| {
                    post_joint(f, r);
                    for c in &mut f.chans {
                        c.sel = [1, 3, 3, 3, 3, 7, 7, 7, 7, 7];
                    }
                },
                huff: true,
                ..d
            },
            12,
        ),
        "joint_h_lin_abits" => random_stream(
            Opts {
                post: |f, r| {
                    post_joint(f, r);
                    for c in &mut f.chans {
                        c.bhuff = 6;
                    }
                },
                huff: true,
                ..d
            },
            12,
        ),
        "joint_h_lin_scale" => random_stream(
            Opts {
                post: |f, r| {
                    post_joint(f, r);
                    for c in &mut f.chans {
                        c.shuff = 6;
                    }
                },
                huff: true,
                ..d
            },
            12,
        ),
        "joint_shuff_h" => random_stream(
            Opts {
                post: |f, r| {
                    post_joint(f, r);
                    for c in &mut f.chans {
                        c.shuff = 2;
                    }
                    for sf in &mut f.subframes {
                        for cs in &mut sf.chans {
                            for b in &mut cs.bands {
                                b.scale[0] /= 2;
                                b.scale[1] /= 2;
                            }
                        }
                    }
                },
                ..d
            },
            12,
        ),
        "joint_shuff_lin6" => random_stream(
            Opts {
                post: |f, r| {
                    post_joint(f, r);
                    for c in &mut f.chans {
                        c.shuff = 5;
                    }
                    for sf in &mut f.subframes {
                        for cs in &mut sf.chans {
                            for b in &mut cs.bands {
                                b.scale[0] /= 2;
                                b.scale[1] /= 2;
                            }
                        }
                    }
                },
                ..d
            },
            12,
        ),
        "joint_jlin" => random_stream(
            Opts {
                post: |f, r| {
                    post_joint(f, r);
                    for sf in &mut f.subframes {
                        for cs in &mut sf.chans {
                            cs.join_shuff = 5;
                            for b in &mut cs.bands {
                                b.jscale = r.range(0, 20);
                            }
                        }
                    }
                },
                ..d
            },
            12,
        ),
        "adpcm_ovf" => {
            // mono band 2: calibration impulse, then PVQ 3916 impulses of growing size
            let sil = || {
                let mut f = silent(0, 8, 1, 0);
                f.hflag = false;
                f
            };
            let mut frames = vec![sil(), sil()];
            for (k, q) in [200, 50, 100, 150, 180, 190, 200, 220, 240, 255]
                .into_iter()
                .enumerate()
            {
                let mut f = sil();
                let b = &mut f.subframes[0].chans[0].bands[2];
                b.abits = PROBE_ABITS;
                b.scale[0] = PROBE_SCALE;
                b.q[0] = q;
                if k > 0 {
                    b.pmode = true;
                    b.pvq = 3916;
                }
                frames.push(f);
                frames.push(sil());
                frames.push(sil());
            }
            stream(&frames)
        }
        "jscale_probe" => {
            // stereo: ch0 impulse in band 20, ch1 joint (nsubs 8) with JOIN_SCALES code k
            let mk = || {
                let mut f = silent(2, 16, 2, 0);
                f.chans[1].nsubs = 8;
                f.chans[1].nvqsub = 8;
                f.chans[1].joinx = 1;
                f
            };
            let mut frames = vec![mk()];
            for k in -64..=64 {
                let mut f = mk();
                let sf = &mut f.subframes[0];
                let b = &mut sf.chans[0].bands[20];
                b.abits = PROBE_ABITS;
                b.scale[0] = if k <= 0 { 118 } else { 88 };
                b.q[0] = 200;
                sf.chans[1].join_shuff = 0;
                sf.chans[1].bands[20].jscale = k;
                frames.push(f);
                frames.push(mk());
            }
            stream(&frames)
        }
        "scalehigh" => {
            // slow walk to the top of the 6-bit table, per channel ending at 58..62
            let mut frames = Vec::new();
            for book in [0u8, 5] {
                let mut f = silent(9, 16, 2, 0);
                for c in &mut f.chans {
                    c.shuff = book;
                }
                for ch in 0..5 {
                    let top = 58 + ch as i32;
                    for n in 0..32 {
                        let b = &mut f.subframes[0].chans[ch].bands[n];
                        b.abits = 2;
                        b.scale[0] = (top - (31 - n as i32) * 2).max(0);
                        b.q = (0..16)
                            .map(|t| [(1), (-1), (2), (0)][(t + n) % 4])
                            .collect();
                    }
                }
                frames.push(f);
            }
            stream(&frames)
        }
        "scalebooks" => {
            // Every scale-factor difference -62..=62 through each of SA129..SE129.
            let mut walk = vec![0i32];
            for d in (1..=62).rev() {
                walk.push(d);
                walk.push(0);
            }
            let mut frames = Vec::new();
            for book in 0..5u8 {
                let mut pos = 0;
                while pos < walk.len() {
                    let mut f = silent(9, 16, 2, 0);
                    for c in &mut f.chans {
                        c.shuff = book;
                    }
                    for ch in 0..5 {
                        for n in 0..32 {
                            let b = &mut f.subframes[0].chans[ch].bands[n];
                            b.abits = 2;
                            b.scale[0] = walk[(pos + ch * 32 + n).min(walk.len() - 1)];
                            b.q = (0..16)
                                .map(|t| [(1), (-1), (2), (0)][(t + n) % 4])
                                .collect();
                        }
                    }
                    pos += 160;
                    frames.push(f);
                }
            }
            stream(&frames)
        }
        "sumf" => random_stream(
            Opts {
                post: |f, _| f.sumf = true,
                ..d
            },
            12,
        ),
        "sums" => random_stream(
            Opts {
                post: |f, _| f.sums = true,
                ..d
            },
            12,
        ),
        "sumf_stereo" => random_stream(
            Opts {
                amode: 2,
                post: |f, _| f.sumf = true,
                ..d
            },
            12,
        ),
        "dynf" => random_stream(
            Opts {
                post: |f, r| {
                    f.dynf = true;
                    for sf in &mut f.subframes {
                        sf.range = r.range(0, 255) as u8;
                    }
                },
                nssc: &[1, 1],
                ..d
            },
            12,
        ),
        "cpf" => random_stream(
            Opts {
                post: |f, _| f.cpf = true,
                ..d
            },
            12,
        ),
        "cpf_dynf" => random_stream(
            Opts {
                post: |f, _| {
                    f.cpf = true;
                    f.dynf = true;
                },
                ..d
            },
            12,
        ),
        "timef_auxf" => random_stream(
            Opts {
                post: |f, _| {
                    f.timef = true;
                    f.auxf = true;
                },
                ..d
            },
            12,
        ),
        "lossless" => random_stream(
            Opts {
                post: |f, _| f.rate = 31,
                ..d
            },
            12,
        ),
        "termination" => random_stream(
            Opts {
                post: |f, r| {
                    f.normal = false;
                    f.short = r.range(0, 30) as u8;
                },
                ..d
            },
            12,
        ),
        "term31" => random_stream(
            Opts {
                post: |f, _| {
                    f.normal = false;
                    f.short = 31;
                },
                ..d
            },
            12,
        ),
        "short5" => random_stream(
            Opts {
                post: |f, _| {
                    f.normal = true;
                    f.short = 5;
                },
                ..d
            },
            12,
        ),
        "psc" => random_stream(
            Opts {
                post: |f, _| {
                    f.subframes[0].psc = 3;
                },
                ..d
            },
            12,
        ),
        "vernum6" => random_stream(
            Opts {
                post: |f, _| {
                    f.vernum = 6;
                    f.dialnorm = 5;
                },
                ..d
            },
            12,
        ),
        "vernum8" => random_stream(
            Opts {
                post: |f, _| f.vernum = 8,
                ..d
            },
            12,
        ),
        "es_flag" => random_stream(
            Opts {
                post: |f, _| f.pcmr = 5,
                ..d
            },
            12,
        ),
        "hfvq_probe" => hfvq_probe(meta),
        _ => {
            if name == "pr_probe" {
                // One episode per band 0..32: mono, FILTS = 1, impulse at t = 0.
                let mut frames = vec![silent(0, 16, 2, 0)];
                for band in 0..32 {
                    let mut f = silent(0, 16, 2, 0);
                    let b = &mut f.subframes[0].chans[0].bands[band];
                    b.abits = PROBE_ABITS;
                    b.scale[0] = 110;
                    b.q[0] = PROBE_Q;
                    frames.push(f);
                    frames.push(silent(0, 16, 2, 0));
                }
                for fr in &mut frames {
                    fr.filts = true;
                }
                meta.push_str(
                    "kind=pr\nframe_samples=512\nfirst=1\nepisode=2\nscale7=110\nabits=12\nq=200\n",
                );
                return Some(stream(&frames));
            }
            if let Some(rest) = name.strip_prefix("imp_f") {
                // imp_f{filts}_b{band}: mono impulse responses of one band.
                let mut it = rest.split("_b");
                let filts = it.next()?.parse::<u8>().ok()? == 1;
                let band: usize = it.next()?.parse().ok()?;
                let mut frames = vec![silent(0, 16, 2, 0)];
                let mut f = silent(0, 16, 2, 0);
                let b = &mut f.subframes[0].chans[0].bands[band];
                b.abits = PROBE_ABITS;
                b.scale[0] = PROBE_SCALE;
                b.q[0] = PROBE_Q;
                frames.push(f);
                frames.push(silent(0, 16, 2, 0));
                frames.push(silent(0, 16, 2, 0));
                for fr in &mut frames {
                    fr.filts = filts;
                }
                return Some(stream(&frames));
            }
            if let Some(v) = name.strip_prefix("sfreq") {
                let code: u8 = v.parse().ok()?;
                return Some(random_stream(
                    Opts {
                        post: match code {
                            1 => |f: &mut Frame, _: &mut Rng| f.sfreq = 1,
                            2 => |f: &mut Frame, _: &mut Rng| f.sfreq = 2,
                            3 => |f: &mut Frame, _: &mut Rng| f.sfreq = 3,
                            6 => |f: &mut Frame, _: &mut Rng| f.sfreq = 6,
                            7 => |f: &mut Frame, _: &mut Rng| f.sfreq = 7,
                            8 => |f: &mut Frame, _: &mut Rng| f.sfreq = 8,
                            11 => |f: &mut Frame, _: &mut Rng| f.sfreq = 11,
                            12 => |f: &mut Frame, _: &mut Rng| f.sfreq = 12,
                            _ => return None,
                        },
                        ..d
                    },
                    6,
                ));
            }
            if let Some(a) = name.strip_prefix("amode") {
                let amode: u8 = a.parse().ok()?;
                return Some(random_stream(Opts { amode, ..d }, 6));
            }
            if let Some(p) = name.strip_prefix("pack_") {
                let to = match p {
                    "be16" => Packing::Be16,
                    "le16" => Packing::Le16,
                    "be14" => Packing::Be14,
                    "le14" => Packing::Le14,
                    _ => return None,
                };
                let mut rng = Rng(0x1234_5678);
                let mut out = Vec::new();
                for _ in 0..8 {
                    out.extend(repack(&write_frame(&random_frame(&d, &mut rng)), to));
                }
                return Some(out);
            }
            return None;
        }
    })
}
