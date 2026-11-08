//! SAO syntax (7.3.8.3), coding quadtree (7.3.8.4) and coding unit (7.3.8.5) parsing,
//! including PCM samples and intra prediction mode derivation (8.4.2).

use super::{
    Cu, PART_2NX2N, PART_2NXN, PART_2NXND, PART_2NXNU, PART_NLX2N, PART_NRX2N, PART_NX2N, PART_NXN,
    SliceDecoder,
};
use crate::bits::{BitCursor, BitSource};
use crate::cabac::ctx;
use crate::error::{DecodeError, Result};
use crate::picture::{Pixel, PuMotion};
use crate::slice::SLICE_I;
use crate::state::*;

impl<P: Pixel> SliceDecoder<'_, P> {
    #[inline(never)]
    pub(super) fn sao_syntax(&mut self, rx: u32, ry: u32) -> Result<()> {
        let pic_w_ctb = self.sps.pic_w_ctb;
        let rs = self.ctb_rs;
        let layout = self.layout;
        let tile = layout.tile_id[self.ctb_ts as usize];
        let mut merge_left = false;
        let mut merge_up = false;
        if rx > 0 {
            let left_in_slice = rs > self.sh.slice_addr_rs;
            let left_in_tile =
                layout.tile_id[layout.ctb_addr_rs_to_ts[rs as usize - 1] as usize] == tile;
            if left_in_slice && left_in_tile {
                merge_left = self.cabac.decision(&mut self.c[ctx::SAO_MERGE]) != 0;
            }
        }
        if ry > 0 && !merge_left {
            let up = rs - pic_w_ctb;
            let up_in_slice = up >= self.sh.slice_addr_rs;
            let up_in_tile = layout.tile_id[layout.ctb_addr_rs_to_ts[up as usize] as usize] == tile;
            if up_in_slice && up_in_tile {
                merge_up = self.cabac.decision(&mut self.c[ctx::SAO_MERGE]) != 0;
            }
        }
        let mut p = SaoParams::default();
        if merge_left {
            p = self.st.sao[rs as usize - 1];
        } else if merge_up {
            p = self.st.sao[(rs - pic_w_ctb) as usize];
        } else {
            for c_idx in 0..3usize {
                let enabled = if c_idx == 0 {
                    self.sh.sao_luma
                } else {
                    self.sh.sao_chroma
                };
                if !enabled {
                    continue;
                }
                if c_idx == 0 || c_idx == 1 {
                    let t = if self.cabac.decision(&mut self.c[ctx::SAO_TYPE]) == 0 {
                        0
                    } else if self.cabac.bypass() == 0 {
                        1
                    } else {
                        2
                    };
                    p.type_idx[c_idx] = t;
                } else {
                    p.type_idx[2] = p.type_idx[1];
                    p.eo_class[2] = p.eo_class[1];
                }
                let t = p.type_idx[c_idx];
                if t == 0 {
                    continue;
                }
                let bd = if c_idx == 0 { self.bd_y } else { self.bd_c };
                let cmax = (1u32 << (bd.min(10) - 5)) - 1;
                let mut abs = [0i16; 4];
                for a in &mut abs {
                    let mut v = 0;
                    while v < cmax && self.cabac.bypass() == 1 {
                        v += 1;
                    }
                    *a = v as i16;
                }
                if t == 1 {
                    for a in &mut abs {
                        if *a != 0 && self.cabac.bypass() == 1 {
                            *a = -*a;
                        }
                    }
                    p.band_pos[c_idx] = self.cabac.bypass_bits(5) as u8;
                    p.offset[c_idx] = abs;
                } else {
                    p.offset[c_idx] = [abs[0], abs[1], -abs[2], -abs[3]];
                    if c_idx == 0 {
                        p.eo_class[0] = self.cabac.bypass_bits(2) as u8;
                    } else if c_idx == 1 {
                        p.eo_class[1] = self.cabac.bypass_bits(2) as u8;
                    }
                }
            }
        }
        if !self.sh.sao_luma {
            p.type_idx[0] = 0;
        }
        if !self.sh.sao_chroma {
            p.type_idx[1] = 0;
            p.type_idx[2] = 0;
        }
        self.st.sao[rs as usize] = p;
        Ok(())
    }

    // ------------------------------------------------------------------ coding quadtree

    pub(super) fn coding_quadtree(
        &mut self,
        x0: u32,
        y0: u32,
        log2: u32,
        depth: u32,
    ) -> Result<()> {
        let size = 1 << log2;
        let split = if x0 + size <= self.width && y0 + size <= self.height && log2 > self.min_cb {
            let mut inc = 0;
            if self.avail(x0, y0, x0 as i32 - 1, y0 as i32)
                && self.blk(x0 - 1, y0).depth as u32 > depth
            {
                inc += 1;
            }
            if self.avail(x0, y0, x0 as i32, y0 as i32 - 1)
                && self.blk(x0, y0 - 1).depth as u32 > depth
            {
                inc += 1;
            }
            self.cabac.decision(&mut self.c[ctx::SPLIT_CU + inc]) != 0
        } else {
            log2 > self.min_cb
        };
        if log2 >= self.log2_min_qg {
            if self.pps.cu_qp_delta_enabled {
                self.is_cu_qp_delta_coded = false;
                self.cu_qp_delta = 0;
            }
            self.start_qg(x0, y0);
        }
        if split {
            let h = size >> 1;
            let x1 = x0 + h;
            let y1 = y0 + h;
            self.coding_quadtree(x0, y0, log2 - 1, depth + 1)?;
            if x1 < self.width {
                self.coding_quadtree(x1, y0, log2 - 1, depth + 1)?;
            }
            if y1 < self.height {
                self.coding_quadtree(x0, y1, log2 - 1, depth + 1)?;
            }
            if x1 < self.width && y1 < self.height {
                self.coding_quadtree(x1, y1, log2 - 1, depth + 1)?;
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2, depth)
        }
    }

    /// Start of a quantization group: derive qPY_PRED (8.6.1).
    fn start_qg(&mut self, xq: u32, yq: u32) {
        let prev = self.last_qp_y;
        let mask = self.ctb_size - 1;
        let qa = if xq & mask != 0 {
            self.blk(xq - 1, yq).qp as i32
        } else {
            prev
        };
        let qb = if yq & mask != 0 {
            self.blk(xq, yq - 1).qp as i32
        } else {
            prev
        };
        self.qg_pred = (qa + qb + 1) >> 1;
        self.update_qp();
    }

    #[inline]
    pub(super) fn update_qp(&mut self) {
        let off = self.qp_bd_off;
        self.qp_y = ((self.qg_pred + self.cu_qp_delta + 52 + 2 * off) % (52 + off)) - off;
    }

    // ------------------------------------------------------------------ coding unit

    fn coding_unit(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> Result<()> {
        let n = 1u32 << log2;
        self.inner_edges = false;
        let bypass = if self.pps.transquant_bypass {
            self.cabac.decision(&mut self.c[ctx::CU_TRANSQUANT_BYPASS]) != 0
        } else {
            false
        };
        let mut skip = false;
        if self.sh.slice_type != SLICE_I {
            let mut inc = 0;
            if self.avail(x0, y0, x0 as i32 - 1, y0 as i32)
                && self.blk(x0 - 1, y0).flags & F_SKIP != 0
            {
                inc += 1;
            }
            if self.avail(x0, y0, x0 as i32, y0 as i32 - 1)
                && self.blk(x0, y0 - 1).flags & F_SKIP != 0
            {
                inc += 1;
            }
            skip = self.cabac.decision(&mut self.c[ctx::CU_SKIP + inc]) != 0;
        }
        self.update_qp();
        let d = depth as u8;
        let mut cu = Cu {
            x0,
            y0,
            log2,
            intra: false,
            part: PART_2NX2N,
            bypass,
            max_trafo_depth: 0,
            intra_split: false,
            chroma_mode: 0,
        };
        let mut merge_2nx2n = false;
        let mut pcm = false;
        // QpY is stored with the block info now and rewritten at the end of the CU only if
        // a cu_qp_delta in this CU changed it.
        let qp_start = self.qp_y;
        let info = |flags: u8| BlkInfo {
            flags,
            depth: d,
            ipm: 1,
            qp: qp_start as i8,
        };
        if skip {
            self.fill_blk(x0, y0, n, n, info(F_SKIP));
            self.prediction_unit(&cu, x0, y0, n, n, 0, true)?;
        } else {
            let intra = if self.sh.slice_type != SLICE_I {
                self.cabac.decision(&mut self.c[ctx::PRED_MODE]) != 0
            } else {
                true
            };
            cu.intra = intra;
            // part_mode
            if !intra || log2 == self.min_cb {
                cu.part = self.parse_part_mode(intra, log2);
            }
            cu.intra_split = intra && cu.part == PART_NXN;
            let flags = if intra { F_INTRA } else { 0 };
            self.fill_blk(x0, y0, n, n, info(flags));
            if intra {
                // Intra blocks have no motion.
                self.clear_motion(x0, y0, n, n);
                if cu.part == PART_2NX2N
                    && self.sps.pcm
                    && log2 >= self.sps.log2_min_pcm
                    && log2 <= self.sps.log2_max_pcm
                {
                    pcm = self.cabac.terminate() != 0;
                }
                if pcm {
                    self.pcm_sample(x0, y0, log2)?;
                    let f = if self.sps.pcm_loop_filter_disabled {
                        self.st.any_nofilter = true;
                        F_NOFILTER | F_PCM
                    } else {
                        F_PCM
                    };
                    self.set_blk_area(x0, y0, n, n, |b| b.flags |= f);
                } else {
                    self.parse_intra_modes(&mut cu)?;
                }
            } else {
                match cu.part {
                    PART_2NX2N => {
                        merge_2nx2n = self.prediction_unit(&cu, x0, y0, n, n, 0, false)?;
                    }
                    PART_2NXN => {
                        self.prediction_unit(&cu, x0, y0, n, n / 2, 0, false)?;
                        self.prediction_unit(&cu, x0, y0 + n / 2, n, n / 2, 1, false)?;
                    }
                    PART_NX2N => {
                        self.prediction_unit(&cu, x0, y0, n / 2, n, 0, false)?;
                        self.prediction_unit(&cu, x0 + n / 2, y0, n / 2, n, 1, false)?;
                    }
                    PART_2NXNU => {
                        self.prediction_unit(&cu, x0, y0, n, n / 4, 0, false)?;
                        self.prediction_unit(&cu, x0, y0 + n / 4, n, n * 3 / 4, 1, false)?;
                    }
                    PART_2NXND => {
                        self.prediction_unit(&cu, x0, y0, n, n * 3 / 4, 0, false)?;
                        self.prediction_unit(&cu, x0, y0 + n * 3 / 4, n, n / 4, 1, false)?;
                    }
                    PART_NLX2N => {
                        self.prediction_unit(&cu, x0, y0, n / 4, n, 0, false)?;
                        self.prediction_unit(&cu, x0 + n / 4, y0, n * 3 / 4, n, 1, false)?;
                    }
                    PART_NRX2N => {
                        self.prediction_unit(&cu, x0, y0, n * 3 / 4, n, 0, false)?;
                        self.prediction_unit(&cu, x0 + n * 3 / 4, y0, n / 4, n, 1, false)?;
                    }
                    _ => {
                        let h = n / 2;
                        self.prediction_unit(&cu, x0, y0, h, h, 0, false)?;
                        self.prediction_unit(&cu, x0 + h, y0, h, h, 1, false)?;
                        self.prediction_unit(&cu, x0, y0 + h, h, h, 2, false)?;
                        self.prediction_unit(&cu, x0 + h, y0 + h, h, h, 3, false)?;
                    }
                }
            }
            if !pcm {
                let rqt_root_cbf = if !intra && !(cu.part == PART_2NX2N && merge_2nx2n) {
                    self.cabac.decision(&mut self.c[ctx::RQT_ROOT_CBF]) != 0
                } else {
                    true
                };
                if rqt_root_cbf {
                    cu.max_trafo_depth = if intra {
                        self.sps.max_th_depth_intra + cu.intra_split as u32
                    } else {
                        self.sps.max_th_depth_inter
                    };
                    self.transform_tree(&cu, x0, y0, x0, y0, log2, 0, 0, false, false)?;
                }
            }
        }
        if bypass {
            self.st.any_nofilter = true;
            self.set_blk_area(x0, y0, n, n, |b| b.flags |= F_NOFILTER);
        }
        // Final QpY for this CU (deblocking, prediction of later groups).
        if self.qp_y != qp_start {
            let qp = self.qp_y as i8;
            self.set_blk_area(x0, y0, n, n, |b| b.qp = qp);
        }
        self.last_qp_y = self.qp_y;
        if !self.sh.deblocking_disabled {
            self.deblock_cu_edges(&cu);
        }
        Ok(())
    }

    fn clear_motion(&mut self, x0: u32, y0: u32, w: u32, h: u32) {
        self.fill_motion(x0, y0, w, h, PuMotion::default());
    }

    pub(super) fn fill_motion(&mut self, x0: u32, y0: u32, w: u32, h: u32, m: PuMotion) {
        let w4 = self.st.w4;
        let (xa, xb) = ((x0 >> 2) as usize, ((x0 + w) >> 2) as usize);
        for y in (y0 >> 2) as usize..((y0 + h) >> 2) as usize {
            self.st.motion[y * w4 + xa..y * w4 + xb].fill(m);
        }
    }

    fn parse_part_mode(&mut self, intra: bool, log2: u32) -> u8 {
        if self.cabac.decision(&mut self.c[ctx::PART_MODE]) != 0 {
            return PART_2NX2N;
        }
        if intra {
            return PART_NXN;
        }
        if log2 == self.min_cb {
            if self.cabac.decision(&mut self.c[ctx::PART_MODE + 1]) != 0 {
                return PART_2NXN;
            }
            if log2 == 3 {
                return PART_NX2N;
            }
            if self.cabac.decision(&mut self.c[ctx::PART_MODE + 2]) != 0 {
                return PART_NX2N;
            }
            return PART_NXN;
        }
        let horizontal = self.cabac.decision(&mut self.c[ctx::PART_MODE + 1]) != 0;
        if !self.sps.amp {
            return if horizontal { PART_2NXN } else { PART_NX2N };
        }
        if horizontal {
            if self.cabac.decision(&mut self.c[ctx::PART_MODE + 3]) != 0 {
                return PART_2NXN;
            }
            if self.cabac.bypass() == 0 {
                PART_2NXNU
            } else {
                PART_2NXND
            }
        } else {
            if self.cabac.decision(&mut self.c[ctx::PART_MODE + 3]) != 0 {
                return PART_NX2N;
            }
            if self.cabac.bypass() == 0 {
                PART_NLX2N
            } else {
                PART_NRX2N
            }
        }
    }

    fn pcm_sample(&mut self, x0: u32, y0: u32, log2: u32) -> Result<()> {
        let pos = self.cabac.aligned_byte_position();
        if pos > self.data.len() {
            return Err(DecodeError::InvalidData("PCM data overrun"));
        }
        let mut r = BitCursor::new(&self.data[pos..]);
        let n = 1usize << log2;
        let bd_y = self.sps.pcm_bit_depth_luma;
        let bd_c = self.sps.pcm_bit_depth_chroma;
        let sy = self.bd_y - bd_y;
        let scy = self.bd_c - bd_c;
        {
            let pl = &mut self.frame.planes[0];
            let stride = pl.stride;
            for j in 0..n {
                for i in 0..n {
                    let v = r.read(bd_y)? as i32;
                    pl.data[(y0 as usize + j) * stride + x0 as usize + i] = P::from_i32(v << sy);
                }
            }
        }
        let nc = n / 2;
        for pl in &mut self.frame.planes[1..] {
            let stride = pl.stride;
            for j in 0..nc {
                for i in 0..nc {
                    let v = r.read(bd_c)? as i32;
                    pl.data[(y0 as usize / 2 + j) * stride + x0 as usize / 2 + i] =
                        P::from_i32(v << scy);
                }
            }
        }
        let consumed = r.position().div_ceil(8);
        self.cabac.restart(pos + consumed)?;
        Ok(())
    }

    #[inline(never)]
    fn parse_intra_modes(&mut self, cu: &mut Cu) -> Result<()> {
        let n = 1u32 << cu.log2;
        let nparts = if cu.part == PART_NXN { 4 } else { 1 };
        let pb = if nparts == 4 { n / 2 } else { n };
        let mut prev = [false; 4];
        for p in prev.iter_mut().take(nparts) {
            *p = self.cabac.decision(&mut self.c[ctx::PREV_INTRA_LUMA_PRED]) != 0;
        }
        for i in 0..nparts {
            let xp = cu.x0 + (i as u32 & 1) * pb;
            let yp = cu.y0 + (i as u32 >> 1) * pb;
            // Candidate list (8.4.2).
            let cand_a = self.cand_intra_mode(xp, yp, xp as i32 - 1, yp as i32, false);
            let cand_b = self.cand_intra_mode(xp, yp, xp as i32, yp as i32 - 1, true);
            let mut list = if cand_a == cand_b {
                if cand_a < 2 {
                    [0, 1, 26]
                } else {
                    [
                        cand_a,
                        2 + ((cand_a + 29) % 32),
                        2 + ((cand_a - 2 + 1) % 32),
                    ]
                }
            } else {
                let third = if cand_a != 0 && cand_b != 0 {
                    0
                } else if cand_a != 1 && cand_b != 1 {
                    1
                } else {
                    26
                };
                [cand_a, cand_b, third]
            };
            let mode = if prev[i] {
                // Each call reads the next bypass bin (truncated unary, 9.3.3.2).
                #[allow(clippy::same_functions_in_if_condition)]
                let idx = if self.cabac.bypass() == 0 {
                    0
                } else if self.cabac.bypass() == 0 {
                    1
                } else {
                    2
                };
                list[idx]
            } else {
                let mut m = self.cabac.bypass_bits(5);
                if list[0] > list[1] {
                    list.swap(0, 1);
                }
                if list[0] > list[2] {
                    list.swap(0, 2);
                }
                if list[1] > list[2] {
                    list.swap(1, 2);
                }
                for &c in &list {
                    if m >= c {
                        m += 1;
                    }
                }
                m
            };
            self.ipm[i] = mode;
            let mb = mode as u8;
            self.set_blk_area(xp, yp, pb, pb, |b| b.ipm = mb);
        }
        // intra_chroma_pred_mode (4:2:0: one per CU), Table 8-2.
        let icpm = if self
            .cabac
            .decision(&mut self.c[ctx::INTRA_CHROMA_PRED_MODE])
            == 0
        {
            4
        } else {
            self.cabac.bypass_bits(2)
        };
        let ym = self.ipm[0];
        cu.chroma_mode = match icpm {
            4 => ym,
            k => {
                let m = [0u32, 26, 10, 1][k as usize];
                if m == ym { 34 } else { m }
            }
        };
        if nparts == 1 {
            self.ipm[1] = self.ipm[0];
            self.ipm[2] = self.ipm[0];
            self.ipm[3] = self.ipm[0];
        }
        Ok(())
    }

    fn cand_intra_mode(&self, xp: u32, yp: u32, xn: i32, yn: i32, is_b: bool) -> u32 {
        if !self.avail(xp, yp, xn, yn) {
            return 1;
        }
        let b = self.blk(xn as u32, yn as u32);
        if b.flags & F_INTRA == 0 || b.flags & F_PCM != 0 {
            return 1;
        }
        if is_b && (yn as u32) < ((yp >> self.log2_ctb) << self.log2_ctb) {
            return 1;
        }
        b.ipm as u32
    }
}
