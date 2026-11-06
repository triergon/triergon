//! Bit stream information (5.3.2, Annex E 2.3.1) and the E-AC-3 audio frame element
//! (Annex E 2.3.2).

use super::{CPL, DownmixCodes, E, LFE, NCH, NFCHANS, REUSE, Substream, Trace, bad};
use crate::bits::BitReader;
use crate::error::{DecodeError, Feature};
use crate::tables::{DEF_SPX_BNDSTRC, FRM_EXPSTR};

impl Substream {
    pub(super) fn parse_bsi_ac3(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        let _crc1 = br.get(16);
        let fscod = br.get(2) as u8;
        let _frmsizecod = br.get(6);
        let bsid = br.get(5) as u8;
        let _bsmod = br.get(3);
        let acmod = br.get(3) as u8;
        let mut dm = DownmixCodes::default();
        if (acmod & 1) != 0 && acmod != 1 {
            dm.cmixlev = Some(br.get(2) as u8);
        }
        if acmod & 4 != 0 {
            dm.surmixlev = Some(br.get(2) as u8);
        }
        if acmod == 2 {
            br.skip(2); // dsurmod
        }
        let lfeon = br.bit();
        let dialnorm = br.get(5) as u8;
        if br.bit() {
            br.skip(8); // compr
        }
        if br.bit() {
            br.skip(8); // langcod
        }
        if br.bit() {
            br.skip(7); // mixlevel, roomtyp
        }
        if acmod == 0 {
            br.skip(5); // dialnorm2
            if br.bit() {
                br.skip(8);
            }
            if br.bit() {
                br.skip(8);
            }
            if br.bit() {
                br.skip(7);
            }
        }
        br.skip(2); // copyrightb, origbs
        // timecod1e/timecod2e, or for bsid 6 (Annex D) xbsi1e/xbsi2e: both carry 14 bits.
        if br.bit() {
            if bsid == 6 {
                dm.dmixmod = Some(br.get(2) as u8);
                dm.ltrtcmixlev = Some(br.get(3) as u8);
                dm.ltrtsurmixlev = Some(br.get(3) as u8);
                dm.lorocmixlev = Some(br.get(3) as u8);
                dm.lorosurmixlev = Some(br.get(3) as u8);
            } else {
                br.skip(14);
            }
        }
        if br.bit() {
            br.skip(14);
        }
        if br.bit() {
            let addbsil = br.get(6) as usize;
            br.skip((addbsil + 1) * 8);
        }
        self.eac3 = false;
        self.strmtyp = 0;
        let shift = crate::header::ac3_rate_shift(bsid);
        self.set_config(acmod, lfeon, fscod, (fscod as usize, shift), 6, bsid);
        self.dialnorm = dialnorm;
        self.chanmap = None;
        self.downmix = dm;
        Ok(())
    }

    pub(super) fn parse_bsi_eac3(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        let strmtyp = br.get(2) as u8;
        let _substreamid = br.get(3);
        let _frmsiz = br.get(11);
        let fscod = br.get(2) as u8;
        let (numblkscod, hth_idx) = if fscod == 3 {
            let fscod2 = br.get(2) as usize;
            if fscod2 == 3 {
                return Err(DecodeError::Unsupported(Feature::ReservedSampleRate));
            }
            (3usize, fscod2)
        } else {
            (br.get(2) as usize, fscod as usize)
        };
        let num_blocks = [1, 2, 3, 6][numblkscod];
        let acmod = br.get(3) as u8;
        let lfeon = br.bit();
        let bsid = br.get(5) as u8;
        let dialnorm = br.get(5) as u8;
        if br.bit() {
            br.skip(8); // compr
        }
        if acmod == 0 {
            br.skip(5); // dialnorm2
            if br.bit() {
                br.skip(8); // compr2
            }
        }
        let mut chanmap = None;
        if strmtyp == 1 && br.bit() {
            chanmap = Some(br.get(16) as u16);
        }
        let mut dm = DownmixCodes::default();
        if br.bit() {
            // mixmdate
            if acmod > 2 {
                dm.dmixmod = Some(br.get(2) as u8);
            }
            if (acmod & 1) != 0 && acmod > 2 {
                dm.ltrtcmixlev = Some(br.get(3) as u8);
                dm.lorocmixlev = Some(br.get(3) as u8);
            }
            if acmod & 4 != 0 {
                dm.ltrtsurmixlev = Some(br.get(3) as u8);
                dm.lorosurmixlev = Some(br.get(3) as u8);
            }
            if lfeon && br.bit() {
                dm.lfemixlevcod = Some(br.get(5) as u8);
            }
            if strmtyp == 0 {
                if br.bit() {
                    br.skip(6); // pgmscl
                }
                if acmod == 0 && br.bit() {
                    br.skip(6); // pgmscl2
                }
                if br.bit() {
                    br.skip(6); // extpgmscl
                }
                match br.get(2) {
                    1 => br.skip(5),
                    2 => br.skip(12),
                    3 => {
                        // mixdeflen gives the length (2..=33 bytes) of the mixdata field,
                        // which begins with mixdeflen itself (Annex E 2.3.1.22, 3.10.4).
                        let start = br.pos();
                        let mixdeflen = br.get(5) as usize;
                        let end = start + 8 * (mixdeflen + 2);
                        br.skip(end - br.pos());
                    }
                    _ => {}
                }
                if acmod < 2 {
                    if br.bit() {
                        br.skip(14); // panmean, paninfo
                    }
                    if acmod == 0 && br.bit() {
                        br.skip(14); // panmean2, paninfo2
                    }
                }
                if br.bit() {
                    // frmmixcfginfoe
                    if numblkscod == 0 {
                        br.skip(5);
                    } else {
                        for _ in 0..num_blocks {
                            if br.bit() {
                                br.skip(5);
                            }
                        }
                    }
                }
            }
        }
        if br.bit() {
            // infomdate
            br.skip(5); // bsmod, copyrightb, origbs
            if acmod == 2 {
                br.skip(4); // dsurmod, dheadphonmod
            }
            if acmod >= 6 {
                br.skip(2); // dsurexmod
            }
            if br.bit() {
                br.skip(8); // mixlevel, roomtyp, adconvtyp
            }
            if acmod == 0 && br.bit() {
                br.skip(8);
            }
            if fscod < 3 {
                br.skip(1); // sourcefscod
            }
        }
        if strmtyp == 0 && numblkscod != 3 {
            br.skip(1); // convsync
        }
        if strmtyp == 2 {
            let blkid = if numblkscod == 3 { true } else { br.bit() };
            if blkid {
                br.skip(6); // frmsizecod
            }
        }
        if br.bit() {
            let addbsil = br.get(6) as usize;
            br.skip((addbsil + 1) * 8);
        }
        self.eac3 = true;
        self.strmtyp = strmtyp;
        self.set_config(acmod, lfeon, fscod, (hth_idx, 0), num_blocks, bsid);
        self.dialnorm = dialnorm;
        self.chanmap = chanmap;
        self.downmix = dm;
        Ok(())
    }

    fn set_config(
        &mut self,
        acmod: u8,
        lfeon: bool,
        fscod: u8,
        (hth_idx, rate_shift): (usize, u32),
        num_blocks: usize,
        bsid: u8,
    ) {
        let key = (acmod as u32) | (lfeon as u32) << 3 | (fscod as u32) << 4 | rate_shift << 6;
        if key != self.config_key {
            for d in self.delay.iter_mut() {
                d.fill(0.0);
            }
            self.config_key = key;
        }
        self.acmod = acmod;
        self.lfeon = lfeon;
        self.nfchans = NFCHANS[acmod as usize];
        self.fscod = fscod;
        self.hth_idx = hth_idx;
        self.rate_shift = rate_shift;
        self.num_blocks = num_blocks;
        self.bsid = bsid;
    }

    pub(super) fn parse_audfrm(
        &mut self,
        br: &mut BitReader<'_>,
        frame_words: usize,
        trace: &mut Trace,
    ) -> Result<(), E> {
        let nblk = self.num_blocks;
        let nfch = self.nfchans;
        let (expstre, ahte) = if nblk == 6 {
            (br.bit(), br.bit())
        } else {
            (true, false)
        };
        self.ahte = ahte;
        self.snroffststr = br.get(2) as u8;
        if self.snroffststr == 3 {
            return Err(bad("reserved snroffststr"));
        }
        if let Some(t) = trace.cur() {
            t.transproce_bit = Some(br.pos());
        }
        let transproce = br.bit();
        self.blkswe = br.bit();
        if let Some(t) = trace.cur() {
            t.dithflage_bit = Some(br.pos());
        }
        self.dithflage = br.bit();
        self.bamode = br.bit();
        self.frmfgaincode = br.bit();
        if let Some(t) = trace.cur() {
            t.dbaflde_bit = Some(br.pos());
        }
        self.dbaflde = br.bit();
        self.skipflde = br.bit();
        let spxattene = br.bit();
        // Coupling data.
        if self.acmod > 1 {
            self.cplstre_blk[0] = true;
            self.cplinu_blk[0] = br.bit();
            for blk in 1..nblk {
                self.cplstre_blk[blk] = br.bit();
                self.cplinu_blk[blk] = if self.cplstre_blk[blk] {
                    br.bit()
                } else {
                    self.cplinu_blk[blk - 1]
                };
            }
        } else {
            self.cplstre_blk = [false; 6];
            self.cplinu_blk = [false; 6];
        }
        let ncplblks = self.cplinu_blk[..nblk].iter().filter(|&&c| c).count();
        // Exponent strategies.
        if expstre {
            for blk in 0..nblk {
                self.expstr[CPL][blk] = if self.cplinu_blk[blk] {
                    br.get(2) as u8
                } else {
                    REUSE
                };
                for ch in 0..nfch {
                    self.expstr[ch][blk] = br.get(2) as u8;
                }
            }
        } else {
            self.features.frame_expstr = true;
            if self.acmod > 1 && ncplblks > 0 {
                let s = br.get(5) as usize;
                for blk in 0..6 {
                    self.expstr[CPL][blk] = FRM_EXPSTR[s][blk];
                }
            } else {
                self.expstr[CPL] = [REUSE; 6];
            }
            for ch in 0..nfch {
                let s = br.get(5) as usize;
                self.expstr[ch] = FRM_EXPSTR[s];
            }
        }
        if self.lfeon {
            for blk in 0..nblk {
                self.expstr[LFE][blk] = br.get(1) as u8;
            }
        }
        // Converter exponent strategy.
        if self.strmtyp == 0 {
            let convexpstre = if nblk != 6 { br.bit() } else { true };
            if convexpstre {
                br.skip(5 * nfch);
            }
        }
        // AHT.
        self.aht = [false; NCH];
        self.aht_read = [false; NCH];
        if ahte {
            let ncplregs = (0..6)
                .filter(|&b| self.cplstre_blk[b] || self.expstr[CPL][b] != REUSE)
                .count();
            if ncplblks == 6 && ncplregs == 1 {
                self.aht[CPL] = br.bit();
            }
            for ch in 0..nfch {
                let nregs = (0..6).filter(|&b| self.expstr[ch][b] != REUSE).count();
                if nregs == 1 {
                    self.aht[ch] = br.bit();
                }
            }
            if self.lfeon {
                let nregs = (0..6).filter(|&b| self.expstr[LFE][b] != REUSE).count();
                if nregs == 1 {
                    self.aht[LFE] = br.bit();
                }
            }
            self.features.aht |= self.aht.iter().any(|&a| a);
        }
        if self.snroffststr == 0 {
            self.frmcsnroffst = br.get(6) as u8;
            self.frmfsnroffst = br.get(4) as u8;
        }
        if let Some(t) = trace.cur() {
            t.transproc_data_bit = Some(br.pos());
        }
        if transproce {
            // Transient pre-noise processing data: parsed, not applied (FFmpeg 9.0.2 does
            // not apply it either; see harness/codecs/ac3/STATUS.md).
            self.features.tpnp = true;
            for _ in 0..nfch {
                if br.bit() {
                    br.skip(18); // transprocloc, transproclen
                }
            }
        }
        self.chinspxatten = [false; 5];
        if spxattene {
            for ch in 0..nfch {
                self.chinspxatten[ch] = br.bit();
                if self.chinspxatten[ch] {
                    self.spxattencod[ch] = br.get(5) as u8;
                }
            }
        }
        let blkstrtinfoe = if nblk != 1 { br.bit() } else { false };
        if blkstrtinfoe {
            let mut lg = 0usize;
            while (1usize << lg) < frame_words {
                lg += 1;
            }
            br.skip((nblk - 1) * (4 + lg));
        }
        self.firstspxcos = [true; 5];
        self.firstcplcos = [true; 5];
        self.firstcplleak = true;
        self.cplbndstrc_set_in_frame = false;
        self.spxbndstrc[..17].copy_from_slice(&DEF_SPX_BNDSTRC);
        Ok(())
    }
}
