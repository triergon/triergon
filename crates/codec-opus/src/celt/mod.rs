//! The CELT layer (RFC 6716 section 4.3): an MDCT codec that codes band energies and
//! normalised band shapes.
//!
//! [`CeltDecoder`] holds the state of one stream; the transforms and per-frame buffers
//! live in a [`Workspace`] that every stream of a decoder shares.

mod alloc;
mod bands;
mod energy;
mod postfilter;
mod pvq;
mod synthesis;
mod tables;

use crate::range::RangeDecoder;
use alloc::{AllocationRequest, BITRES, band_width};
use bands::{AntiCollapse, BandParams, NORM_LEN};
use energy::Energies;
use postfilter::{MAX_REACH, Pitch};
use pvq::Spread;
use synthesis::{MAX_FRAME, OVERLAP, Synthesis};
use tables::{BANDS, EBANDS, SPREAD_ICDF, TF_SELECT, TRIM_ICDF, WINDOW};

/// The first band a hybrid frame codes in CELT (8 kHz and up).
pub(crate) const HYBRID_START_BAND: usize = 17;

/// Output history: the frame plus everything the post-filter reads before it.
const HISTORY: usize = 2048;
const _: () = assert!(HISTORY >= MAX_FRAME + MAX_REACH);

/// De-emphasis filter coefficient (the inverse of the encoder's pre-emphasis).
const DEEMPHASIS: f32 = 0.850_006_1;
/// Signal scale of the reference decoder, folded into the band amplitudes.
const SIGNAL_SCALE: f32 = 1.0 / 32768.0;
/// Log energy of silence.
const SILENCE: f32 = -28.0;

/// Sample `i` of a 2.5 ms cross-fade from `outgoing` to `incoming` over the
/// power-complementary window overlap (`smooth_fade`).
#[inline]
pub(crate) fn fade(i: usize, outgoing: f32, incoming: f32) -> f32 {
    let w = WINDOW[i] * WINDOW[i];
    w * incoming + (1.0 - w) * outgoing
}

/// Buffers and transforms of one CELT frame, shared by all streams of a decoder.
#[derive(Debug)]
pub(crate) struct Workspace {
    synthesis: Synthesis,
    /// Normalised shapes, then spectra, of both channels (`MAX_FRAME` each).
    shapes: [f32; 2 * MAX_FRAME],
    spectrum: [f32; 2 * MAX_FRAME],
    norm: [f32; 2 * NORM_LEN],
}

impl Workspace {
    pub(crate) fn new() -> Workspace {
        Workspace {
            synthesis: Synthesis::new(),
            shapes: [0.0; 2 * MAX_FRAME],
            spectrum: [0.0; 2 * MAX_FRAME],
            norm: [0.0; 2 * NORM_LEN],
        }
    }
}

/// What a CELT frame codes, from the Opus TOC and mode.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameLayout {
    /// Frame size `120 << lm` samples.
    pub(crate) lm: usize,
    /// Coded bands.
    pub(crate) start: usize,
    pub(crate) end: usize,
    /// Channels in the bitstream (the TOC stereo flag).
    pub(crate) coded_channels: usize,
}

/// CELT state of one stream.
#[derive(Clone, Debug)]
pub(crate) struct CeltDecoder {
    /// Output channels.
    channels: usize,
    /// Post-filtered output; the newest frame ends the buffer.
    history: [[f32; HISTORY]; 2],
    overlap: [[f32; OVERLAP]; 2],
    /// Log energies of the last frame and the two before (anti-collapse).
    energy: Energies,
    prev_energy: [Energies; 2],
    /// The post-filter of the current frame and the one it fades from.
    pitch: Pitch,
    pitch_old: Pitch,
    deemphasis: [f32; 2],
    /// Folding noise seed: the final range of the last frame.
    seed: u32,
}

impl CeltDecoder {
    pub(crate) fn new(channels: usize) -> CeltDecoder {
        let mut decoder = CeltDecoder {
            channels,
            history: [[0.0; HISTORY]; 2],
            overlap: [[0.0; OVERLAP]; 2],
            energy: [[0.0; BANDS]; 2],
            prev_energy: [[[SILENCE; BANDS]; 2]; 2],
            pitch: Pitch::default(),
            pitch_old: Pitch::default(),
            deemphasis: [0.0; 2],
            seed: 0,
        };
        decoder.reset();
        decoder
    }

    /// Drop all signal history.
    pub(crate) fn reset(&mut self) {
        for channel in &mut self.history {
            channel.fill(0.0);
        }
        self.overlap = [[0.0; OVERLAP]; 2];
        self.energy = [[0.0; BANDS]; 2];
        self.prev_energy = [[[SILENCE; BANDS]; 2]; 2];
        self.pitch = Pitch::default();
        self.pitch_old = Pitch::default();
        self.deemphasis = [0.0; 2];
        self.seed = 0;
    }

    /// Decode one frame from `rc` (which a hybrid frame shares with SILK) into
    /// `out[channel][..120 << lm]` for each output channel.
    pub(crate) fn decode(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        layout: FrameLayout,
        work: &mut Workspace,
        out: [&mut [f32]; 2],
    ) {
        let FrameLayout {
            lm,
            start,
            end,
            coded_channels: c,
        } = layout;
        let bands = start..end;
        let total_bits = rc.len() as i32 * 8;
        if c == 1 {
            // A mono frame predicts from the louder channel of a stereo one.
            let [first, second] = &mut self.energy;
            for (a, &b) in first.iter_mut().zip(second.iter()) {
                *a = a.max(b);
            }
        }

        let header = Header::decode(rc, layout, total_bits);
        energy::decode_coarse(rc, &mut self.energy, bands.clone(), header.intra, c, lm);
        let tf_change = decode_tf(rc, bands.clone(), header.transient, lm);
        let spread = if rc.tell() + 4 <= total_bits {
            Spread::from_symbol(rc.icdf(&SPREAD_ICDF, 5))
        } else {
            Spread::Normal
        };
        let caps = alloc::caps(lm, c);
        let (boosts, trim) = decode_boosts_and_trim(rc, bands.clone(), &caps, total_bits, c, lm);
        let mut bits = (total_bits << BITRES) - rc.tell_frac() - 1;
        let anti_collapse_reserve =
            if header.transient && lm >= 2 && bits >= (lm as i32 + 2) << BITRES {
                1 << BITRES
            } else {
                0
            };
        bits -= anti_collapse_reserve;
        let request = AllocationRequest {
            bands: bands.clone(),
            boosts: &boosts,
            caps: &caps,
            trim,
            total: bits,
            channels: c,
            lm,
        };
        let allocation = alloc::allocate(rc, &request);
        energy::decode_fine(rc, &mut self.energy, bands.clone(), &allocation, c);

        // Band shapes, then the energy bits left over and the anti-collapse noise.
        let blocks = if header.transient { 1 << lm } else { 1 };
        let params = BandParams {
            bands: bands.clone(),
            allocation: &allocation,
            tf_change: &tf_change,
            spread,
            blocks,
            lm,
            total_bits: (total_bits << BITRES) - anti_collapse_reserve,
        };
        let collapse = self.decode_shapes(rc, &params, c, work);
        let anti_collapse = anti_collapse_reserve > 0 && rc.bits(1) != 0;
        let bits_left = total_bits - rc.tell();
        energy::decode_final(
            rc,
            &mut self.energy,
            bands.clone(),
            &allocation,
            bits_left,
            c,
        );
        if anti_collapse {
            let noise = AntiCollapse {
                bands: bands.clone(),
                lm,
                channels: c,
                energy: &self.energy,
                prev1: &self.prev_energy[0],
                prev2: &self.prev_energy[1],
                pulses: &allocation.pulses,
                collapse: &collapse,
            };
            bands::anti_collapse(&noise, &mut work.shapes, MAX_FRAME, self.seed);
        }

        if header.silence {
            for energy in self.energy.iter_mut().take(c) {
                energy.fill(SILENCE);
            }
        }
        self.denormalise(layout, header.silence, work);
        self.synthesize(layout, blocks, header.pitch.unwrap_or_default(), work, out);
        self.update_history(layout, header.transient);
        self.seed = rc.range();
    }

    /// Decode the normalised band shapes into `work.shapes` (channel `c` from
    /// `c * MAX_FRAME`) and return the collapse masks.
    fn decode_shapes(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        params: &BandParams<'_>,
        channels: usize,
        work: &mut Workspace,
    ) -> [[u8; 2]; BANDS] {
        let n = 120 << params.lm;
        let (x, y) = work.shapes.split_at_mut(MAX_FRAME);
        let (x, y) = (&mut x[..n], &mut y[..n]);
        for shape in [&mut *x, &mut *y].into_iter().take(channels) {
            shape[..EBANDS[params.bands.start] << params.lm].fill(0.0);
            shape[EBANDS[params.bands.end] << params.lm..].fill(0.0);
        }
        let mut collapse = [[0u8; 2]; BANDS];
        let y = (channels == 2).then_some(y);
        bands::decode(
            rc,
            params,
            x,
            y,
            &mut collapse,
            &mut work.norm,
            &mut self.seed,
        );
        collapse
    }

    /// Scale the shapes by the band amplitudes into `work.spectrum`, matching the coded
    /// channels to the output channels.
    fn denormalise(&self, layout: FrameLayout, silence: bool, work: &mut Workspace) {
        let FrameLayout {
            lm,
            start,
            end,
            coded_channels,
        } = layout;
        let n = 120 << lm;
        for ch in 0..coded_channels {
            let mut amplitude = [0f32; BANDS];
            if !silence {
                energy::amplitudes(&self.energy[ch], start..end, &mut amplitude);
            }
            let shape = &work.shapes[ch * MAX_FRAME..ch * MAX_FRAME + n];
            let spectrum = &mut work.spectrum[ch * MAX_FRAME..ch * MAX_FRAME + n];
            spectrum.fill(0.0);
            for band in start..end {
                let range = EBANDS[band] << lm..EBANDS[band + 1] << lm;
                let gain = amplitude[band] * SIGNAL_SCALE;
                for (f, &v) in spectrum[range.clone()].iter_mut().zip(&shape[range]) {
                    *f = v * gain;
                }
            }
        }
        let (left, right) = work.spectrum.split_at_mut(MAX_FRAME);
        match (self.channels, coded_channels) {
            (2, 1) => right[..n].copy_from_slice(&left[..n]),
            (1, 2) => {
                for (l, &r) in left[..n].iter_mut().zip(&right[..n]) {
                    *l = 0.5 * (*l + r);
                }
            }
            _ => {}
        }
    }

    /// Inverse transform, post-filter and de-emphasis of `work.spectrum` into `out`;
    /// `pitch` is this frame's post-filter, which fades in after the first short block.
    fn synthesize(
        &mut self,
        layout: FrameLayout,
        blocks: usize,
        pitch: Pitch,
        work: &mut Workspace,
        out: [&mut [f32]; 2],
    ) {
        let lm = layout.lm;
        let n = 120 << lm;
        let frame = HISTORY - n;
        self.pitch = self.pitch.bounded();
        self.pitch_old = self.pitch_old.bounded();
        for (ch, out) in out.into_iter().enumerate().take(self.channels) {
            let history = &mut self.history[ch];
            history.copy_within(n.., 0);
            let spectrum = &work.spectrum[ch * MAX_FRAME..ch * MAX_FRAME + n];
            work.synthesis.inverse(
                spectrum,
                blocks,
                lm,
                &mut history[frame..],
                &mut self.overlap[ch],
            );
            postfilter::comb(history, frame, WINDOW.len(), self.pitch_old, self.pitch);
            if lm != 0 {
                // A disabled filter has no period; its taps are zero whatever it reads.
                postfilter::comb(
                    history,
                    frame + WINDOW.len(),
                    n - WINDOW.len(),
                    self.pitch,
                    pitch.bounded(),
                );
            }
            let mut memory = self.deemphasis[ch];
            for (o, &v) in out[..n].iter_mut().zip(&history[frame..]) {
                let t = v + memory;
                memory = DEEMPHASIS * t;
                *o = t;
            }
            self.deemphasis[ch] = memory;
        }
        self.pitch_old = if lm != 0 { pitch } else { self.pitch };
        self.pitch = pitch;
    }

    /// Keep the energies for the next frame's prediction and anti-collapse; bands
    /// outside the coded ones restart from silence.
    fn update_history(&mut self, layout: FrameLayout, transient: bool) {
        if layout.coded_channels == 1 {
            self.energy[1] = self.energy[0];
        }
        let [prev1, prev2] = &mut self.prev_energy;
        if transient {
            for (p, e) in prev1.iter_mut().flatten().zip(self.energy.iter().flatten()) {
                *p = p.min(*e);
            }
        } else {
            *prev2 = *prev1;
            *prev1 = self.energy;
        }
        for ch in 0..2 {
            for band in (0..layout.start).chain(layout.end..BANDS) {
                self.energy[ch][band] = 0.0;
                prev1[ch][band] = SILENCE;
                prev2[ch][band] = SILENCE;
            }
        }
    }
}

/// The frame header flags (RFC 6716 Table 56).
#[derive(Clone, Copy, Debug)]
struct Header {
    /// The frame codes silence; its remaining bits count as read.
    silence: bool,
    pitch: Option<Pitch>,
    transient: bool,
    /// Coarse energy without inter-frame prediction.
    intra: bool,
}

impl Header {
    fn decode(rc: &mut RangeDecoder<'_>, layout: FrameLayout, total_bits: i32) -> Header {
        let mut tell = rc.tell();
        let silence = tell >= total_bits || (tell == 1 && rc.bit_logp(15));
        if silence {
            rc.exhaust();
            tell = total_bits;
        }
        let mut pitch = None;
        if layout.start == 0 && tell + 16 <= total_bits {
            pitch = Pitch::decode(rc, total_bits);
            tell = rc.tell();
        }
        let mut transient = false;
        if layout.lm > 0 && tell + 3 <= total_bits {
            transient = rc.bit_logp(3);
            tell = rc.tell();
        }
        let intra = tell + 3 <= total_bits && rc.bit_logp(3);
        Header {
            silence,
            pitch,
            transient,
            intra,
        }
    }
}

/// Decode the per-band time-frequency changes (RFC 6716 section 4.3.4.5).
fn decode_tf(
    rc: &mut RangeDecoder<'_>,
    bands: core::ops::Range<usize>,
    transient: bool,
    lm: usize,
) -> [i32; BANDS] {
    let mut budget = rc.len() as i32 * 8;
    let mut tell = rc.tell();
    let mut logp = if transient { 2 } else { 4 };
    let select_reserved = lm > 0 && tell + logp < budget;
    if select_reserved {
        budget -= 1;
    }
    let mut tf = [0i32; BANDS];
    let (mut current, mut changed) = (0, 0);
    for band in bands.clone() {
        if tell + logp <= budget {
            current ^= i32::from(rc.bit_logp(logp as u32));
            tell = rc.tell();
            changed |= current;
        }
        tf[band] = current;
        logp = if transient { 4 } else { 5 };
    }
    let row = &TF_SELECT[lm];
    let base = 4 * usize::from(transient);
    let changed = changed as usize;
    let select =
        select_reserved && row[base + changed] != row[base + 2 + changed] && rc.bit_logp(1);
    for band in bands {
        tf[band] = i32::from(row[base + 2 * usize::from(select) + tf[band] as usize]);
    }
    tf
}

/// Decode the dynamic band boosts and the allocation trim (RFC 6716 sections 4.3.3).
fn decode_boosts_and_trim(
    rc: &mut RangeDecoder<'_>,
    bands: core::ops::Range<usize>,
    caps: &[i32; BANDS],
    total_bits: i32,
    channels: usize,
    lm: usize,
) -> ([i32; BANDS], i32) {
    let mut boosts = [0i32; BANDS];
    let mut total = total_bits << BITRES;
    let mut tell = rc.tell_frac();
    let mut logp = 6;
    for band in bands {
        let width = (channels * band_width(band, lm)) as i32;
        // Six bits, but no more than one bit and no less than 1/8 bit per bin.
        let quanta = (width << BITRES).min((6 << BITRES).max(width));
        let mut loop_logp = logp;
        let mut boost = 0;
        while tell + (loop_logp << BITRES) < total && boost < caps[band] {
            let flag = rc.bit_logp(loop_logp as u32);
            tell = rc.tell_frac();
            if !flag {
                break;
            }
            boost += quanta;
            total -= quanta;
            loop_logp = 1;
        }
        boosts[band] = boost;
        // Later boosts become more likely.
        if boost > 0 {
            logp = (logp - 1).max(2);
        }
    }
    let trim = if tell + (6 << BITRES) <= total {
        rc.icdf(&TRIM_ICDF, 7) as i32
    } else {
        5
    };
    (boosts, trim)
}
