//! Output stages: filter bank setup and history, output plane layout, subband-domain
//! post-processing (X96 recombination, XCh down-mix removal, sum/difference), QMF
//! synthesis and LFE interpolation.

use super::CoreFrame;
use crate::qmf::{Qmf, QmfState};
use crate::subframe::{self, HIST, SubBuf};
use crate::{
    AudioFrame, ChannelLayout, DecodeError, Decoder, MAX_SLOTS, OUT_SCALE, QMF64_GAIN, Speaker,
    tables,
};

impl Decoder {
    /// Resets filter bank histories on an output rate change, allocates the 64-band
    /// filter bank on first use and saves the subband histories for the next frame.
    #[inline(always)]
    pub(super) fn prepare_synthesis(&mut self, f: &CoreFrame<'_>, nslots: usize, have_x96: bool) {
        if have_x96 != self.prev_x96 {
            // Output rate changes: filter bank histories do not carry over.
            for q in &mut self.qstate {
                q.reset();
            }
            for q in &mut self.q64state {
                q.reset();
            }
            self.lfe2x.reset();
        }
        self.prev_x96 = have_x96;
        if have_x96 && self.qmf64.is_none() {
            self.qmf64 = Some(Qmf::new(&tables::X96_FIR, QMF64_GAIN * OUT_SCALE));
            self.q64state = (0..MAX_SLOTS).map(|_| QmfState::new(64)).collect();
        }
        let blocks = f.h.blocks;

        // Histories for the next frame are taken before any in-place post-processing.
        self.sub.save_history(0..nslots, blocks);
        if have_x96 && let Some(xb) = self.x96buf.as_mut() {
            xb.save_history(0..f.set.nch, blocks);
        }
    }

    /// Subband-domain post-processing, in place on the synthesis source rows.
    #[inline(always)]
    pub(super) fn postprocess_subbands(
        &mut self,
        f: &CoreFrame<'_>,
        xch: bool,
        have_x96: bool,
    ) -> Result<(), DecodeError> {
        let (h, set) = (&f.h, &f.set);
        let blocks = h.blocks;
        if have_x96 {
            // X96 recombination: core subband samples join the residual's lower 32 bands
            // (no saturation, observed `x96_s58` / `x96_s60`).
            let xbuf = self
                .x96buf
                .as_mut()
                .ok_or(DecodeError::InvalidData("X96 buffer"))?;
            for ch in 0..set.nch {
                for b in 0..32 {
                    let core_row = self.sub.row(ch, b);
                    let row = xbuf.row_mut(ch, b);
                    for t in HIST..HIST + blocks {
                        row[t] += core_row[t];
                    }
                }
            }
        }
        let mut cs_rows = [[0f32; subframe::MAX_T]; 32];
        if xch {
            for (b, dst) in cs_rows.iter_mut().enumerate() {
                dst[..blocks].copy_from_slice(&self.sub.row(set.nch, b)[HIST..HIST + blocks]);
            }
        }
        let x96buf = self.x96buf.as_mut();
        let (src, sbands): (&mut SubBuf, usize) = match x96buf {
            Some(xb) if have_x96 => (xb, 64),
            _ => (&mut self.sub, 32),
        };
        if xch {
            // XCh carries Cs; the core surrounds hold Ls + 0.7071 Cs and
            // Rs + 0.7071 Cs (clause 6.4.1). Remove the embedded down-mix.
            if let Some((l, r)) = surround_pair(h.amode) {
                for (b, cs) in cs_rows.iter().enumerate() {
                    for (pair_ch, t) in [l, r].into_iter().zip([0, 0]) {
                        let _ = t;
                        let row = &mut src.row_mut(pair_ch, b)[HIST..HIST + blocks];
                        for (v, &c) in row.iter_mut().zip(cs.iter()) {
                            *v -= c * core::f32::consts::FRAC_1_SQRT_2;
                        }
                    }
                }
            }
        }
        let fpair = if h.sumf || h.amode == 3 {
            front_pair(h.amode)
        } else {
            None
        };
        let spair = if h.sums { surround_pair(h.amode) } else { None };
        for (l, r) in [fpair, spair].into_iter().flatten() {
            for b in 0..sbands {
                let mut rr = [0f32; subframe::MAX_T];
                rr[..blocks].copy_from_slice(&src.row(r, b)[HIST..HIST + blocks]);
                let mut ll = [0f32; subframe::MAX_T];
                ll[..blocks].copy_from_slice(&src.row(l, b)[HIST..HIST + blocks]);
                for (t, v) in src.row_mut(l, b)[HIST..HIST + blocks]
                    .iter_mut()
                    .enumerate()
                {
                    *v = ll[t] + rr[t];
                }
                for (t, v) in src.row_mut(r, b)[HIST..HIST + blocks]
                    .iter_mut()
                    .enumerate()
                {
                    *v = ll[t] - rr[t];
                }
            }
        }
        Ok(())
    }

    /// Synthesis: transpose each slot to time-major blocks and run the filter bank
    /// straight into its output plane.
    #[inline(always)]
    pub(super) fn synthesize(
        &mut self,
        out: &mut AudioFrame,
        f: &CoreFrame<'_>,
        nslots: usize,
        have_x96: bool,
        plane_of: &[usize; MAX_SLOTS],
    ) -> Result<(), DecodeError> {
        let h = &f.h;
        let bands = if have_x96 { 64 } else { 32 };
        let blocks = h.blocks;
        let qmf = if have_x96 {
            self.qmf64
                .as_ref()
                .ok_or(DecodeError::InvalidData("X96 filter bank"))?
        } else if h.filts {
            &self.qmf_pr
        } else {
            &self.qmf_np
        };
        if self.tbuf.len() < blocks * bands {
            self.tbuf.resize(blocks * bands, 0.0);
        }
        for ch in 0..nslots {
            let (srcbuf, sb): (&SubBuf, usize) = match self.x96buf.as_ref() {
                Some(xb) if have_x96 && ch < f.set.nch => (xb, 64),
                _ => (&self.sub, 32),
            };
            let tb = &mut self.tbuf[..blocks * bands];
            if sb < bands {
                tb.iter_mut().for_each(|v| *v = 0.0);
            }
            for b in 0..sb {
                let row = &srcbuf.row(ch, b)[HIST..HIST + blocks];
                for (t, &v) in row.iter().enumerate() {
                    tb[t * bands + b] = v;
                }
            }
            let st = if have_x96 {
                &mut self.q64state[ch]
            } else {
                &mut self.qstate[ch]
            };
            let plane = &mut out.planes[plane_of[ch]];
            for t in 0..blocks {
                qmf.synth(
                    st,
                    &mut tb[t * bands..t * bands + bands],
                    &mut plane[t * bands..t * bands + bands],
                );
            }
        }
        Ok(())
    }

    /// LFE interpolation into output plane `lp` (with the 2x stage at the X96 rate).
    #[inline(always)]
    pub(super) fn synthesize_lfe(
        &mut self,
        out: &mut AudioFrame,
        lp: usize,
        f: &CoreFrame<'_>,
        have_x96: bool,
    ) {
        let h = &f.h;
        let blocks = h.blocks;
        let nsamples = blocks * if have_x96 { 64 } else { 32 };
        let factor = if h.lff == 1 { 128 } else { 64 };
        let n48 = blocks * 32;
        let ndec = n48 / factor;
        let plane = &mut out.planes[lp];
        if have_x96 {
            if self.lfe48.len() < n48 {
                self.lfe48.resize(n48, 0.0);
            }
            self.lfe_state.interpolate(
                &self.lfe_fir,
                factor,
                &self.lfe_dec[..ndec],
                &mut self.lfe48[..n48],
            );
            self.lfe2x
                .interpolate(&self.lfe48[..n48], &mut plane[..nsamples]);
        } else {
            self.lfe_state.interpolate(
                &self.lfe_fir,
                factor,
                &self.lfe_dec[..ndec],
                &mut plane[..nsamples],
            );
        }
    }
}

/// Output plane order (FFmpeg native order) and output format. Returns the plane of
/// every coded slot and the LFE plane.
#[inline(always)]
pub(super) fn output_layout(
    out: &mut AudioFrame,
    f: &CoreFrame<'_>,
    xch: bool,
    have_x96: bool,
) -> ([usize; MAX_SLOTS], Option<usize>) {
    let (h, set) = (&f.h, &f.set);
    let nsamples = h.blocks * if have_x96 { 64 } else { 32 };
    let mut order: [(Speaker, usize); MAX_SLOTS + 1] = [(Speaker::FrontLeft, 0); MAX_SLOTS + 1];
    let mut n = 0;
    for (ch, &sp) in f.speakers.iter().enumerate() {
        order[n] = (sp, ch);
        n += 1;
    }
    if xch {
        order[n] = (Speaker::BackCenter, set.nch);
        n += 1;
    }
    const LFE_SRC: usize = usize::MAX;
    if h.lff > 0 {
        order[n] = (Speaker::LowFrequency, LFE_SRC);
        n += 1;
    }
    let order = &mut order[..n];
    order.sort_by_key(|e| e.0);
    let mut plane_of = [0usize; MAX_SLOTS];
    let mut lfe_plane = None;
    out.sample_rate = if have_x96 {
        h.sample_rate * 2
    } else {
        h.sample_rate
    };
    out.samples_per_channel = nsamples;
    out.layout = ChannelLayout::new(order.iter().map(|e| e.0)).expect("at most 9 planes");
    out.planes.resize_with(n, Vec::new);
    for (i, &(_, src)) in order.iter().enumerate() {
        out.planes[i].resize(nsamples, 0.0);
        if src == LFE_SRC {
            lfe_plane = Some(i);
        } else {
            plane_of[src] = i;
        }
    }
    (plane_of, lfe_plane)
}

/// Coded slot indices of the front and surround L/R pairs for sum/difference decoding.
fn front_pair(amode: u8) -> Option<(usize, usize)> {
    match amode {
        2..=4 | 6 | 8 => Some((0, 1)),
        5 | 7 | 9 => Some((1, 2)),
        _ => None,
    }
}

fn surround_pair(amode: u8) -> Option<(usize, usize)> {
    match amode {
        8 => Some((2, 3)),
        9 => Some((3, 4)),
        _ => None,
    }
}
