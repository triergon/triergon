//! Per-block analysis dumps for the harness/codecs/ac3 comparison scripts (feature
//! `debug-dump`).

use super::{NCH, Substream};
use crate::tables::spx_band_start;

impl Substream {
    /// Analysis aid (feature `debug-dump`, never enabled in production builds): writes
    /// per-block coefficients/delay lines to $AC3DUMP and a JSON summary line to stderr
    /// when $AC3J is set. Used by the harness/codecs/ac3 comparison scripts.
    pub(super) fn debug_dump_block(&self) {
        if std::env::var_os("AC3J").is_some() {
            let baps: Vec<Vec<u8>> = (0..NCH).map(|c| self.bap[c].to_vec()).collect();
            let exps: Vec<Vec<u8>> = (0..NCH).map(|c| self.exps[c].to_vec()).collect();
            let spx = if self.spxinu {
                (spx_band_start(self.spx_begin), spx_band_start(self.spx_end))
            } else {
                (0, 0)
            };
            eprintln!(
                "J {{\"remat\":{:?},\"nremat\":{},\"end\":{:?},\"spx\":[{},{}],\"chinspx\":{:?},\"aht\":{:?},\"cpl\":{},\"cplrange\":[{},{}],\"bap\":{:?},\"exps\":{:?},\"spxstrtf\":{},\"spxbndsz\":{:?},\"spxco\":{:?},\"nblend\":{:?},\"sblend\":{:?},\"atten\":{:?},\"attencod\":{:?},\"dyn\":{:?},\"blksw\":{:?}}}",
                self.rematflg,
                self.nrematbd,
                self.endmant,
                spx.0,
                spx.1,
                self.chinspx,
                self.aht,
                self.cplinu,
                self.cplstrtmant,
                self.cplendmant,
                baps,
                exps,
                spx_band_start(self.spxstrtf),
                &self.spxbndsz[..self.nspxbnds],
                self.spxco,
                self.nblend,
                self.sblend,
                self.chinspxatten,
                self.spxattencod,
                self.dynrng,
                self.blksw
            );
        }
        if let Some(path) = std::env::var_os("AC3DUMP") {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
            {
                for ch in 0..NCH {
                    for v in self.coef[ch].iter().chain(self.delay[ch].iter()) {
                        let _ = f.write_all(&v.to_le_bytes());
                    }
                }
            }
        }
    }
}
