//! program_config_element() (ISO/IEC 14496-3 4.4.1.1, Table 4.2) including the height
//! extension carried in its comment field (ISO/IEC 14496-3:2009/Amd.4, 4.5.1.2.2).

use crate::error::{DecodeError, Result};
use triergon_bitstream::CachedBitReader;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct PceElement {
    pub is_cpe: bool,
    pub tag: u8,
    /// 0 normal, 1 top, 2 bottom
    pub height: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct Pce {
    pub tag: u8,
    pub object_type: u8,
    pub sf_index: u8,
    pub front: Vec<PceElement>,
    pub side: Vec<PceElement>,
    pub back: Vec<PceElement>,
    pub lfe: Vec<u8>,
    pub assoc: Vec<u8>,
    /// (is_ind_sw, tag)
    pub cc: Vec<(bool, u8)>,
}

fn read_elements(br: &mut CachedBitReader<'_>, n: usize, with_cpe: bool) -> Vec<PceElement> {
    (0..n)
        .map(|_| {
            let is_cpe = if with_cpe { br.bit() } else { false };
            PceElement {
                is_cpe,
                tag: br.read(4) as u8,
                height: 0,
            }
        })
        .collect()
}

/// Parse a PCE. `align_base` is the bit position that byte_alignment() refers to.
pub(crate) fn parse_pce(br: &mut CachedBitReader<'_>, align_base: usize) -> Result<Pce> {
    let mut p = Pce {
        tag: br.read(4) as u8,
        object_type: br.read(2) as u8,
        sf_index: br.read(4) as u8,
        ..Default::default()
    };
    let nf = br.read(4) as usize;
    let ns = br.read(4) as usize;
    let nb = br.read(4) as usize;
    let nl = br.read(2) as usize;
    let na = br.read(3) as usize;
    let nc = br.read(4) as usize;
    if br.bit() {
        br.skip(4); // mono_mixdown_element_number
    }
    if br.bit() {
        br.skip(4); // stereo_mixdown_element_number
    }
    if br.bit() {
        br.skip(3); // matrix_mixdown_idx, pseudo_surround_enable
    }
    p.front = read_elements(br, nf, true);
    p.side = read_elements(br, ns, true);
    p.back = read_elements(br, nb, true);
    p.lfe = (0..nl).map(|_| br.read(4) as u8).collect();
    p.assoc = (0..na).map(|_| br.read(4) as u8).collect();
    p.cc = (0..nc)
        .map(|_| {
            let ind = br.bit();
            (ind, br.read(4) as u8)
        })
        .collect();
    // byte_alignment()
    let rel = br.position() - align_base;
    br.skip((8 - (rel & 7)) & 7);
    let comment_bytes = br.read(8) as usize;
    if br.left() < (comment_bytes * 8) as isize {
        return Err(DecodeError::InvalidData("PCE comment field"));
    }
    let comment_start = br.position();
    // height extension (sync 0xAC) followed by one 2-bit height per front/side/back element
    if comment_bytes > 0 {
        let mut hb = *br;
        if hb.read(8) == 0xAC {
            let mut ok = true;
            for e in p
                .front
                .iter_mut()
                .chain(p.side.iter_mut())
                .chain(p.back.iter_mut())
            {
                e.height = hb.read(2) as u8;
                if e.height == 3 {
                    ok = false;
                }
            }
            if hb.position() > comment_start + comment_bytes * 8 {
                ok = false;
            }
            if !ok {
                for e in p
                    .front
                    .iter_mut()
                    .chain(p.side.iter_mut())
                    .chain(p.back.iter_mut())
                {
                    e.height = 0;
                }
            }
        }
    }
    br.skip(comment_bytes * 8);
    if br.overrun() {
        return Err(DecodeError::InvalidData("PCE overrun"));
    }
    Ok(p)
}
