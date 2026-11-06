//! Core frame header (ETSI TS 102 114 clause 5.4.2, Table 5-1) and the primary audio
//! coding header shared by the core and the XCh channel set (Tables 5-21 and 6-18).

use crate::DecodeError;
use crate::bits::BitReader;

/// Core sampling frequencies, Table 5-5 (0 = invalid).
const SFREQ_HZ: [u32; 16] = [
    0, 8000, 16000, 32000, 0, 0, 11025, 22050, 44100, 0, 0, 12000, 24000, 48000, 0, 0,
];

/// Decoded core frame header fields (Table 5-1).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameHeader {
    /// FTYPE (1 = normal frame). Termination frames decode like normal ones.
    #[allow(dead_code)]
    pub normal: bool,
    pub short: u8,
    pub cpf: bool,
    /// NBLKS + 1: number of 32-sample blocks (= subband samples per band).
    pub blocks: usize,
    /// FSIZE + 1 bytes.
    pub fsize: usize,
    pub amode: u8,
    pub sample_rate: u32,
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
}

impl FrameHeader {
    pub(crate) fn parse(br: &mut BitReader<'_>) -> Result<Self, DecodeError> {
        if br.read(32) != 0x7FFE_8001 {
            return Err(DecodeError::InvalidData("core sync word"));
        }
        let mut h = FrameHeader {
            normal: br.bit(),
            short: br.read(5) as u8,
            cpf: br.bit(),
            blocks: br.read(7) as usize + 1,
            fsize: br.read(14) as usize + 1,
            amode: br.read(6) as u8,
            ..Default::default()
        };
        let sfreq = br.read(4) as usize;
        h.sample_rate = SFREQ_HZ[sfreq];
        h.rate = br.read(5) as u8;
        let _fixed = br.bit();
        h.dynf = br.bit();
        h.timef = br.bit();
        h.auxf = br.bit();
        let _hdcd = br.bit();
        h.ext_audio_id = br.read(3) as u8;
        h.ext_audio = br.bit();
        h.aspf = br.bit();
        h.lff = br.read(2) as u8;
        h.hflag = br.bit();
        if h.cpf {
            let _hcrc = br.read(16);
        }
        h.filts = br.bit();
        h.vernum = br.read(4) as u8;
        let _chist = br.read(2);
        h.pcmr = br.read(3) as u8;
        h.sumf = br.bit();
        h.sums = br.bit();
        let _dialnorm = br.read(4);

        if h.sample_rate == 0 {
            return Err(DecodeError::InvalidData("core sampling frequency"));
        }
        if h.blocks < 6 {
            return Err(DecodeError::InvalidData("NBLKS below 5"));
        }
        if h.fsize < 96 {
            return Err(DecodeError::InvalidData("FSIZE below 95"));
        }
        if h.lff == 3 {
            return Err(DecodeError::InvalidData("LFF = 3"));
        }
        Ok(h)
    }
}

/// Maximum primary channels carried by one channel set.
pub(crate) const MAX_SET_CH: usize = 8;

/// Primary audio coding header of a channel set (Table 5-21 / 6-18).
#[derive(Clone, Copy)]
pub(crate) struct SetHeader {
    pub nch: usize,
    pub nsubs: [u8; MAX_SET_CH],
    pub nvqsub: [u8; MAX_SET_CH],
    pub joinx: [u8; MAX_SET_CH],
    pub thuff: [u8; MAX_SET_CH],
    pub shuff: [u8; MAX_SET_CH],
    pub bhuff: [u8; MAX_SET_CH],
    /// SEL by ABITS-1 (ABITS 1..=10).
    pub sel: [[u8; 10]; MAX_SET_CH],
    /// Scale factor adjustment by ABITS-1 (Table 5-27).
    pub adj: [[f32; 10]; MAX_SET_CH],
}

/// Table 5-27.
const ADJ_TABLE: [f32; 4] = [1.0, 1.125, 1.25, 1.4375];

impl SetHeader {
    /// Parse from PCHS onwards (`SUBFS` is read by the caller for the core).
    pub(crate) fn parse(br: &mut BitReader<'_>, cpf: bool) -> Result<Self, DecodeError> {
        let nch = br.read(3) as usize + 1;
        let mut h = SetHeader {
            nch,
            nsubs: [0; MAX_SET_CH],
            nvqsub: [0; MAX_SET_CH],
            joinx: [0; MAX_SET_CH],
            thuff: [0; MAX_SET_CH],
            shuff: [0; MAX_SET_CH],
            bhuff: [0; MAX_SET_CH],
            sel: [[0; 10]; MAX_SET_CH],
            adj: [[1.0; 10]; MAX_SET_CH],
        };
        for ch in 0..nch {
            h.nsubs[ch] = br.read(5) as u8 + 2;
            if h.nsubs[ch] > 32 {
                return Err(DecodeError::InvalidData("SUBS above 32 subbands"));
            }
        }
        for ch in 0..nch {
            h.nvqsub[ch] = br.read(5) as u8 + 1;
        }
        for ch in 0..nch {
            h.joinx[ch] = br.read(3) as u8;
        }
        for ch in 0..nch {
            h.thuff[ch] = br.read(2) as u8;
        }
        for ch in 0..nch {
            h.shuff[ch] = br.read(3) as u8;
            if h.shuff[ch] == 7 {
                return Err(DecodeError::InvalidData("SHUFF = 7"));
            }
        }
        for ch in 0..nch {
            h.bhuff[ch] = br.read(3) as u8;
            if h.bhuff[ch] == 7 {
                return Err(DecodeError::InvalidData("BHUFF = 7"));
            }
        }
        for n in 0..10 {
            let w = match n {
                0 => 1,
                1..=4 => 2,
                _ => 3,
            };
            for ch in 0..nch {
                h.sel[ch][n] = br.read(w) as u8;
            }
        }
        for n in 0..10 {
            let limit = match n {
                0 => 1,
                1..=4 => 3,
                _ => 7,
            };
            for ch in 0..nch {
                if h.sel[ch][n] < limit {
                    h.adj[ch][n] = ADJ_TABLE[br.read(2) as usize];
                }
            }
        }
        if cpf {
            let _ahcrc = br.read(16);
        }
        Ok(h)
    }
}
