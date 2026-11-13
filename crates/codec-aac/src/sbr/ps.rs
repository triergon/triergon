//! Parametric Stereo (placeholder while the tool is rebuilt).

use crate::error::Result;
use crate::fft::Cpx;
use triergon_bitstream::CachedBitReader;
use triergon_codec::{Budget, BudgetVec};

#[derive(Debug)]
pub(crate) struct Ps;

impl Ps {
    pub(crate) fn new(_budget: &Budget) -> Result<Ps> {
        Ok(Ps)
    }

    pub(crate) fn reset(&mut self) {}

    pub(crate) fn renew(&mut self) {}

    pub(crate) fn parse(
        &mut self,
        br: &mut CachedBitReader<'_>,
        bits_left: usize,
    ) -> Result<usize> {
        br.skip(bits_left);
        Ok(bits_left)
    }

    pub(crate) fn apply(
        &mut self,
        x: &mut BudgetVec<[Cpx; 64]>,
        x_r: &mut BudgetVec<[Cpx; 64]>,
        _ready: bool,
    ) {
        for i in 0..32 {
            x_r[i] = x[i];
        }
    }
}
