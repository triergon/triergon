//! Development aids for the research examples, behind the `inspect` feature: per-block
//! parse statistics, the bit ranges of the SBR fill elements and the QMF slots Parametric
//! Stereo receives. Without the feature the records are empty and record nothing.

#[cfg(feature = "inspect")]
pub(crate) use collect::{FrameRecord, PsInput};
#[cfg(not(feature = "inspect"))]
pub(crate) use off::{FrameRecord, PsInput};

use crate::cce::CceState;
use crate::decoder::ElementState;
use crate::fft::Cpx;
use triergon_codec::Charged;

#[cfg(feature = "inspect")]
mod collect {
    use super::{CceState, Charged, Cpx, ElementState};
    use crate::ics::{ChannelData, MAX_SFB};
    use std::fmt::Write;

    /// What the decoder records of the last access unit.
    #[derive(Debug)]
    pub(crate) struct FrameRecord {
        /// Payload bit ranges of the SBR fill elements.
        sbr_fills: Vec<(usize, usize)>,
        /// One line per decoded channel and coupling channel of the last raw data block,
        /// once enabled.
        stats: Option<String>,
    }

    impl FrameRecord {
        pub(crate) fn new() -> FrameRecord {
            FrameRecord {
                sbr_fills: Vec::new(),
                stats: None,
            }
        }

        pub(crate) fn start_access_unit(&mut self) {
            self.sbr_fills.clear();
        }

        pub(crate) fn sbr_fill(&mut self, start: usize, end: usize) {
            self.sbr_fills.push((start, end));
        }

        pub(crate) fn sbr_fills(&self) -> &[(usize, usize)] {
            &self.sbr_fills
        }

        pub(crate) fn enable_stats(&mut self) {
            self.stats.get_or_insert_default();
        }

        pub(crate) fn stats(&self) -> &str {
            self.stats.as_deref().unwrap_or_default()
        }

        /// The statistics of the raw data block `elements` and `cce` hold.
        pub(crate) fn block(
            &mut self,
            elements: &[Charged<Box<ElementState>>],
            cce: &[Charged<Box<CceState>>],
        ) {
            let Some(stats) = &mut self.stats else {
                return;
            };
            stats.clear();
            for e in elements.iter().filter(|e| e.decoded) {
                for c in (0..2).filter(|&c| e.ch_decoded[c]) {
                    let cd = &e.cd[c];
                    let _ = write!(
                        stats,
                        "el{}/{} ch{} seq{} sh{} maxsfb{} grp{} cb{:#x} tns{} pulse{} pred{} ltp{}; ",
                        e.kind,
                        e.tag,
                        c,
                        cd.info.window_sequence,
                        cd.info.window_shape,
                        cd.info.max_sfb,
                        cd.info.num_window_groups,
                        codebooks(cd),
                        u8::from(cd.tns.present),
                        u8::from(cd.pulse_used),
                        u8::from(cd.info.predictor_data_present),
                        u8::from(cd.info.ltp[0].present)
                    );
                }
            }
            for c in cce {
                let _ = write!(
                    stats,
                    "cce{} dec{} ind{} dom{} seq{} g1={:.4} tns{} cb{:#x} maxsfb{} tgts{:?}; ",
                    c.tag,
                    u8::from(c.decoded),
                    u8::from(c.ind_sw),
                    u8::from(c.domain),
                    c.cd.info.window_sequence,
                    c.gains[1][0],
                    u8::from(c.cd.tns.present),
                    codebooks(&c.cd),
                    c.cd.info.max_sfb,
                    c.targets
                        .iter()
                        .map(|t| (t.is_cpe, t.tag, t.l, t.r))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    /// The codebooks the coded bands of `cd` use, one bit each.
    fn codebooks(cd: &ChannelData) -> u32 {
        let mut mask = 0u32;
        for g in 0..cd.info.num_window_groups {
            for &book in &cd.band_type[g * MAX_SFB..][..cd.info.max_sfb] {
                mask |= 1 << book;
            }
        }
        mask
    }

    /// The QMF slots Parametric Stereo received in the last frame.
    #[derive(Debug)]
    pub(crate) struct PsInput(Vec<f32>);

    impl PsInput {
        pub(crate) fn new() -> PsInput {
            PsInput(Vec::new())
        }

        pub(crate) fn capture(&mut self, x: &[[Cpx; 64]]) {
            self.0.clear();
            for v in x.iter().flatten() {
                self.0.extend([v.re, v.im]);
            }
        }

        pub(crate) fn values(&self) -> &[f32] {
            &self.0
        }
    }
}

#[cfg(not(feature = "inspect"))]
mod off {
    use super::{CceState, Charged, Cpx, ElementState};

    #[derive(Debug)]
    pub(crate) struct FrameRecord;

    impl FrameRecord {
        pub(crate) fn new() -> FrameRecord {
            FrameRecord
        }

        #[inline(always)]
        pub(crate) fn start_access_unit(&mut self) {}

        #[inline(always)]
        pub(crate) fn sbr_fill(&mut self, _start: usize, _end: usize) {}

        #[inline(always)]
        pub(crate) fn block(
            &mut self,
            _elements: &[Charged<Box<ElementState>>],
            _cce: &[Charged<Box<CceState>>],
        ) {
        }
    }

    #[derive(Debug)]
    pub(crate) struct PsInput;

    impl PsInput {
        pub(crate) fn new() -> PsInput {
            PsInput
        }

        #[inline(always)]
        pub(crate) fn capture(&mut self, _x: &[[Cpx; 64]]) {}
    }
}

#[cfg(feature = "inspect")]
impl crate::Decoder {
    /// Development aid: payload bit ranges of the SBR fill elements of the last frame.
    pub fn sbr_fill_ranges(&self) -> &[(usize, usize)] {
        self.core.inspect.sbr_fills()
    }

    /// Development aid: collect [`Self::stats`] from now on (it formats text per frame).
    pub fn enable_stats(&mut self) {
        self.core.inspect.enable_stats();
    }

    /// Development aid: the QMF slots the Parametric Stereo tool received in the last
    /// frame, 38 rows of 64 bands, re and im interleaved (empty without PS).
    pub fn ps_input(&self) -> &[f32] {
        self.core
            .elements
            .iter()
            .filter_map(|e| e.sbr.as_ref())
            .map(|s| s.ps_input.values())
            .find(|values| !values.is_empty())
            .unwrap_or_default()
    }

    /// Development statistics of the last decoded block (unstable format).
    pub fn stats(&self) -> &str {
        self.core.inspect.stats()
    }
}
