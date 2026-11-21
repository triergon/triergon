//! Picture analysis before coding: the motion vector and intra or inter decision of
//! every macroblock of a P picture, the scene-change test that turns a P picture into
//! an I picture, and the per-row activity the rate model plans with.

use crate::motion::{self, Bounds, Coarse, Mv, Search};
use crate::syntax::PictureType;
use crate::{Encoder, Result};
use triergon_pixel::Picture;

/// Search window in half-samples (±64 luma samples).
const SEARCH_RANGE: i32 = 128;
const MAX_F_CODE: u32 = 4;
/// A P macroblock is intra coded when its deviation from its own mean beats
/// the motion-compensated SAD by this margin (intra blocks cost more bits).
const INTRA_MARGIN: u32 = 512;

/// The analysis of one macroblock: its searched vector, the SAD there, and whether
/// it is intra coded.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Decision {
    pub(crate) mv: Mv,
    pub(crate) sad: u32,
    pub(crate) intra: bool,
}

/// How a source picture repeats the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repetition {
    /// The luma differs, or the picture was not compared.
    None,
    /// The luma repeats and the chroma differs.
    Luma,
    /// Every plane repeats.
    Picture,
}

impl Encoder {
    /// How `input` repeats the previous source picture.
    pub(crate) fn repetition(&self, input: &Picture) -> Repetition {
        if input.y() != &self.previous_luma[..] {
            return Repetition::None;
        }
        let (previous_cb, previous_cr) = self.previous_chroma.split_at(input.cb().len());
        if input.cb() == previous_cb && input.cr() == previous_cr {
            Repetition::Picture
        } else {
            Repetition::Luma
        }
    }

    /// Keep `input` as the previous source picture, copying the planes that changed.
    pub(crate) fn remember_source(&mut self, input: &Picture, repetition: Repetition) {
        if repetition == Repetition::None {
            self.previous_luma.copy_from_slice(input.y());
        }
        if repetition != Repetition::Picture {
            let (cb, cr) = self.previous_chroma.split_at_mut(input.cb().len());
            cb.copy_from_slice(input.cb());
            cr.copy_from_slice(input.cr());
        }
    }

    /// Keep the coded picture's vectors as the temporal predictors of the next
    /// search: zero for intra macroblocks and after an I picture.
    pub(crate) fn remember_vectors(&mut self, picture_type: PictureType) {
        match picture_type {
            PictureType::Predicted => {
                for (prev, a) in self.previous_mvs.iter_mut().zip(self.analysis.iter()) {
                    *prev = if a.intra { Mv::ZERO } else { a.mv };
                }
            }
            PictureType::Intra => self.previous_mvs.fill(Mv::ZERO),
        }
    }

    /// Motion analysis for a P picture; returns the forward f_code covering
    /// every chosen vector and whether the picture is a scene change.
    ///
    /// Scene score: Σ over macroblocks of √min(SSE, V + 100λ) − √(V + 20λ),
    /// with SSE the difference to the previous *source* picture at the
    /// full-sample part of the chosen vector, V the block's variance·256 and
    /// λ ≈ 0.85·q²; a positive total means the content changed more than
    /// intra coding costs. Comparing sources keeps the decision about the
    /// content rather than about the reconstruction's quantisation.
    pub(crate) fn analyse(
        &mut self,
        input: &Picture,
        same_source_luma: bool,
    ) -> Result<(u32, bool)> {
        let g = self.cfg.geometry;
        let stride = g.luma_stride();
        let lambda = self.rc.last_q.clamp(2, 31);
        let scene_lambda = (85 * lambda * lambda) as f64 / 100.0;
        let scene_floor = (20.0 * scene_lambda).sqrt();
        let mut scene = 0.0f64;
        let mut unchanged = 0;
        let (mut lo, mut hi) = (0i32, 0i32);
        let coarse = if same_source_luma {
            None
        } else {
            let rows = g.macroblock_height() * 16;
            let width = stride / 4;
            let plane_len = width * (rows / 4);
            if self.coarse_planes.is_empty() {
                self.coarse_planes.try_resize(2 * plane_len, 0)?;
            }
            let (cur, reference) = self.coarse_planes.split_at_mut(plane_len);
            motion::decimate4(input.y(), stride, rows, cur);
            motion::decimate4(self.reference.y(), stride, rows, reference);
            Some(Coarse {
                cur,
                reference,
                width,
            })
        };
        for mb_y in 0..g.macroblock_height() {
            let mut row = 0.0;
            for mb_x in 0..g.macroblock_width() {
                let i = mb_y * g.macroblock_width() + mb_x;
                let (x, y) = (mb_x * 16, mb_y * 16);
                let (mv, sad) = if let Some(coarse) = &coarse {
                    let left = if mb_x > 0 {
                        self.analysis[i - 1].mv
                    } else {
                        Mv::ZERO
                    };
                    let mut candidates = [left, self.previous_mvs[i], Mv::ZERO, Mv::ZERO, Mv::ZERO];
                    if mb_y > 0 {
                        candidates[2] = self.analysis[i - g.macroblock_width()].mv;
                        if mb_x + 1 < g.macroblock_width() {
                            candidates[3] = self.analysis[i - g.macroblock_width() + 1].mv;
                        }
                    }
                    if mb_x + 1 < g.macroblock_width() {
                        candidates[4] = self.previous_mvs[i + 1];
                    }
                    let search = Search {
                        cur: input.y(),
                        reference: self.reference.y(),
                        stride,
                        x,
                        y,
                        bounds: Bounds::new(&g, mb_x, mb_y, SEARCH_RANGE),
                        pred: left,
                        lambda,
                        f_code: 2,
                    };
                    let neighbour = if mb_x > 0 {
                        self.analysis[i - 1].sad
                    } else {
                        u32::MAX
                    };
                    search.run(&candidates, coarse, mb_x, mb_y, neighbour)
                } else {
                    // Repeated luma predicts at zero motion, even when quantisation
                    // changed the reconstructed reference.
                    let origin = y * stride + x;
                    let zero = motion::sad16(
                        &input.y()[origin..],
                        stride,
                        &self.reference.y()[origin..],
                        stride,
                    );
                    (Mv::ZERO, zero)
                };
                // Intra only pays off when prediction is poor; skip the
                // deviation pass otherwise.
                let deviation = if sad > INTRA_MARGIN {
                    intra_activity(&input.y()[y * stride + x..], stride)
                } else {
                    u32::MAX
                };
                let intra = deviation.saturating_add(INTRA_MARGIN) < sad;
                // Rate-model activity: intra deviation or compensated SAD.
                row += if intra { deviation } else { sad } as f64;
                let origin = y * stride + x;
                let block = &input.y()[origin..];
                let full =
                    (y as i32 + (mv.y >> 1)) as usize * stride + (x as i32 + (mv.x >> 1)) as usize;
                // The scene metric compares two source blocks. A repeated
                // source proves equality only at the same integer offset;
                // motion-compensated blocks still need their own comparison.
                let sse = if sad == 0 || (same_source_luma && full == origin) {
                    0
                } else {
                    motion::sse16(block, stride, &self.previous_luma[full..], stride)
                };
                if sse == 0 {
                    // Unchanged content: its exact term −√(V + 20λ) is at most
                    // −√(20λ); the variance is only needed if that bound leaves
                    // the decision open (below).
                    scene -= scene_floor;
                    self.unchanged[unchanged] = i as u32;
                    unchanged += 1;
                } else {
                    let variance = variance256(block, stride) as f64;
                    scene += (sse as f64).min(variance + 100.0 * scene_lambda).sqrt()
                        - (variance + 20.0 * scene_lambda).sqrt();
                }
                self.analysis[i] = Decision { mv, sad, intra };
                if !intra {
                    lo = lo.min(mv.x).min(mv.y);
                    hi = hi.max(mv.x).max(mv.y);
                }
            }
            self.row_activity[mb_y] = row;
        }
        if scene > 0.0 {
            for &i in &self.unchanged[..unchanged] {
                let (x, y) = (
                    (i as usize % g.macroblock_width()) * 16,
                    (i as usize / g.macroblock_width()) * 16,
                );
                let variance = variance256(&input.y()[y * stride + x..], stride) as f64;
                scene -= (variance + 20.0 * scene_lambda).sqrt() - scene_floor;
            }
        }
        let f_code = (1..=MAX_F_CODE)
            .find(|&f| lo >= -(16 << (f - 1)) && hi < 16 << (f - 1))
            .unwrap_or(MAX_F_CODE);
        Ok((f_code, scene > 0.0))
    }

    /// Per-row intra activity of an I picture.
    pub(crate) fn intra_analysis(&mut self, input: &Picture) {
        let g = self.cfg.geometry;
        let stride = g.luma_stride();
        for mb_y in 0..g.macroblock_height() {
            let mut row = 0.0;
            for mb_x in 0..g.macroblock_width() {
                let cost = intra_activity(&input.y()[mb_y * 16 * stride + mb_x * 16..], stride);
                self.analysis[mb_y * g.macroblock_width() + mb_x] = Decision {
                    mv: Mv::ZERO,
                    sad: 0,
                    intra: true,
                };
                row += cost as f64;
            }
            self.row_activity[mb_y] = row;
        }
    }
}

/// Σx² − (Σx)²/256 over a 16×16 block: 256 × its variance.
fn variance256(block: &[u8], stride: usize) -> u32 {
    // Per lane: at most 16 × 255 in the sum, 16 × 255² in the squares, each
    // square formed in 16 bits.
    let (mut sum, mut squares) = ([0u16; 16], [0u32; 16]);
    for r in 0..16 {
        let row: &[u8; 16] = block[r * stride..r * stride + 16].try_into().unwrap();
        for i in 0..16 {
            let v = u16::from(row[i]);
            sum[i] += v;
            squares[i] += u32::from(v * v);
        }
    }
    let sum: u32 = sum.iter().map(|&v| u32::from(v)).sum();
    let squares: u32 = squares.iter().sum();
    squares - (sum * sum) / 256
}

/// Mean absolute deviation of a 16×16 block from its mean (intra cost proxy).
fn intra_activity(block: &[u8], stride: usize) -> u32 {
    let mut sum = 0u32;
    for r in 0..16 {
        sum += block[r * stride..r * stride + 16]
            .iter()
            .map(|&v| v as u32)
            .sum::<u32>();
    }
    let mean = (sum + 128) / 256;
    let mut dev = 0u32;
    for r in 0..16 {
        dev += block[r * stride..r * stride + 16]
            .iter()
            .map(|&v| (v as u32).abs_diff(mean))
            .sum::<u32>();
    }
    dev
}
