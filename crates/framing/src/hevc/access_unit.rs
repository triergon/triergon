//! A four-byte-start-code HEVC feed over complete length-prefixed samples.
use crate::nal::annexb::START_CODE;
use crate::{
    BuildStep,
    avc::AvcAnnexBLimits,
    hevc::{HvcConfigNal, HvcConfiguration, PPS, VPS, is_irap},
    nal::{Codec, NalHeader, split_length_prefixed},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// The bounds of one sample's conversion; `maximum_output_bytes` counts the
/// parameter sets put before a random access point.
pub type HevcAnnexBLimits = AvcAnnexBLimits;

/// An `hvcC` record prepared for [`HevcAnnexBJob`], with its VPS, SPS and PPS NAL units.
#[derive(Debug)]
pub struct HevcAnnexBConfig {
    record: HvcConfiguration,
    parameter_bytes: usize,
}

impl HevcAnnexBConfig {
    /// Prepares `record` for [`HevcAnnexBJob`]: samples have NAL length fields of the
    /// width it declares, and a random access unit starts with the NAL units of its
    /// VPS, SPS and PPS arrays. Fails when a NAL unit's type and the type its array
    /// declares differ and either is a VPS, SPS or PPS, or the Annex B size of the
    /// parameter sets overflows `usize`.
    pub fn new(record: HvcConfiguration) -> Result<Self> {
        let mut parameter_bytes = 0;
        for nal in record.nals() {
            nal.check_parameter_set_declaration()?;
            if is_parameter_set(&nal) {
                parameter_bytes = add_nal_bytes(parameter_bytes, nal.source_bytes().len())?;
            }
        }
        Ok(Self {
            record,
            parameter_bytes,
        })
    }

    /// The record given to [`Self::new`].
    pub fn record(&self) -> &HvcConfiguration {
        &self.record
    }

    /// Starts the conversion of `sample`, whose NAL units each follow a length field of
    /// the width the record declares. Fails when a limit is zero, or the sample is
    /// empty or longer than `limits.maximum_packet_bytes`.
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

fn is_parameter_set(nal: &HvcConfigNal<'_>) -> bool {
    matches!(nal.declared_nal_type(), VPS..=PPS)
}

/// A checked access unit as Annex B segments, borrowing the sample and its
/// configuration. Its IRAP indication comes from the NAL types, not the container.
#[derive(Debug)]
pub struct HevcAnnexBAccessUnit<'a> {
    config: &'a HevcAnnexBConfig,
    sample: &'a [u8],
    random_access: bool,
    bytes: usize,
}

impl HevcAnnexBAccessUnit<'_> {
    /// Whether the sample holds a NAL unit of an IRAP type, 16 to 23.
    pub const fn is_random_access(&self) -> bool {
        self.random_access
    }

    /// Total length of [`Self::segments`] in bytes.
    pub const fn byte_len(&self) -> usize {
        self.bytes
    }

    /// The access unit as Annex B, in pieces to write one after another: a
    /// four-byte start code and then a NAL unit, for the NAL units of the record's
    /// VPS, SPS and PPS arrays when [`Self::is_random_access`] and then for each NAL
    /// unit of the sample. Iterating rereads only the length fields; it allocates
    /// and copies nothing.
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

    /// The configuration the job was created from.
    pub fn configuration(&self) -> &HevcAnnexBConfig {
        self.config
    }

    /// The sample's NAL units without their length fields, in sample order.
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

/// Converts one complete length-prefixed sample to a [`HevcAnnexBAccessUnit`]. It
/// checks every length field and NAL header of the sample and copies nothing.
/// [`HevcAnnexBConfig::job`] creates it.
#[derive(Debug)]
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
    /// Advances by at most the units in `work`, one per length field and NAL header,
    /// and returns the access unit once the whole sample is checked.
    ///
    /// Fails when `cancel` is set, a length field or NAL unit runs past the end of the
    /// sample, a NAL unit is shorter than its two-byte header or has
    /// `forbidden_zero_bit` set or `nuh_temporal_id_plus1` zero, or a limit is
    /// exceeded. Every later call returns the same error.
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
            self.random_access |= is_irap(nal_type);
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
    let (nal, rest) = split_length_prefixed(sample, width).ok_or(Error::Truncated)?;
    if nal.is_empty() {
        return Err(Error::Invalid("empty HEVC NAL"));
    }
    Ok((nal, rest))
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
