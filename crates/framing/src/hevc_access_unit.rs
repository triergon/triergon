//! A four-byte-start-code HEVC feed over complete length-prefixed samples.
use crate::{AvcAnnexBLimits, BuildStep, Codec, HvcConfigNalRef, HvcConfigurationView, NalHeader};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

const START_CODE: &[u8; 4] = &[0, 0, 0, 1];

/// The NAL count bounds the sample. The output byte bound includes prepended
/// parameter sets from the separately bounded configuration record.
pub type HevcAnnexBLimits = AvcAnnexBLimits;

/// Holds the hvcC record and its declaration-order VPS/SPS/PPS extent.
pub struct HevcAnnexBConfig {
    record: HvcConfigurationView,
    parameter_bytes: usize,
}

impl HevcAnnexBConfig {
    pub fn new(record: HvcConfigurationView) -> Result<Self> {
        let mut parameter_bytes = 0;
        for nal in record.nals() {
            let declared_parameter = is_parameter_set(&nal);
            if (declared_parameter || nal.header().parameter_set_kind().is_some())
                && !nal.type_matches_declaration()
            {
                return Err(Error::Invalid("hvcC parameter-set declaration"));
            }
            if declared_parameter {
                parameter_bytes = add_nal_bytes(parameter_bytes, nal.source_bytes().len())?;
            }
        }
        Ok(Self {
            record,
            parameter_bytes,
        })
    }

    pub fn record(&self) -> &HvcConfigurationView {
        &self.record
    }

    pub fn job<'a>(
        &'a self,
        sample: &'a [u8],
        limits: HevcAnnexBLimits,
    ) -> Result<HevcAnnexBJob<'a>> {
        if limits.maximum_packet_bytes == 0
            || limits.maximum_output_bytes == 0
            || limits.maximum_nals == 0
        {
            return Err(Error::Invalid("zero HEVC feed limit"));
        }
        if sample.is_empty() {
            return Err(Error::Invalid("empty HEVC access unit"));
        }
        check_limit(sample.len(), limits.maximum_packet_bytes)?;
        Ok(HevcAnnexBJob {
            config: self,
            sample,
            rest: Some(sample),
            limits,
            nals: 0,
            bytes: 0,
            random_access: false,
            failure: None,
        })
    }
}

fn is_parameter_set(nal: &HvcConfigNalRef<'_>) -> bool {
    matches!(nal.declared_nal_type(), 32..=34)
}

/// Borrows a fully checked access unit and its configuration. The HEVC IRAP
/// indication is a NAL type fact, independent of the container key flag.
pub struct HevcAnnexBAccessUnit<'a> {
    config: &'a HevcAnnexBConfig,
    sample: &'a [u8],
    random_access: bool,
    bytes: usize,
}

impl HevcAnnexBAccessUnit<'_> {
    pub const fn is_random_access(&self) -> bool {
        self.random_access
    }

    pub const fn byte_len(&self) -> usize {
        self.bytes
    }

    /// Length fields are re-read from the checked sample. Each segment is a
    /// static prefix or a borrowed NAL, in original declaration/sample order.
    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        let parameters = self
            .random_access
            .then_some(self.config)
            .into_iter()
            .flat_map(|config| {
                config
                    .record
                    .nals()
                    .filter(is_parameter_set)
                    .map(|nal| nal.source_bytes())
            });
        parameters
            .chain(self.source_nals())
            .flat_map(|nal| [START_CODE.as_slice(), nal])
    }

    pub fn configuration(&self) -> &HevcAnnexBConfig {
        self.config
    }

    /// Borrow the checked sample's NAL bodies in original order.
    pub fn source_nals(&self) -> impl Iterator<Item = &[u8]> {
        let width = usize::from(self.config.record.length_size().bytes());
        let mut rest = self.sample;
        std::iter::from_fn(move || {
            let (nal, tail) = next_nal(rest, width).ok()?;
            rest = tail;
            Some(nal)
        })
    }
}

pub struct HevcAnnexBJob<'a> {
    config: &'a HevcAnnexBConfig,
    sample: &'a [u8],
    rest: Option<&'a [u8]>,
    limits: HevcAnnexBLimits,
    nals: usize,
    bytes: usize,
    random_access: bool,
    failure: Option<Error>,
}

impl<'a> HevcAnnexBJob<'a> {
    /// One work unit covers the length field and fixed HEVC header. Even after
    /// finding IRAP, the complete sample must validate before publication.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HevcAnnexBAccessUnit<'a>>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.scan(work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn scan(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<HevcAnnexBAccessUnit<'a>>> {
        let Some(mut rest) = self.rest else {
            return Ok(BuildStep::Complete);
        };
        let width = usize::from(self.config.record.length_size().bytes());
        while !rest.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            self.nals = self.nals.checked_add(1).ok_or(Error::Overflow)?;
            check_limit(self.nals, self.limits.maximum_nals)?;
            let (nal, tail) = next_nal(rest, width)?;
            let nal_type = NalHeader::parse(Codec::Hevc, nal)?.nal_unit_type();
            self.random_access |= (16..=23).contains(&nal_type);
            self.bytes = add_nal_bytes(self.bytes, nal.len())?;
            check_limit(self.bytes, self.limits.maximum_output_bytes)?;
            self.rest = Some(tail);
            rest = tail;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut bytes = self.bytes;
        if self.random_access {
            bytes = bytes
                .checked_add(self.config.parameter_bytes)
                .ok_or(Error::Overflow)?;
            check_limit(bytes, self.limits.maximum_output_bytes)?;
        }
        self.rest = None;
        Ok(BuildStep::Ready(HevcAnnexBAccessUnit {
            config: self.config,
            sample: self.sample,
            random_access: self.random_access,
            bytes,
        }))
    }
}

fn next_nal(sample: &[u8], width: usize) -> Result<(&[u8], &[u8])> {
    let (prefix, payload) = sample.split_at_checked(width).ok_or(Error::Truncated)?;
    let size = prefix
        .iter()
        .fold(0usize, |size, &byte| (size << 8) | usize::from(byte));
    if size == 0 {
        return Err(Error::Invalid("empty HEVC NAL"));
    }
    payload.split_at_checked(size).ok_or(Error::Truncated)
}

fn add_nal_bytes(bytes: usize, nal_bytes: usize) -> Result<usize> {
    bytes
        .checked_add(START_CODE.len())
        .and_then(|n| n.checked_add(nal_bytes))
        .ok_or(Error::Overflow)
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        return Err(Error::Limit { requested, maximum });
    }
    Ok(())
}
