//! Minimal DTS core frame *writer* for tests and oracle experiments (not production code).
//! It emits exactly the syntax of ETSI TS 102 114 Tables 5-1, 5-21, 5-28, 5-29 and 6-17..6-20
//! from explicit field values; it does no analysis or psychoacoustics.
#![allow(dead_code, clippy::needless_range_loop, clippy::too_many_arguments)]

use std::collections::HashMap;
use triergon_codec_dca::tables::{self, HuffBook};

pub struct BitWriter {
    pub bytes: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            acc: 0,
            n: 0,
        }
    }
    pub fn put(&mut self, v: u32, n: u32) {
        assert!(n <= 32);
        if n == 0 {
            return;
        }
        let v = u64::from(v) & ((1u64 << n) - 1);
        self.acc = (self.acc << n) | v;
        self.n += n;
        while self.n >= 8 {
            self.n -= 8;
            self.bytes.push((self.acc >> self.n) as u8);
        }
        self.acc &= (1u64 << self.n) - 1;
    }
    pub fn put_signed(&mut self, v: i32, n: u32) {
        assert!(
            v >= -(1i64 << (n - 1)) as i32 && i64::from(v) < (1i64 << (n - 1)),
            "value {v} in {n} bits"
        );
        self.put(v as u32, n);
    }
    pub fn bits(&self) -> usize {
        self.bytes.len() * 8 + self.n as usize
    }
    pub fn align(&mut self, n: usize) {
        while !self.bits().is_multiple_of(n) {
            self.put(0, 1);
        }
    }
    pub fn finish(mut self) -> Vec<u8> {
        self.align(8);
        self.bytes
    }
}

/// Symbol -> (code, length) recovered from a decoder lookup table.
pub struct HuffEnc(HashMap<i32, (u32, u32)>);

impl HuffEnc {
    pub fn new(book: &HuffBook) -> Self {
        let mut m = HashMap::new();
        let pb = book.primary_bits;
        for i in 0..(1usize << pb) {
            let (s, l) = book.lut[i];
            if l > 0 {
                let l = l as u32;
                m.insert(i32::from(s), ((i as u32) >> (pb - l), l));
            } else {
                let sb = (-l) as u32;
                for j in 0..(1usize << sb) {
                    let (s2, l2) = book.lut[s as usize + j];
                    let l2 = l2 as u32;
                    let r = l2 - pb;
                    let code = ((i as u32) << r) | ((j as u32) >> (sb - r));
                    m.insert(i32::from(s2), (code, l2));
                }
            }
        }
        Self(m)
    }
    pub fn put(&self, w: &mut BitWriter, sym: i32) {
        let &(c, l) = self
            .0
            .get(&sym)
            .unwrap_or_else(|| panic!("symbol {sym} not in codebook"));
        w.put(c, l);
    }
    pub fn has(&self, sym: i32) -> bool {
        self.0.contains_key(&sym)
    }
}

pub fn tmode_book(i: u8) -> &'static HuffBook {
    [
        &tables::HUFF_A4,
        &tables::HUFF_B4,
        &tables::HUFF_C4,
        &tables::HUFF_D4,
    ][i as usize]
}
pub fn scale_book(i: u8) -> &'static HuffBook {
    [
        &tables::HUFF_SA129,
        &tables::HUFF_SB129,
        &tables::HUFF_SC129,
        &tables::HUFF_SD129,
        &tables::HUFF_SE129,
    ][i as usize]
}
pub fn abits_book(i: u8) -> &'static HuffBook {
    [
        &tables::HUFF_A12,
        &tables::HUFF_B12,
        &tables::HUFF_C12,
        &tables::HUFF_D12,
        &tables::HUFF_E12,
    ][i as usize]
}
pub fn sample_book(abits: u32, sel: u32) -> &'static HuffBook {
    use tables::*;
    let b: &[&'static HuffBook] = match abits {
        1 => &[&HUFF_A3],
        2 => &[&HUFF_A5, &HUFF_B5, &HUFF_C5],
        3 => &[&HUFF_A7, &HUFF_B7, &HUFF_C7],
        4 => &[&HUFF_A9, &HUFF_B9, &HUFF_C9],
        5 => &[&HUFF_A13, &HUFF_B13, &HUFF_C13],
        6 => &[
            &HUFF_A17, &HUFF_B17, &HUFF_C17, &HUFF_D17, &HUFF_E17, &HUFF_F17, &HUFF_G17,
        ],
        7 => &[
            &HUFF_A25, &HUFF_B25, &HUFF_C25, &HUFF_D25, &HUFF_E25, &HUFF_F25, &HUFF_G25,
        ],
        8 => &[
            &HUFF_A33, &HUFF_B33, &HUFF_C33, &HUFF_D33, &HUFF_E33, &HUFF_F33, &HUFF_G33,
        ],
        9 => &[
            &HUFF_A65, &HUFF_B65, &HUFF_C65, &HUFF_D65, &HUFF_E65, &HUFF_F65, &HUFF_G65,
        ],
        _ => &[
            &HUFF_A129, &HUFF_B129, &HUFF_C129, &HUFF_D129, &HUFF_E129, &HUFF_F129, &HUFF_G129,
        ],
    };
    b[sel as usize]
}
pub fn huffman_sel_count(abits: u32) -> u32 {
    match abits {
        1 => 1,
        2..=5 => 3,
        6..=10 => 7,
        _ => 0,
    }
}
pub fn levels(abits: u32) -> i32 {
    [0, 3, 5, 7, 9, 13, 17, 25][abits as usize]
}
pub fn block_bits(abits: u32) -> u32 {
    [0, 7, 10, 12, 13, 15, 17, 19][abits as usize]
}

#[derive(Clone)]
pub struct ChanCfg {
    pub nsubs: u8,
    pub nvqsub: u8,
    pub joinx: u8,
    pub thuff: u8,
    pub shuff: u8,
    pub bhuff: u8,
    pub sel: [u8; 10],
    pub adj: [u8; 10],
}

impl Default for ChanCfg {
    fn default() -> Self {
        // All subbands active, no VQ, linear ABITS (5-bit), 7-bit linear scales,
        // block code / no-further-encoding everywhere.
        Self {
            nsubs: 32,
            nvqsub: 32,
            joinx: 0,
            thuff: 0,
            shuff: 6,
            bhuff: 6,
            sel: [1, 3, 3, 3, 3, 7, 7, 7, 7, 7],
            adj: [0; 10],
        }
    }
}

#[derive(Clone, Default)]
pub struct Band {
    pub pmode: bool,
    pub pvq: u16,
    pub abits: u8,
    pub tmode: u8,
    /// Scale factor *indices* (into the 6- or 7-bit table selected by SHUFF).
    pub scale: [i32; 2],
    /// Quantization indices, `nssc * 8` values.
    pub q: Vec<i32>,
    /// High frequency VQ index (bands nvqsub..nsubs).
    pub vq: u16,
    /// Joint intensity scale code (-64..=64) for bands nsubs..nsubs[src].
    pub jscale: i32,
}

#[derive(Clone)]
pub struct ChanSub {
    pub bands: Vec<Band>,
    pub join_shuff: u8,
}

impl Default for ChanSub {
    fn default() -> Self {
        Self {
            bands: vec![Band::default(); 32],
            join_shuff: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct Subframe {
    pub nssc: usize,
    pub psc: u8,
    pub chans: Vec<ChanSub>,
    pub lfe: Vec<i32>,
    pub lfe_scale: u8,
    pub range: u8,
}

#[derive(Clone)]
pub struct Frame {
    pub normal: bool,
    pub short: u8,
    pub cpf: bool,
    pub blocks: usize,
    pub amode: u8,
    pub sfreq: u8,
    pub rate: u8,
    pub dynf: bool,
    pub timef: bool,
    pub auxf: bool,
    pub ext_audio_id: u8,
    pub ext_audio: bool,
    pub aspf: bool,
    pub lff: u8,
    pub hflag: bool,
    pub filts: bool,
    pub vernum: u8,
    pub pcmr: u8,
    pub sumf: bool,
    pub sums: bool,
    pub dialnorm: u8,
    pub chans: Vec<ChanCfg>,
    pub subframes: Vec<Subframe>,
    /// Raw extension payload appended after the core data (32-bit aligned).
    pub ext: Vec<u8>,
    /// Minimum frame size in bytes.
    pub min_size: usize,
}

impl Frame {
    pub fn new(amode: u8, nch: usize, blocks: usize) -> Self {
        Self {
            normal: true,
            short: 31,
            cpf: false,
            blocks,
            amode,
            sfreq: 13,
            rate: 15,
            dynf: false,
            timef: false,
            auxf: false,
            ext_audio_id: 0,
            ext_audio: false,
            aspf: false,
            lff: 0,
            hflag: true,
            filts: false,
            vernum: 7,
            pcmr: 6,
            sumf: false,
            sums: false,
            dialnorm: 0,
            chans: vec![ChanCfg::default(); nch],
            subframes: Vec::new(),
            ext: Vec::new(),
            min_size: 96,
        }
    }

    /// Append a silent subframe with `nssc` subsubframes.
    pub fn silent_subframe(&mut self, nssc: usize) -> &mut Subframe {
        let nch = self.chans.len();
        let mut sf = Subframe {
            nssc,
            chans: vec![ChanSub::default(); nch],
            ..Default::default()
        };
        for c in &mut sf.chans {
            for b in &mut c.bands {
                b.q = vec![0; nssc * 8];
            }
        }
        sf.lfe = vec![0; 2 * self.lff as usize * nssc];
        self.subframes.push(sf);
        self.subframes.last_mut().unwrap()
    }
}

fn put_scale(w: &mut BitWriter, shuff: u8, idx: i32, sum: &mut i32) {
    match shuff {
        0..=4 => {
            HuffEnc::new(scale_book(shuff)).put(w, idx - *sum);
            *sum = idx;
        }
        5 => {
            w.put(idx as u32, 6);
            *sum = idx;
        }
        _ => {
            w.put(idx as u32, 7);
            *sum = idx;
        }
    }
}

/// Write the per-subframe part of a channel set (side info + data arrays).
pub fn write_set_subframe(
    w: &mut BitWriter,
    chans: &[ChanCfg],
    sf: &Subframe,
    core: Option<&Frame>,
    aspf: bool,
    cpf: bool,
) {
    let nch = chans.len();
    let nssc = sf.nssc;
    for ch in 0..nch {
        for n in 0..chans[ch].nsubs as usize {
            w.put(sf.chans[ch].bands[n].pmode as u32, 1);
        }
    }
    for ch in 0..nch {
        for n in 0..chans[ch].nsubs as usize {
            let b = &sf.chans[ch].bands[n];
            if b.pmode {
                w.put(u32::from(b.pvq), 12);
            }
        }
    }
    for ch in 0..nch {
        let c = &chans[ch];
        for n in 0..c.nvqsub.min(32) as usize {
            let a = sf.chans[ch].bands[n].abits as u32;
            match c.bhuff {
                0..=4 => HuffEnc::new(abits_book(c.bhuff)).put(w, a as i32),
                5 => w.put(a, 4),
                _ => w.put(a, 5),
            }
        }
    }
    if nssc > 1 {
        for ch in 0..nch {
            let c = &chans[ch];
            let enc = HuffEnc::new(tmode_book(c.thuff));
            for n in 0..c.nvqsub.min(32) as usize {
                let b = &sf.chans[ch].bands[n];
                if b.abits > 0 {
                    enc.put(w, i32::from(b.tmode));
                }
            }
        }
    }
    for ch in 0..nch {
        let c = &chans[ch];
        let mut sum = 0;
        for n in 0..c.nvqsub.min(32) as usize {
            let b = &sf.chans[ch].bands[n];
            if b.abits > 0 {
                put_scale(w, c.shuff, b.scale[0], &mut sum);
                if b.tmode > 0 {
                    put_scale(w, c.shuff, b.scale[1], &mut sum);
                }
            }
        }
        for n in c.nvqsub as usize..c.nsubs as usize {
            put_scale(w, c.shuff, sf.chans[ch].bands[n].scale[0], &mut sum);
        }
    }
    for ch in 0..nch {
        if chans[ch].joinx > 0 {
            w.put(u32::from(sf.chans[ch].join_shuff), 3);
        }
    }
    for ch in 0..nch {
        if chans[ch].joinx > 0 {
            let src = chans[ch].joinx as usize - 1;
            for n in chans[ch].nsubs as usize..chans[src].nsubs as usize {
                let mut s = 0;
                put_scale(
                    w,
                    sf.chans[ch].join_shuff,
                    sf.chans[ch].bands[n].jscale,
                    &mut s,
                );
            }
        }
    }
    if let Some(f) = core
        && f.dynf
    {
        w.put(u32::from(sf.range), 8);
    }
    if cpf {
        w.put(0, 16);
    }
    for ch in 0..nch {
        let c = &chans[ch];
        for n in c.nvqsub as usize..c.nsubs as usize {
            w.put(u32::from(sf.chans[ch].bands[n].vq), 10);
        }
    }
    if let Some(f) = core
        && f.lff > 0
    {
        assert_eq!(sf.lfe.len(), 2 * f.lff as usize * nssc);
        for &v in &sf.lfe {
            w.put_signed(v, 8);
        }
        w.put(u32::from(sf.lfe_scale), 8);
    }
    for ssf in 0..nssc {
        for ch in 0..nch {
            let c = &chans[ch];
            for n in 0..c.nvqsub.min(32) as usize {
                let b = &sf.chans[ch].bands[n];
                let a = b.abits as u32;
                if a == 0 {
                    continue;
                }
                let q = &b.q[ssf * 8..ssf * 8 + 8];
                let sel = if a <= 10 {
                    u32::from(c.sel[a as usize - 1])
                } else {
                    0
                };
                if a <= 10 && sel < huffman_sel_count(a) {
                    let enc = HuffEnc::new(sample_book(a, sel));
                    for &v in q {
                        enc.put(w, v);
                    }
                } else if a <= 7 {
                    let l = levels(a);
                    let off = (l - 1) / 2;
                    for blk in 0..2 {
                        let mut code = 0i64;
                        for j in (0..4).rev() {
                            let v = q[blk * 4 + j] + off;
                            assert!(v >= 0 && v < l);
                            code = code * i64::from(l) + i64::from(v);
                        }
                        w.put(code as u32, block_bits(a));
                    }
                } else {
                    for &v in q {
                        w.put_signed(v, a - 3);
                    }
                }
            }
        }
        if ssf == nssc - 1 || aspf {
            w.put(0xFFFF, 16);
        }
    }
}

/// Serialize a complete core frame (16-bit big endian).
pub fn write_frame(f: &Frame) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(0x7FFE_8001, 32);
    w.put(f.normal as u32, 1);
    w.put(u32::from(f.short), 5);
    w.put(f.cpf as u32, 1);
    w.put(f.blocks as u32 - 1, 7);
    w.put(0, 14); // FSIZE patched below
    w.put(u32::from(f.amode), 6);
    w.put(u32::from(f.sfreq), 4);
    w.put(u32::from(f.rate), 5);
    w.put(0, 1);
    w.put(f.dynf as u32, 1);
    w.put(f.timef as u32, 1);
    w.put(f.auxf as u32, 1);
    w.put(0, 1);
    w.put(u32::from(f.ext_audio_id), 3);
    w.put(f.ext_audio as u32, 1);
    w.put(f.aspf as u32, 1);
    w.put(u32::from(f.lff), 2);
    w.put(f.hflag as u32, 1);
    if f.cpf {
        w.put(0, 16);
    }
    w.put(f.filts as u32, 1);
    w.put(u32::from(f.vernum), 4);
    w.put(0, 2);
    w.put(u32::from(f.pcmr), 3);
    w.put(f.sumf as u32, 1);
    w.put(f.sums as u32, 1);
    w.put(u32::from(f.dialnorm), 4);
    // primary audio coding header
    w.put(f.subframes.len() as u32 - 1, 4);
    write_set_header(&mut w, &f.chans, f.cpf);
    for sf in &f.subframes {
        w.put(sf.nssc as u32 - 1, 2);
        w.put(u32::from(sf.psc), 3);
        write_set_subframe(&mut w, &f.chans, sf, Some(f), f.aspf, f.cpf);
    }
    if f.timef {
        w.put(0, 32);
    }
    if f.auxf {
        w.put(0, 6);
    }
    if f.cpf && f.dynf {
        w.put(0, 16);
    }
    let mut out = w.finish();
    if !f.ext.is_empty() {
        // Extensions end the frame: pad the core part, then append (32-bit aligned).
        while !out.len().is_multiple_of(4) || out.len() + f.ext.len() < f.min_size {
            out.push(0);
        }
        out.extend_from_slice(&f.ext);
    }
    while out.len() < f.min_size || !out.len().is_multiple_of(2) {
        out.push(0);
    }
    let fsize = (out.len() - 1) as u32;
    // FSIZE occupies bits 46..60.
    let mut v = u64::from_be_bytes(out[..8].try_into().unwrap());
    v &= !(0x3FFFu64 << (64 - 60));
    v |= u64::from(fsize) << (64 - 60);
    out[..8].copy_from_slice(&v.to_be_bytes());
    out
}

pub fn write_set_header(w: &mut BitWriter, chans: &[ChanCfg], cpf: bool) {
    let nch = chans.len();
    w.put(nch as u32 - 1, 3);
    for c in chans {
        w.put(u32::from(c.nsubs) - 2, 5);
    }
    for c in chans {
        w.put(u32::from(c.nvqsub) - 1, 5);
    }
    for c in chans {
        w.put(u32::from(c.joinx), 3);
    }
    for c in chans {
        w.put(u32::from(c.thuff), 2);
    }
    for c in chans {
        w.put(u32::from(c.shuff), 3);
    }
    for c in chans {
        w.put(u32::from(c.bhuff), 3);
    }
    for n in 0..10 {
        let bits = match n {
            0 => 1,
            1..=4 => 2,
            _ => 3,
        };
        for c in chans {
            w.put(u32::from(c.sel[n]), bits);
        }
    }
    for n in 0..10 {
        let limit = match n {
            0 => 1,
            1..=4 => 3,
            _ => 7,
        };
        for c in chans {
            if c.sel[n] < limit {
                w.put(u32::from(c.adj[n]), 2);
            }
        }
    }
    if cpf {
        w.put(0, 16);
    }
}

/// Convert a 16-bit big-endian stream into another packing.
pub fn repack(be16: &[u8], to: triergon_codec_dca::Packing) -> Vec<u8> {
    use triergon_codec_dca::Packing;
    match to {
        Packing::Be16 => be16.to_vec(),
        Packing::Le16 => be16
            .chunks(2)
            .flat_map(|w| [w.get(1).copied().unwrap_or(0), w[0]])
            .collect(),
        Packing::Be14 | Packing::Le14 => {
            let total = be16.len() * 8;
            let words = total.div_ceil(14);
            let mut out = Vec::with_capacity(words * 2);
            for i in 0..words {
                let mut v: u16 = 0;
                for k in 0..14 {
                    let bit = i * 14 + k;
                    let b = if bit < total {
                        (be16[bit / 8] >> (7 - bit % 8)) & 1
                    } else {
                        0
                    };
                    v = (v << 1) | u16::from(b);
                }
                // sign-extend bit 13 into the top two bits
                if v & 0x2000 != 0 {
                    v |= 0xC000;
                }
                if to == Packing::Le14 {
                    out.extend_from_slice(&v.to_le_bytes());
                } else {
                    out.extend_from_slice(&v.to_be_bytes());
                }
            }
            out
        }
    }
}

// ------------------------------------------------------------------ X96 (Tables 6-1, 6-4, 6-9, 6-10)

#[derive(Clone)]
pub struct X96Chan {
    pub sbe: u8,
    pub joinx: u8,
    pub shuff: u8,
    pub bhuff: u8,
    pub sel: [u8; 10],
}

#[derive(Clone, Default)]
pub struct X96Band {
    pub pmode: bool,
    pub pvq: u16,
    pub abits: u8,
    pub scale: i32,
    pub q: Vec<i32>,
    pub vq: Vec<u16>,
    pub jscale: i32,
}

#[derive(Clone)]
pub struct X96Sub {
    pub bands: Vec<Vec<X96Band>>,
    pub join_shuff: Vec<u8>,
}

pub struct X96 {
    pub revno: u8,
    pub highres: bool,
    pub sbs: u8,
    pub chans: Vec<X96Chan>,
    pub subs: Vec<X96Sub>,
}

pub fn abits96_book(highres: bool, i: u8) -> &'static HuffBook {
    use tables::*;
    if highres {
        [
            &HUFF_A33, &HUFF_B33, &HUFF_C33, &HUFF_D33, &HUFF_E33, &HUFF_F33, &HUFF_G33,
        ][i as usize]
    } else {
        [
            &HUFF_A17, &HUFF_B17, &HUFF_C17, &HUFF_D17, &HUFF_E17, &HUFF_F17, &HUFF_G17,
        ][i as usize]
    }
}

/// Serialize a DTS_BCCORE_X96 block (sync .. last DSYNC), FSIZE96 = its own length.
pub fn write_x96(x: &X96, nssc: &[usize], aspf: bool, cpf: bool) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(0x1D95_F262, 32);
    w.put(0, 12); // patched
    w.put(u32::from(x.revno), 4);
    w.put(x.highres as u32, 1);
    let sbs = if x.revno < 8 {
        w.put(u32::from(x.sbs), 5);
        x.sbs as usize
    } else {
        32
    };
    for c in &x.chans {
        w.put(u32::from(c.sbe) - 1, 6);
    }
    for c in &x.chans {
        w.put(u32::from(c.joinx), 3);
    }
    for c in &x.chans {
        w.put(u32::from(c.shuff), 3);
    }
    for c in &x.chans {
        w.put(u32::from(c.bhuff), 3);
    }
    let count = if x.highres { 10 } else { 6 };
    for n in 0..count {
        let bits = match n {
            0 => 1,
            1..=4 => 2,
            _ => 3,
        };
        for c in &x.chans {
            w.put(u32::from(c.sel[n]), bits);
        }
    }
    if cpf {
        w.put(0, 16);
    }
    let nch = x.chans.len();
    for (si, sub) in x.subs.iter().enumerate() {
        let ns = nssc[si];
        for ch in 0..nch {
            for n in sbs..x.chans[ch].sbe as usize {
                w.put(sub.bands[ch][n].pmode as u32, 1);
            }
        }
        for ch in 0..nch {
            for n in sbs..x.chans[ch].sbe as usize {
                if sub.bands[ch][n].pmode {
                    w.put(u32::from(sub.bands[ch][n].pvq), 12);
                }
            }
        }
        for ch in 0..nch {
            let c = &x.chans[ch];
            let mut sum = 0;
            for n in sbs..c.sbe as usize {
                let a = i32::from(sub.bands[ch][n].abits);
                if c.bhuff < 7 {
                    HuffEnc::new(abits96_book(x.highres, c.bhuff)).put(&mut w, a - sum);
                    sum = a;
                } else {
                    w.put(a as u32, if x.highres { 4 } else { 3 });
                }
            }
        }
        for ch in 0..nch {
            let c = &x.chans[ch];
            let mut sum = 0;
            for n in sbs..c.sbe as usize {
                let v = sub.bands[ch][n].scale;
                if c.shuff < 5 {
                    HuffEnc::new(scale_book(c.shuff)).put(&mut w, v - sum);
                    sum = v;
                } else {
                    w.put(v as u32, 6);
                }
            }
        }
        for ch in 0..nch {
            if x.chans[ch].joinx > 0 {
                w.put(u32::from(sub.join_shuff[ch]), 3);
            }
        }
        for ch in 0..nch {
            if x.chans[ch].joinx > 0 {
                let src = x.chans[ch].joinx as usize - 1;
                for n in x.chans[ch].sbe as usize..x.chans[src].sbe as usize {
                    let js = sub.join_shuff[ch];
                    let v = sub.bands[ch][n].jscale;
                    if js < 5 {
                        HuffEnc::new(scale_book(js)).put(&mut w, v);
                    } else {
                        w.put(v as u32, 6);
                    }
                }
            }
        }
        if cpf {
            w.put(0, 16);
        }
        for ch in 0..nch {
            for n in sbs..x.chans[ch].sbe as usize {
                let b = &sub.bands[ch][n];
                if b.abits == 1 {
                    assert_eq!(b.vq.len(), (ns * 8).div_ceil(16));
                    for &v in &b.vq {
                        w.put(u32::from(v), 10);
                    }
                }
            }
        }
        for ssf in 0..ns {
            for ch in 0..nch {
                let c = &x.chans[ch];
                for n in sbs..c.sbe as usize {
                    let b = &sub.bands[ch][n];
                    let a96 = u32::from(b.abits);
                    if a96 < 2 {
                        continue;
                    }
                    let g = a96 - 1;
                    let q = &b.q[ssf * 8..ssf * 8 + 8];
                    let sel = if a96 <= 11 {
                        u32::from(c.sel[a96 as usize - 2])
                    } else {
                        0
                    };
                    if g <= 10 && sel < huffman_sel_count(g) {
                        let enc = HuffEnc::new(sample_book(g, sel));
                        for &v in q {
                            enc.put(&mut w, v);
                        }
                    } else if g <= 7 {
                        let l = levels(g);
                        let off = (l - 1) / 2;
                        for blk in 0..2 {
                            let mut code = 0i64;
                            for j in (0..4).rev() {
                                code = code * i64::from(l) + i64::from(q[blk * 4 + j] + off);
                            }
                            w.put(code as u32, block_bits(g));
                        }
                    } else {
                        for &v in q {
                            w.put_signed(v, g - 3);
                        }
                    }
                }
            }
            if ssf == ns - 1 || aspf {
                w.put(0xFFFF, 16);
            }
        }
    }
    let mut out = w.finish();
    while !out.len().is_multiple_of(4) || out.len() < 96 {
        out.push(0);
    }
    let size = (out.len() - 1) as u32;
    out[4] = (size >> 4) as u8;
    out[5] = ((size & 15) << 4) as u8 | (out[5] & 15);
    out
}
