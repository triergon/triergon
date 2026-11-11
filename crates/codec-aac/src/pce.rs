//! program_config_element() (ISO/IEC 14496-3 4.4.1.1, Table 4.2) including the height
//! extension carried in its comment field (ISO/IEC 14496-3:2009/Amd.4, 4.5.1.3).

use crate::error::{DecodeError, Result};
use triergon_bitstream::CachedBitReader;
use triergon_bitstream::crc::crc8;

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
    apply_heights(&mut p, *br, comment_bytes);
    br.skip(comment_bytes * 8);
    if br.overrun() {
        return Err(DecodeError::InvalidData("PCE overrun"));
    }
    Ok(p)
}

fn apply_heights(program: &mut Pce, mut comment: CachedBitReader<'_>, bytes: usize) {
    let count = program.front.len() + program.side.len() + program.back.len();
    let covered = 1 + (count * 2).div_ceil(8);
    if bytes <= covered || comment.peek(8) != 0xac {
        return;
    }
    // The sync byte, height fields and alignment bits are protected by CRC-8.
    let mut checksum = comment;
    let mut crc = 0xff;
    for _ in 0..covered {
        crc = crc8(crc, &[checksum.read(8) as u8]);
    }
    if crc != checksum.read(8) as u8 {
        return;
    }
    comment.skip(8);
    let elements = program
        .front
        .iter_mut()
        .chain(&mut program.side)
        .chain(&mut program.back);
    let mut check = comment;
    if (0..count).all(|_| check.read(2) != 3) {
        for element in elements {
            element.height = comment.read(2) as u8;
        }
    }
}
