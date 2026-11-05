//! Parameter sets: VPS (clause 7.3.2.1), SPS (7.3.2.2), PPS (7.3.2.3), profile/tier/level
//! (7.3.3), scaling lists (7.3.4 / 7.4.5), short-term RPS (7.3.7 / 7.4.8), VUI (E.2.1).

use crate::bits::BitReader;
use crate::error::{DecodeError, Result};
use crate::tables::{DEFAULT_SCALING_INTER, DEFAULT_SCALING_INTRA, DIAG_SCAN_4X4, DIAG_SCAN_8X8};

pub const MAX_SPS: usize = 16;
pub const MAX_PPS: usize = 64;
pub const MAX_VPS: usize = 16;
/// Level 6.2 MaxLumaPs (Table A.8).
pub const MAX_LUMA_PS: u64 = 35_651_584;
pub const MAX_DIM: u32 = 16888;

#[derive(Clone, Debug, Default)]
pub struct ShortTermRps {
    pub num_negative: u8,
    pub num_positive: u8,
    /// DeltaPocS0 followed by DeltaPocS1.
    pub delta_poc: [i32; 32],
    pub used: [bool; 32],
}

impl ShortTermRps {
    #[inline]
    pub fn num_delta_pocs(&self) -> usize {
        self.num_negative as usize + self.num_positive as usize
    }
    #[inline]
    pub fn s0(&self, i: usize) -> (i32, bool) {
        (self.delta_poc[i], self.used[i])
    }
    #[inline]
    pub fn s1(&self, i: usize) -> (i32, bool) {
        let j = self.num_negative as usize + i;
        (self.delta_poc[j], self.used[j])
    }
}

/// st_ref_pic_set( stRpsIdx ). `sets` holds candidate RPSs 0..stRpsIdx-1.
pub fn parse_st_rps(
    r: &mut BitReader,
    st_rps_idx: usize,
    num_short_term_ref_pic_sets: usize,
    sets: &[ShortTermRps],
    max_dec_pic_buffering_minus1: u32,
) -> Result<ShortTermRps> {
    let mut out = ShortTermRps::default();
    let inter = if st_rps_idx != 0 { r.flag()? } else { false };
    if inter {
        let delta_idx_minus1 = if st_rps_idx == num_short_term_ref_pic_sets {
            r.ue_max(st_rps_idx as u32 - 1, "delta_idx_minus1")?
        } else {
            0
        };
        let ref_idx = st_rps_idx - (delta_idx_minus1 as usize + 1);
        let rf = &sets[ref_idx];
        let delta_rps_sign = r.flag()?;
        let abs_delta_rps_minus1 = r.ue_max(32767, "abs_delta_rps_minus1")? as i32;
        let delta_rps = if delta_rps_sign {
            -(abs_delta_rps_minus1 + 1)
        } else {
            abs_delta_rps_minus1 + 1
        };
        let n = rf.num_delta_pocs();
        let mut used_by_curr = [false; 33];
        let mut use_delta = [true; 33];
        for j in 0..=n {
            used_by_curr[j] = r.flag()?;
            if !used_by_curr[j] {
                use_delta[j] = r.flag()?;
            }
        }
        let nneg = rf.num_negative as usize;
        let npos = rf.num_positive as usize;
        // (7-61)
        let mut s0: Vec<(i32, bool)> = Vec::with_capacity(16);
        for j in (0..npos).rev() {
            let dpoc = rf.delta_poc[nneg + j] + delta_rps;
            if dpoc < 0 && use_delta[nneg + j] {
                s0.push((dpoc, used_by_curr[nneg + j]));
            }
        }
        if delta_rps < 0 && use_delta[n] {
            s0.push((delta_rps, used_by_curr[n]));
        }
        for j in 0..nneg {
            let dpoc = rf.delta_poc[j] + delta_rps;
            if dpoc < 0 && use_delta[j] {
                s0.push((dpoc, used_by_curr[j]));
            }
        }
        // (7-62)
        let mut s1: Vec<(i32, bool)> = Vec::with_capacity(16);
        for j in (0..nneg).rev() {
            let dpoc = rf.delta_poc[j] + delta_rps;
            if dpoc > 0 && use_delta[j] {
                s1.push((dpoc, used_by_curr[j]));
            }
        }
        if delta_rps > 0 && use_delta[n] {
            s1.push((delta_rps, used_by_curr[n]));
        }
        for j in 0..npos {
            let dpoc = rf.delta_poc[nneg + j] + delta_rps;
            if dpoc > 0 && use_delta[nneg + j] {
                s1.push((dpoc, used_by_curr[nneg + j]));
            }
        }
        if s0.len() + s1.len() > 16 {
            return Err(DecodeError::InvalidData("too many RPS entries"));
        }
        out.num_negative = s0.len() as u8;
        out.num_positive = s1.len() as u8;
        for (i, &(d, u)) in s0.iter().chain(s1.iter()).enumerate() {
            out.delta_poc[i] = d;
            out.used[i] = u;
        }
    } else {
        let nneg = r.ue_max(max_dec_pic_buffering_minus1.max(15), "num_negative_pics")?;
        let npos = r.ue_max(max_dec_pic_buffering_minus1.max(15), "num_positive_pics")?;
        if nneg + npos > 16 {
            return Err(DecodeError::InvalidData("too many RPS entries"));
        }
        out.num_negative = nneg as u8;
        out.num_positive = npos as u8;
        let mut poc = 0i32;
        for i in 0..nneg as usize {
            let d = r.ue_max(32767, "delta_poc_s0_minus1")? as i32 + 1;
            poc -= d;
            out.delta_poc[i] = poc;
            out.used[i] = r.flag()?;
        }
        poc = 0;
        for i in 0..npos as usize {
            let d = r.ue_max(32767, "delta_poc_s1_minus1")? as i32 + 1;
            poc += d;
            out.delta_poc[nneg as usize + i] = poc;
            out.used[nneg as usize + i] = r.flag()?;
        }
    }
    Ok(out)
}

/// Scaling factors (clause 7.4.5) expanded to full block sizes, stored as [y][x] row-major.
#[derive(Clone)]
pub struct ScalingList {
    /// sizeId 0: 6 matrices of 16.
    pub sl4: [[u8; 16]; 6],
    /// sizeId 1: 6 matrices of 64.
    pub sl8: [[u8; 64]; 6],
    /// sizeId 2: 6 matrices of 256.
    pub sl16: Box<[[u8; 256]; 6]>,
    /// sizeId 3: 6 matrices of 1024 (only 0 and 3 used for 4:2:0).
    pub sl32: Box<[[u8; 1024]; 6]>,
}

#[derive(Clone)]
struct ScalingListRaw {
    list: [[[u8; 64]; 6]; 4],
    dc: [[u8; 6]; 2],
}

impl ScalingListRaw {
    fn default_lists() -> Self {
        let mut list = [[[16u8; 64]; 6]; 4];
        for sl in list.iter_mut().skip(1) {
            for (m, l) in sl.iter_mut().enumerate() {
                *l = if m < 3 {
                    DEFAULT_SCALING_INTRA
                } else {
                    DEFAULT_SCALING_INTER
                };
            }
        }
        ScalingListRaw {
            list,
            dc: [[16; 6]; 2],
        }
    }

    fn parse(r: &mut BitReader) -> Result<Self> {
        let mut s = ScalingListRaw::default_lists();
        for size_id in 0..4usize {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut matrix_id = 0usize;
            while matrix_id < 6 {
                let pred_mode = r.flag()?;
                let coef_num = 64.min(1 << (4 + (size_id << 1)));
                if !pred_mode {
                    let max = if size_id == 3 {
                        matrix_id / 3
                    } else {
                        matrix_id
                    };
                    let delta = r.ue_max(max as u32, "scaling_list_pred_matrix_id_delta")? as usize;
                    if delta == 0 {
                        let d = if size_id == 0 {
                            [16u8; 64]
                        } else if matrix_id < 3 {
                            DEFAULT_SCALING_INTRA
                        } else {
                            DEFAULT_SCALING_INTER
                        };
                        s.list[size_id][matrix_id] = d;
                        if size_id > 1 {
                            s.dc[size_id - 2][matrix_id] = 16;
                        }
                    } else {
                        let ref_id = matrix_id - delta * step;
                        s.list[size_id][matrix_id] = s.list[size_id][ref_id];
                        if size_id > 1 {
                            s.dc[size_id - 2][matrix_id] = s.dc[size_id - 2][ref_id];
                        }
                    }
                } else {
                    let mut next = 8i32;
                    if size_id > 1 {
                        let dc = r.se_range(-7, 247, "scaling_list_dc_coef_minus8")?;
                        next = dc + 8;
                        s.dc[size_id - 2][matrix_id] = next as u8;
                    }
                    for e in s.list[size_id][matrix_id].iter_mut().take(coef_num) {
                        let delta = r.se_range(-128, 127, "scaling_list_delta_coef")?;
                        next = (next + delta + 256) % 256;
                        if next == 0 {
                            return Err(DecodeError::InvalidData("scaling list entry zero"));
                        }
                        *e = next as u8;
                    }
                }
                matrix_id += step;
            }
        }
        // 32x32 chroma matrices (only used for ChromaArrayType 3) derive from 16x16 (7-50).
        for m in [1usize, 2, 4, 5] {
            s.list[3][m] = s.list[2][m];
            s.dc[1][m] = s.dc[0][m];
        }
        Ok(s)
    }

    fn expand(&self) -> ScalingList {
        let mut out = ScalingList {
            sl4: [[16; 16]; 6],
            sl8: [[16; 64]; 6],
            sl16: Box::new([[16; 256]; 6]),
            sl32: Box::new([[16; 1024]; 6]),
        };
        for m in 0..6 {
            for i in 0..16 {
                let (x, y) = DIAG_SCAN_4X4[i];
                out.sl4[m][y as usize * 4 + x as usize] = self.list[0][m][i];
            }
            for i in 0..64 {
                let (x, y) = DIAG_SCAN_8X8[i];
                let (x, y) = (x as usize, y as usize);
                out.sl8[m][y * 8 + x] = self.list[1][m][i];
                let v2 = self.list[2][m][i];
                for j in 0..2 {
                    for k in 0..2 {
                        out.sl16[m][(y * 2 + j) * 16 + x * 2 + k] = v2;
                    }
                }
                let v3 = self.list[3][m][i];
                for j in 0..4 {
                    for k in 0..4 {
                        out.sl32[m][(y * 4 + j) * 32 + x * 4 + k] = v3;
                    }
                }
            }
            out.sl16[m][0] = self.dc[0][m];
            out.sl32[m][0] = self.dc[1][m];
        }
        out
    }
}

impl ScalingList {
    pub fn default_list() -> ScalingList {
        ScalingListRaw::default_lists().expand()
    }
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // informative fields kept for diagnostics
pub struct ProfileInfo {
    pub profile_space: u8,
    pub tier: bool,
    pub profile_idc: u8,
    pub compat: u32,
    pub level_idc: u8,
}

fn parse_ptl(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<ProfileInfo> {
    let mut p = ProfileInfo {
        profile_space: r.u(2)? as u8,
        tier: r.flag()?,
        profile_idc: r.u(5)? as u8,
        compat: r.u(32)?,
        level_idc: 0,
    };
    // progressive/interlaced/non_packed/frame_only (4) + 43 bits + 1 bit.
    r.skip(4 + 43 + 1)?;
    p.level_idc = r.u(8)? as u8;
    let mut prof_present = [false; 8];
    let mut lvl_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        prof_present[i] = r.flag()?;
        lvl_present[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if prof_present[i] {
            r.skip(88)?;
        }
        if lvl_present[i] {
            r.skip(8)?;
        }
    }
    Ok(p)
}

#[derive(Clone, Debug)]
pub struct Vps {
    pub id: u8,
}

pub fn parse_vps(rbsp: &[u8]) -> Result<Vps> {
    let mut r = BitReader::new(rbsp);
    let id = r.u(4)? as u8;
    Ok(Vps { id })
}

#[derive(Clone, Debug, Default)]
pub struct Vui {
    pub sar: (u32, u32),
    pub video_signal_type_present: bool,
    pub video_format: u8,
    pub full_range: bool,
    pub colour_description_present: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coeffs: u8,
    pub field_seq_flag: bool,
    pub default_display_window: [u32; 4],
}

/// Table E.1 sample aspect ratio indicators.
const SAR_TABLE: [(u32, u32); 17] = [
    (0, 1),
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

fn parse_sub_layer_hrd(r: &mut BitReader, cpb_cnt: u32, sub_pic: bool) -> Result<()> {
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        if sub_pic {
            r.ue()?;
            r.ue()?;
        }
        r.skip(1)?;
    }
    Ok(())
}

fn parse_hrd(r: &mut BitReader, common: bool, max_sub_layers_minus1: u32) -> Result<()> {
    let mut nal = false;
    let mut vcl = false;
    let mut sub_pic = false;
    if common {
        nal = r.flag()?;
        vcl = r.flag()?;
        if nal || vcl {
            sub_pic = r.flag()?;
            if sub_pic {
                r.skip(8 + 5 + 1 + 5)?;
            }
            r.skip(8)?;
            if sub_pic {
                r.skip(4)?;
            }
            r.skip(15)?;
        }
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.flag()?;
        let fixed_cvs = if !fixed_general { r.flag()? } else { true };
        let mut low_delay = false;
        if fixed_cvs {
            r.ue()?;
        } else {
            low_delay = r.flag()?;
        }
        let cpb_cnt = if !low_delay {
            r.ue_max(31, "cpb_cnt_minus1")? + 1
        } else {
            1
        };
        if nal {
            parse_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
        if vcl {
            parse_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
    }
    Ok(())
}

fn parse_vui(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<Vui> {
    let mut v = Vui {
        sar: (0, 1),
        colour_primaries: 2,
        transfer_characteristics: 2,
        matrix_coeffs: 2,
        video_format: 5,
        ..Default::default()
    };
    if r.flag()? {
        let idc = r.u(8)?;
        if idc == 255 {
            let w = r.u(16)?;
            let h = r.u(16)?;
            v.sar = (w, h);
        } else if (idc as usize) < SAR_TABLE.len() {
            v.sar = SAR_TABLE[idc as usize];
        } else {
            v.sar = (0, 1);
        }
    }
    if r.flag()? {
        r.skip(1)?; // overscan_appropriate_flag
    }
    if r.flag()? {
        v.video_signal_type_present = true;
        v.video_format = r.u(3)? as u8;
        v.full_range = r.flag()?;
        if r.flag()? {
            v.colour_description_present = true;
            v.colour_primaries = r.u(8)? as u8;
            v.transfer_characteristics = r.u(8)? as u8;
            v.matrix_coeffs = r.u(8)? as u8;
        }
    }
    if r.flag()? {
        r.ue()?;
        r.ue()?;
    }
    r.skip(1)?; // neutral_chroma_indication_flag
    v.field_seq_flag = r.flag()?;
    r.skip(1)?; // frame_field_info_present_flag
    if r.flag()? {
        for e in v.default_display_window.iter_mut() {
            *e = r.ue()?;
        }
    }
    if r.flag()? {
        r.skip(64)?;
        if r.flag()? {
            r.ue()?;
        }
        if r.flag()? {
            parse_hrd(r, true, max_sub_layers_minus1)?;
        }
    }
    if r.flag()? {
        r.skip(3)?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
    }
    Ok(v)
}

#[derive(Clone)]
#[allow(dead_code)] // informative fields kept for diagnostics
pub struct Sps {
    pub id: u8,
    pub vps_id: u8,
    pub max_sub_layers_minus1: u32,
    pub profile: ProfileInfo,
    pub chroma_format_idc: u32,
    pub width: u32,
    pub height: u32,
    /// Conformance window in luma samples (left, right, top, bottom).
    pub conf_win: [u32; 4],
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub log2_max_poc_lsb: u32,
    pub max_dec_pic_buffering: [u32; 8],
    pub max_num_reorder: [u32; 8],
    pub max_latency_increase_plus1: [u32; 8],
    pub log2_min_cb: u32,
    pub log2_ctb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub max_th_depth_inter: u32,
    pub max_th_depth_intra: u32,
    pub scaling_list_enabled: bool,
    pub scaling_list: Option<ScalingList>,
    pub amp: bool,
    pub sao: bool,
    pub pcm: bool,
    pub pcm_bit_depth_luma: u32,
    pub pcm_bit_depth_chroma: u32,
    pub log2_min_pcm: u32,
    pub log2_max_pcm: u32,
    pub pcm_loop_filter_disabled: bool,
    pub st_rps: Vec<ShortTermRps>,
    pub long_term_refs_present: bool,
    pub lt_ref_pic_poc_lsb: Vec<u32>,
    pub lt_used_by_curr: Vec<bool>,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    pub vui: Vui,
    // Derived.
    pub ctb_size: u32,
    pub pic_w_ctb: u32,
    pub pic_h_ctb: u32,
    pub min_cb_w: u32,
    pub min_cb_h: u32,
}

impl Sps {
    #[inline]
    pub fn pic_size_ctb(&self) -> u32 {
        self.pic_w_ctb * self.pic_h_ctb
    }
    pub fn high_bit_depth(&self) -> bool {
        self.bit_depth_luma > 8 || self.bit_depth_chroma > 8
    }
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(rbsp);
    let vps_id = r.u(4)? as u8;
    let max_sub_layers_minus1 = r.u(3)?;
    if max_sub_layers_minus1 > 6 {
        return Err(DecodeError::InvalidData("sps_max_sub_layers_minus1"));
    }
    let _temporal_id_nesting = r.flag()?;
    let profile = parse_ptl(&mut r, max_sub_layers_minus1)?;
    let id = r.ue_max(15, "sps_seq_parameter_set_id")? as u8;
    let chroma_format_idc = r.ue_max(3, "chroma_format_idc")?;
    if chroma_format_idc == 3 {
        let sep = r.flag()?;
        if sep {
            return Err(DecodeError::Unsupported(
                "separate_colour_plane_flag (4:4:4 RExt)",
            ));
        }
    }
    match chroma_format_idc {
        1 => {}
        0 => {
            return Err(DecodeError::Unsupported(
                "chroma_format_idc 0 (monochrome, RExt)",
            ));
        }
        2 => {
            return Err(DecodeError::Unsupported(
                "chroma_format_idc 2 (4:2:2, RExt)",
            ));
        }
        _ => {
            return Err(DecodeError::Unsupported(
                "chroma_format_idc 3 (4:4:4, RExt)",
            ));
        }
    }
    let width = r.ue()?;
    let height = r.ue()?;
    let mut conf_win = [0u32; 4];
    if r.flag()? {
        for c in conf_win.iter_mut() {
            *c = r.ue()?;
        }
    }
    let bit_depth_luma = r.ue_max(8, "bit_depth_luma_minus8")? + 8;
    let bit_depth_chroma = r.ue_max(8, "bit_depth_chroma_minus8")? + 8;
    if bit_depth_luma > 10 || bit_depth_chroma > 10 {
        return Err(DecodeError::Unsupported("bit depth above 10 (RExt)"));
    }
    if bit_depth_luma != bit_depth_chroma {
        return Err(DecodeError::Unsupported("unequal luma/chroma bit depth"));
    }
    let log2_max_poc_lsb = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4;
    let sub_layer_ordering_info = r.flag()?;
    let mut max_dec_pic_buffering = [0u32; 8];
    let mut max_num_reorder = [0u32; 8];
    let mut max_latency_increase_plus1 = [0u32; 8];
    let start = if sub_layer_ordering_info {
        0
    } else {
        max_sub_layers_minus1
    };
    for i in start..=max_sub_layers_minus1 {
        let i = i as usize;
        max_dec_pic_buffering[i] = r.ue_max(15, "sps_max_dec_pic_buffering_minus1")? + 1;
        max_num_reorder[i] = r.ue_max(15, "sps_max_num_reorder_pics")?;
        max_latency_increase_plus1[i] = r.ue()?;
    }
    if !sub_layer_ordering_info {
        for i in 0..max_sub_layers_minus1 as usize {
            let s = max_sub_layers_minus1 as usize;
            max_dec_pic_buffering[i] = max_dec_pic_buffering[s];
            max_num_reorder[i] = max_num_reorder[s];
            max_latency_increase_plus1[i] = max_latency_increase_plus1[s];
        }
    }
    let log2_min_cb = r.ue_max(3, "log2_min_luma_coding_block_size_minus3")? + 3;
    let log2_ctb = log2_min_cb + r.ue_max(3, "log2_diff_max_min_luma_coding_block_size")?;
    let log2_min_tb = r.ue_max(3, "log2_min_luma_transform_block_size_minus2")? + 2;
    let log2_max_tb = log2_min_tb + r.ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
    if !(4..=6).contains(&log2_ctb) {
        return Err(DecodeError::InvalidData("CTB size outside 16..64"));
    }
    if log2_min_tb >= log2_min_cb || log2_max_tb > 5 || log2_max_tb > log2_ctb {
        return Err(DecodeError::InvalidData("transform block size range"));
    }
    if width == 0
        || height == 0
        || width % (1 << log2_min_cb) != 0
        || height % (1 << log2_min_cb) != 0
    {
        return Err(DecodeError::InvalidData(
            "picture size not a multiple of MinCbSizeY",
        ));
    }
    if width > MAX_DIM || height > MAX_DIM || (width as u64 * height as u64) > MAX_LUMA_PS {
        return Err(DecodeError::ResourceLimit(
            "picture size above level 6.2 limits",
        ));
    }
    let max_th_depth_inter = r.ue_max(
        log2_ctb - log2_min_tb,
        "max_transform_hierarchy_depth_inter",
    )?;
    let max_th_depth_intra = r.ue_max(
        log2_ctb - log2_min_tb,
        "max_transform_hierarchy_depth_intra",
    )?;
    let scaling_list_enabled = r.flag()?;
    let mut scaling_list = None;
    if scaling_list_enabled {
        if r.flag()? {
            scaling_list = Some(ScalingListRaw::parse(&mut r)?.expand());
        } else {
            scaling_list = Some(ScalingList::default_list());
        }
    }
    let amp = r.flag()?;
    let sao = r.flag()?;
    let pcm = r.flag()?;
    let (
        mut pcm_bit_depth_luma,
        mut pcm_bit_depth_chroma,
        mut log2_min_pcm,
        mut log2_max_pcm,
        mut pcm_lf,
    ) = (8, 8, 3, 3, false);
    if pcm {
        pcm_bit_depth_luma = r.u(4)? + 1;
        pcm_bit_depth_chroma = r.u(4)? + 1;
        if pcm_bit_depth_luma > bit_depth_luma || pcm_bit_depth_chroma > bit_depth_chroma {
            return Err(DecodeError::InvalidData("PCM bit depth exceeds bit depth"));
        }
        log2_min_pcm = r.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")? + 3;
        log2_max_pcm =
            log2_min_pcm + r.ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")?;
        if log2_max_pcm > 5.min(log2_ctb) {
            return Err(DecodeError::InvalidData("PCM size"));
        }
        pcm_lf = r.flag()?;
    }
    let num_st = r.ue_max(64, "num_short_term_ref_pic_sets")? as usize;
    let mut st_rps: Vec<ShortTermRps> = Vec::with_capacity(num_st);
    let mdpb = max_dec_pic_buffering[max_sub_layers_minus1 as usize] - 1;
    for i in 0..num_st {
        let s = parse_st_rps(&mut r, i, num_st, &st_rps, mdpb)?;
        st_rps.push(s);
    }
    let long_term_refs_present = r.flag()?;
    let mut lt_ref_pic_poc_lsb = Vec::new();
    let mut lt_used_by_curr = Vec::new();
    if long_term_refs_present {
        let n = r.ue_max(32, "num_long_term_ref_pics_sps")?;
        for _ in 0..n {
            lt_ref_pic_poc_lsb.push(r.u(log2_max_poc_lsb)?);
            lt_used_by_curr.push(r.flag()?);
        }
    }
    let temporal_mvp = r.flag()?;
    let strong_intra_smoothing = r.flag()?;
    let vui = if r.flag()? {
        parse_vui(&mut r, max_sub_layers_minus1)?
    } else {
        Vui {
            sar: (0, 1),
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coeffs: 2,
            video_format: 5,
            ..Default::default()
        }
    };
    if r.flag()? {
        let range = r.flag()?;
        let _multilayer = r.flag()?;
        let _3d = r.flag()?;
        let scc = r.flag()?;
        let _ext4 = r.u(4)?;
        if range {
            let names = [
                "transform_skip_rotation_enabled_flag (RExt)",
                "transform_skip_context_enabled_flag (RExt)",
                "implicit_rdpcm_enabled_flag (RExt)",
                "explicit_rdpcm_enabled_flag (RExt)",
                "extended_precision_processing_flag (RExt)",
                "intra_smoothing_disabled_flag (RExt)",
                "high_precision_offsets_enabled_flag (RExt)",
                "persistent_rice_adaptation_enabled_flag (RExt)",
                "cabac_bypass_alignment_enabled_flag (RExt)",
            ];
            for name in names {
                if r.flag()? {
                    return Err(DecodeError::Unsupported(name));
                }
            }
        }
        if scc {
            return Err(DecodeError::Unsupported("sps_scc_extension (SCC)"));
        }
        // Multilayer / 3D extensions only constrain non-base layers; ignored for the base layer.
    }
    let ctb_size = 1 << log2_ctb;
    let pic_w_ctb = width.div_ceil(ctb_size);
    let pic_h_ctb = height.div_ceil(ctb_size);
    let sub = 2; // SubWidthC == SubHeightC == 2 for 4:2:0
    if (conf_win[0] + conf_win[1]) * sub >= width || (conf_win[2] + conf_win[3]) * sub >= height {
        return Err(DecodeError::InvalidData("conformance window"));
    }
    let conf_win = [
        conf_win[0] * sub,
        conf_win[1] * sub,
        conf_win[2] * sub,
        conf_win[3] * sub,
    ];
    Ok(Sps {
        id,
        vps_id,
        max_sub_layers_minus1,
        profile,
        chroma_format_idc,
        width,
        height,
        conf_win,
        bit_depth_luma,
        bit_depth_chroma,
        log2_max_poc_lsb,
        max_dec_pic_buffering,
        max_num_reorder,
        max_latency_increase_plus1,
        log2_min_cb,
        log2_ctb,
        log2_min_tb,
        log2_max_tb,
        max_th_depth_inter,
        max_th_depth_intra,
        scaling_list_enabled,
        scaling_list,
        amp,
        sao,
        pcm,
        pcm_bit_depth_luma,
        pcm_bit_depth_chroma,
        log2_min_pcm,
        log2_max_pcm,
        pcm_loop_filter_disabled: pcm_lf,
        st_rps,
        long_term_refs_present,
        lt_ref_pic_poc_lsb,
        lt_used_by_curr,
        temporal_mvp,
        strong_intra_smoothing,
        vui,
        ctb_size,
        pic_w_ctb,
        pic_h_ctb,
        min_cb_w: width >> log2_min_cb,
        min_cb_h: height >> log2_min_cb,
    })
}

#[derive(Clone)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default: u32,
    pub num_ref_idx_l1_default: u32,
    pub init_qp: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync: bool,
    pub num_tile_columns: u32,
    pub num_tile_rows: u32,
    pub uniform_spacing: bool,
    pub column_width_minus1: Vec<u32>,
    pub row_height_minus1: Vec<u32>,
    pub loop_filter_across_tiles: bool,
    pub loop_filter_across_slices: bool,
    pub deblocking_override_enabled: bool,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub scaling_list: Option<ScalingList>,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u32,
    pub slice_header_extension_present: bool,
    /// A range-extension tool signalled in pps_range_extension(), if any.
    pub range_ext_tool: Option<&'static str>,
    /// pps_scc_extension_flag was set (refused for SCC profiles at activation).
    pub scc_ext: bool,
}

pub fn parse_pps(rbsp: &[u8]) -> Result<Pps> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue_max(63, "pps_pic_parameter_set_id")? as u8;
    let sps_id = r.ue_max(15, "pps_seq_parameter_set_id")? as u8;
    let dependent_slice_segments_enabled = r.flag()?;
    let output_flag_present = r.flag()?;
    let num_extra_slice_header_bits = r.u(3)?;
    let sign_data_hiding = r.flag()?;
    let cabac_init_present = r.flag()?;
    let num_ref_idx_l0_default = r.ue_max(14, "num_ref_idx_l0_default_active_minus1")? + 1;
    let num_ref_idx_l1_default = r.ue_max(14, "num_ref_idx_l1_default_active_minus1")? + 1;
    let init_qp = 26 + r.se_range(-(26 + 12), 25, "init_qp_minus26")?;
    let constrained_intra_pred = r.flag()?;
    let transform_skip = r.flag()?;
    let cu_qp_delta_enabled = r.flag()?;
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled {
        r.ue_max(3, "diff_cu_qp_delta_depth")?
    } else {
        0
    };
    let cb_qp_offset = r.se_range(-12, 12, "pps_cb_qp_offset")?;
    let cr_qp_offset = r.se_range(-12, 12, "pps_cr_qp_offset")?;
    let slice_chroma_qp_offsets_present = r.flag()?;
    let weighted_pred = r.flag()?;
    let weighted_bipred = r.flag()?;
    let transquant_bypass = r.flag()?;
    let tiles_enabled = r.flag()?;
    let entropy_coding_sync = r.flag()?;
    let mut num_tile_columns = 1;
    let mut num_tile_rows = 1;
    let mut uniform_spacing = true;
    let mut column_width_minus1 = Vec::new();
    let mut row_height_minus1 = Vec::new();
    let mut loop_filter_across_tiles = true;
    if tiles_enabled {
        num_tile_columns = r.ue_max(19, "num_tile_columns_minus1")? + 1;
        num_tile_rows = r.ue_max(21, "num_tile_rows_minus1")? + 1;
        uniform_spacing = r.flag()?;
        if !uniform_spacing {
            for _ in 0..num_tile_columns - 1 {
                column_width_minus1.push(r.ue_max(1024, "column_width_minus1")?);
            }
            for _ in 0..num_tile_rows - 1 {
                row_height_minus1.push(r.ue_max(1024, "row_height_minus1")?);
            }
        }
        loop_filter_across_tiles = r.flag()?;
    }
    let loop_filter_across_slices = r.flag()?;
    let mut deblocking_override_enabled = false;
    let mut deblocking_disabled = false;
    let mut beta_offset_div2 = 0;
    let mut tc_offset_div2 = 0;
    if r.flag()? {
        deblocking_override_enabled = r.flag()?;
        deblocking_disabled = r.flag()?;
        if !deblocking_disabled {
            beta_offset_div2 = r.se_range(-6, 6, "pps_beta_offset_div2")?;
            tc_offset_div2 = r.se_range(-6, 6, "pps_tc_offset_div2")?;
        }
    }
    let scaling_list = if r.flag()? {
        Some(ScalingListRaw::parse(&mut r)?.expand())
    } else {
        None
    };
    let lists_modification_present = r.flag()?;
    let log2_parallel_merge_level = r.ue_max(4, "log2_parallel_merge_level_minus2")? + 2;
    let slice_header_extension_present = r.flag()?;
    let mut range_ext_tool: Option<&'static str> = None;
    let mut scc_ext = false;
    if r.flag()? {
        let range = r.flag()?;
        let _ml = r.flag()?;
        let _3d = r.flag()?;
        let scc = r.flag()?;
        let _e4 = r.u(4)?;
        if range {
            // Parsed tolerantly; only refused at activation when the SPS signals a range
            // extensions (or higher) profile. For Main/Main 10 profiles the payload is ignored.
            let mut parse = || -> Result<Option<&'static str>> {
                if transform_skip && r.ue()? != 0 {
                    return Ok(Some("log2_max_transform_skip_block_size_minus2 > 0 (RExt)"));
                }
                if r.flag()? {
                    return Ok(Some("cross_component_prediction_enabled_flag (RExt)"));
                }
                if r.flag()? {
                    return Ok(Some("chroma_qp_offset_list_enabled_flag (RExt)"));
                }
                if r.ue()? != 0 {
                    return Ok(Some("log2_sao_offset_scale_luma (RExt)"));
                }
                if r.ue()? != 0 {
                    return Ok(Some("log2_sao_offset_scale_chroma (RExt)"));
                }
                Ok(None)
            };
            range_ext_tool = parse().unwrap_or(Some("pps_range_extension (unparsable)"));
        }
        if scc && range_ext_tool.is_none() {
            scc_ext = true;
        }
    }
    Ok(Pps {
        scc_ext,
        range_ext_tool,
        id,
        sps_id,
        dependent_slice_segments_enabled,
        output_flag_present,
        num_extra_slice_header_bits,
        sign_data_hiding,
        cabac_init_present,
        num_ref_idx_l0_default,
        num_ref_idx_l1_default,
        init_qp,
        constrained_intra_pred,
        transform_skip,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        weighted_pred,
        weighted_bipred,
        transquant_bypass,
        tiles_enabled,
        entropy_coding_sync,
        num_tile_columns,
        num_tile_rows,
        uniform_spacing,
        column_width_minus1,
        row_height_minus1,
        loop_filter_across_tiles,
        loop_filter_across_slices,
        deblocking_override_enabled,
        deblocking_disabled,
        beta_offset_div2,
        tc_offset_div2,
        scaling_list,
        lists_modification_present,
        log2_parallel_merge_level,
        slice_header_extension_present,
    })
}

/// Picture-level layout derived from an (SPS, PPS) pair: tile scan conversion (6.5.1) and
/// z-scan order (6.5.2).
#[allow(dead_code)] // informative fields kept for diagnostics
pub struct Layout {
    pub ctb_addr_rs_to_ts: Vec<u32>,
    pub ctb_addr_ts_to_rs: Vec<u32>,
    pub tile_id: Vec<u16>, // indexed by ts
    /// Column boundaries in CTBs (len = cols + 1).
    pub col_bd: Vec<u32>,
    pub row_bd: Vec<u32>,
    /// MinTbAddrZs[y * w + x] in min TB units.
    pub min_tb_addr_zs: Vec<u32>,
    pub min_tb_w: u32,
    pub min_tb_h: u32,
}

impl Layout {
    pub fn new(sps: &Sps, pps: &Pps) -> Result<Layout> {
        let w = sps.pic_w_ctb;
        let h = sps.pic_h_ctb;
        let cols = pps.num_tile_columns;
        let rows = pps.num_tile_rows;
        if cols > w || rows > h {
            return Err(DecodeError::InvalidData("more tiles than CTBs"));
        }
        let mut col_w = vec![0u32; cols as usize];
        let mut row_h = vec![0u32; rows as usize];
        if pps.uniform_spacing {
            for i in 0..cols {
                col_w[i as usize] = ((i + 1) * w) / cols - (i * w) / cols;
            }
            for j in 0..rows {
                row_h[j as usize] = ((j + 1) * h) / rows - (j * h) / rows;
            }
        } else {
            let mut rem = w as i64;
            for i in 0..cols as usize - 1 {
                col_w[i] = pps.column_width_minus1[i] + 1;
                rem -= col_w[i] as i64;
            }
            if rem <= 0 {
                return Err(DecodeError::InvalidData("tile column widths"));
            }
            col_w[cols as usize - 1] = rem as u32;
            let mut rem = h as i64;
            for j in 0..rows as usize - 1 {
                row_h[j] = pps.row_height_minus1[j] + 1;
                rem -= row_h[j] as i64;
            }
            if rem <= 0 {
                return Err(DecodeError::InvalidData("tile row heights"));
            }
            row_h[rows as usize - 1] = rem as u32;
        }
        let mut col_bd = vec![0u32; cols as usize + 1];
        for i in 0..cols as usize {
            col_bd[i + 1] = col_bd[i] + col_w[i];
        }
        let mut row_bd = vec![0u32; rows as usize + 1];
        for j in 0..rows as usize {
            row_bd[j + 1] = row_bd[j] + row_h[j];
        }
        let n = (w * h) as usize;
        let mut rs_to_ts = vec![0u32; n];
        for rs in 0..n as u32 {
            let tbx = rs % w;
            let tby = rs / w;
            let tile_x = col_bd[..cols as usize]
                .iter()
                .rposition(|&b| tbx >= b)
                .unwrap_or(0);
            let tile_y = row_bd[..rows as usize]
                .iter()
                .rposition(|&b| tby >= b)
                .unwrap_or(0);
            let mut v = 0;
            for cw in &col_w[..tile_x] {
                v += row_h[tile_y] * cw;
            }
            for rh in &row_h[..tile_y] {
                v += w * rh;
            }
            v += (tby - row_bd[tile_y]) * col_w[tile_x] + tbx - col_bd[tile_x];
            rs_to_ts[rs as usize] = v;
        }
        let mut ts_to_rs = vec![0u32; n];
        for rs in 0..n {
            ts_to_rs[rs_to_ts[rs] as usize] = rs as u32;
        }
        let mut tile_id = vec![0u16; n];
        let mut tidx = 0u16;
        for j in 0..rows as usize {
            for i in 0..cols as usize {
                for y in row_bd[j]..row_bd[j + 1] {
                    for x in col_bd[i]..col_bd[i + 1] {
                        tile_id[rs_to_ts[(y * w + x) as usize] as usize] = tidx;
                    }
                }
                tidx += 1;
            }
        }
        let shift = sps.log2_ctb - sps.log2_min_tb;
        let mtw = w << shift;
        let mth = h << shift;
        let mut zs = vec![0u32; (mtw * mth) as usize];
        for y in 0..mth {
            for x in 0..mtw {
                let tbx = (x << sps.log2_min_tb) >> sps.log2_ctb;
                let tby = (y << sps.log2_min_tb) >> sps.log2_ctb;
                let rs = w * tby + tbx;
                let mut v = rs_to_ts[rs as usize] << (shift * 2);
                let mut p = 0;
                for i in 0..shift {
                    let m = 1u32 << i;
                    p += (if m & x != 0 { m * m } else { 0 })
                        + (if m & y != 0 { 2 * m * m } else { 0 });
                }
                v += p;
                zs[(y * mtw + x) as usize] = v;
            }
        }
        Ok(Layout {
            ctb_addr_rs_to_ts: rs_to_ts,
            ctb_addr_ts_to_rs: ts_to_rs,
            tile_id,
            col_bd,
            row_bd,
            min_tb_addr_zs: zs,
            min_tb_w: mtw,
            min_tb_h: mth,
        })
    }
}
