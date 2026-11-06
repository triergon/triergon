//! Bounded initialization-extradata view of the same pinned packet receiver.
use crate::nal_packet::{NalConfigurationRefusal, PacketConfig};
use crate::{AvcConfigArrayKind, BuildStep, NalPacketConfiguration, NalPacketViewConfiguration};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

pub struct NalPacketConfigOutput {
    bytes: Bytes,
    source: Bytes,
    source_refusal: NalConfigurationRefusal,
}
impl NalPacketConfigOutput {
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.source_refusal.error()
    }
}

/// Original initialization bytes and source error for an observed unsupported
/// HEVC decoder-array declaration. No produced configuration is fabricated.
pub struct NalPacketConfigRejection {
    source: Bytes,
    error: Error,
    declared_array_type: u8,
    source_refusal: NalConfigurationRefusal,
}
impl NalPacketConfigRejection {
    pub fn source_bytes(&self) -> &Bytes {
        &self.source
    }
    pub fn source_error(&self) -> Error {
        self.error
    }
    pub fn declared_array_type(&self) -> u8 {
        self.declared_array_type
    }
    pub fn strict_source_refusal(&self) -> Option<Error> {
        self.source_refusal.error()
    }
    pub fn native_initialize_result(&self) -> i32 {
        -1_094_995_529
    }
}
pub struct NalPacketConfigWriter<'a, C = NalPacketConfiguration> {
    // A pool's limit is immutable; the output vector holds the pool handle.
    pool_limit: usize,
    config: &'a C,
    output: Option<BudgetVec<u8>>,
    maximum: usize,
    total: usize,
    array: usize,
    index: usize,
    copied: usize,
    phase: u8,
    failure: Option<Error>,
    rejected_array: Option<u8>,
    rejection_taken: bool,
}
impl<'a> NalPacketConfigWriter<'a> {
    pub fn new(
        pool: &Budget,
        configuration: &'a NalPacketConfiguration,
        maximum_output_bytes: usize,
    ) -> Result<Self> {
        Self::with_configuration(pool, configuration, maximum_output_bytes)
    }
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketConfigRejection> {
        self.rejection()
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketConfigOutput>> {
        self.run(work, cancel)
    }
}
impl<'a> NalPacketConfigWriter<'a, NalPacketViewConfiguration> {
    /// The same initialization writer over a compact configuration view.
    pub fn new_view(
        pool: &Budget,
        configuration: &'a NalPacketViewConfiguration,
        maximum_output_bytes: usize,
    ) -> Result<Self> {
        Self::with_configuration(pool, configuration, maximum_output_bytes)
    }
    pub fn take_receiver_rejection(&mut self) -> Option<NalPacketConfigRejection> {
        self.rejection()
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketConfigOutput>> {
        self.run(work, cancel)
    }
}
impl<'a, C> NalPacketConfigWriter<'a, C> {
    fn with_configuration(
        pool: &Budget,
        configuration: &'a C,
        maximum_output_bytes: usize,
    ) -> Result<Self>
    where
        C: PacketConfig,
    {
        if !configuration.raw().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        Ok(Self {
            pool_limit: pool.limit(),
            config: configuration,
            output: Some(BudgetVec::new(pool)),
            maximum: maximum_output_bytes,
            total: 0,
            array: 0,
            index: 0,
            copied: 0,
            phase: 0,
            failure: None,
            rejected_array: None,
            rejection_taken: false,
        })
    }
    fn nal(config: &C, index: usize) -> Option<(bool, &[u8], usize)>
    where
        C: PacketConfig,
    {
        if config.length_width().is_some() {
            config.config_nal(index).map(|n| {
                (
                    n.kind != Some(AvcConfigArrayKind::SequenceExtension),
                    n.payload(),
                    4,
                )
            })
        } else {
            (index == 0).then_some((true, config.raw().as_ref(), 0))
        }
    }
    fn rejection(&mut self) -> Option<NalPacketConfigRejection>
    where
        C: PacketConfig,
    {
        if self.rejection_taken {
            return None;
        }
        let declared_array_type = self.rejected_array?;
        let error = self.failure?;
        self.rejection_taken = true;
        Some(NalPacketConfigRejection {
            source: self.config.raw().clone(),
            error,
            declared_array_type,
            source_refusal: self.config.refusal(),
        })
    }
    fn run(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketConfigOutput>>
    where
        C: PacketConfig,
    {
        if let Some(e) = self.failure {
            return Err(e);
        }
        let r = self.advance(work, cancel);
        if let Err(e) = r
            && !matches!(e, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(e);
            self.output = None;
        }
        r
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<NalPacketConfigOutput>>
    where
        C: PacketConfig,
    {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                0 => {
                    if let Some(declared) = self.config.hevc_array_type(self.array) {
                        if !work.try_charge(1) {
                            return Ok(BuildStep::Yield);
                        }
                        if !matches!(declared, 32 | 33 | 34 | 39 | 40) {
                            self.rejected_array = Some(declared);
                            return Err(Error::Unsupported("pinned HEVC decoder array type"));
                        }
                        self.array += 1;
                        continue;
                    }
                    self.phase = 1;
                }
                1 => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    if let Some((selected, n, prefix)) = Self::nal(self.config, self.index) {
                        if selected {
                            let total = self
                                .total
                                .checked_add(prefix)
                                .and_then(|v| v.checked_add(n.len()))
                                .ok_or(Error::Overflow)?;
                            if total > self.maximum {
                                return Err(Error::Limit {
                                    requested: total,
                                    maximum: self.maximum,
                                });
                            }
                            self.total = total;
                        }
                        self.index += 1;
                    } else {
                        self.phase = 2;
                        self.index = 0;
                    }
                }
                2 => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    let minimum = self
                        .total
                        .checked_add(Bytes::owner_overhead_bytes())
                        .and_then(|v| v.checked_add(Budget::control_storage_bytes()))
                        .ok_or(Error::Overflow)?;
                    if minimum > self.pool_limit {
                        return Err(Error::Limit {
                            requested: minimum,
                            maximum: self.pool_limit,
                        });
                    }
                    self.output
                        .as_mut()
                        .ok_or(Error::Invalid("missing config output"))?
                        .try_reserve_exact(self.total)?;
                    self.phase = 3;
                }
                3 => {
                    let Some((selected, b, prefix)) = Self::nal(self.config, self.index) else {
                        self.phase = 4;
                        continue;
                    };
                    if !selected
                        || self.copied == b.len().checked_add(prefix).ok_or(Error::Overflow)?
                    {
                        if !work.try_charge(1) {
                            return Ok(BuildStep::Yield);
                        }
                        self.index += 1;
                        self.copied = 0;
                        continue;
                    }
                    let count = if self.copied < prefix {
                        prefix - self.copied
                    } else {
                        (b.len() - (self.copied - prefix)).min(4096)
                    }
                    .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX));
                    if count == 0 {
                        return Ok(BuildStep::Yield);
                    }
                    let out = self
                        .output
                        .as_mut()
                        .ok_or(Error::Invalid("missing config output"))?;
                    if self.copied < prefix {
                        out.try_extend_from_slice(&[0, 0, 0, 1][self.copied..self.copied + count])?;
                    } else {
                        let start = self.copied - prefix;
                        out.try_extend_from_slice(&b[start..start + count])?;
                    }
                    work.take(count as u64);
                    self.copied += count;
                }
                4 => {
                    if !work.try_charge(1) {
                        return Ok(BuildStep::Yield);
                    }
                    let b = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing config output"))?;
                    let bytes = match b.freeze() {
                        Ok(v) => v,
                        Err(e) => {
                            self.output = Some(e.buffer);
                            return Err(e.error);
                        }
                    };
                    self.phase = 5;
                    return Ok(BuildStep::Ready(NalPacketConfigOutput {
                        bytes,
                        source: self.config.raw().clone(),
                        source_refusal: self.config.refusal(),
                    }));
                }
                _ => return Ok(BuildStep::Complete),
            }
        }
    }
}
