//! A four-byte-start-code AVC feed, over complete length-prefixed samples.
use crate::nal::annexb::START_CODE;
use crate::{
    BuildStep,
    avc::{AvcConfigArrayKind, AvcConfigNal, AvcConfiguration, IDR},
    nal::{Codec, NalHeader, split_length_prefixed},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// Holds one bounded record. A random-access feed uses its SPS and PPS arrays.
#[derive(Debug)]
pub struct AvcAnnexBConfig {
    record: AvcConfiguration,
    parameter_bytes: usize,
}

impl AvcAnnexBConfig {
    /// Prepares `record` for [`AvcAnnexBJob`]: samples have NAL length fields of the
    /// width it declares, and an IDR access unit starts with its SPS and PPS NAL units.
    /// Fails when the Annex B size of those parameter sets overflows `usize`.
    pub fn new(record: AvcConfiguration) -> Result<Self> {
        let mut parameter_bytes = 0usize;
        for nal in record.nals().filter(parameter_set) {
            parameter_bytes = add_nal_bytes(parameter_bytes, nal.source_bytes().len())?;
        }
        Ok(Self {
            record,
            parameter_bytes,
        })
    }

    /// The record given to [`Self::new`].
    pub fn record(&self) -> &AvcConfiguration {
        &self.record
    }
}

fn parameter_set(nal: &AvcConfigNal<'_>) -> bool {
    matches!(
        nal.array_kind(),
        AvcConfigArrayKind::Sequence | AvcConfigArrayKind::Picture
    )
}

fn add_nal_bytes(bytes: usize, nal_bytes: usize) -> Result<usize> {
    bytes
        .checked_add(START_CODE.len())
        .and_then(|n| n.checked_add(nal_bytes))
        .ok_or(Error::Overflow)
}

/// The bounds of one sample's conversion to an Annex B access unit. Each must be
/// above 0.
#[derive(Clone, Copy, Debug)]
pub struct AvcAnnexBLimits {
    /// Largest sample accepted, in bytes.
    pub maximum_packet_bytes: usize,
    /// Largest access unit produced, in bytes, counting its start codes and the
    /// parameter sets put before a random access point.
    pub maximum_output_bytes: usize,
    /// Most NAL units in one sample.
    pub maximum_nals: usize,
}

/// A complete feed view. Parameter sets precede an IDR access unit and every
/// NAL keeps its original bytes. The container's key flag is a separate fact.
#[derive(Debug)]
pub struct AvcAnnexBAccessUnit<'a> {
    config: &'a AvcAnnexBConfig,
    sample: &'a [u8],
    idr: bool,
    bytes: usize,
}

impl AvcAnnexBAccessUnit<'_> {
    /// Whether the sample holds a NAL unit of type 5, a slice of an IDR picture.
    pub const fn is_idr(&self) -> bool {
        self.idr
    }

    /// Total length of [`Self::segments`] in bytes.
    pub const fn byte_len(&self) -> usize {
        self.bytes
    }

    /// The access unit as Annex B, in pieces to write one after another: a
    /// four-byte start code and then a NAL unit, for the record's SPS and PPS NAL
    /// units when [`Self::is_idr`] and then for each NAL unit of the sample.
    ///
    /// All bounds were checked before publication. Iterating reparses only the
    /// short length fields, with no descriptor allocation or payload copy.
    #[inline]
    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        AnnexBSegments {
            configuration: self.idr.then_some(&self.config.record),
            parameter_index: 0,
            sample: self.sample,
            length_width: usize::from(self.config.record.sample_length_size().bytes()),
            pending_payload: None,
        }
    }

    /// The configuration the job was created with.
    pub fn configuration(&self) -> &AvcAnnexBConfig {
        self.config
    }

    /// Borrow the checked sample's NAL bodies in original order.
    pub fn source_nals(&self) -> impl Iterator<Item = &[u8]> {
        let width = usize::from(self.config.record.sample_length_size().bytes());
        let mut rest = self.sample;
        std::iter::from_fn(move || {
            let (nal, tail) = next_nal(rest, width).ok()?;
            rest = tail;
            Some(nal)
        })
    }
}

struct AnnexBSegments<'a> {
    configuration: Option<&'a AvcConfiguration>,
    parameter_index: usize,
    sample: &'a [u8],
    length_width: usize,
    pending_payload: Option<&'a [u8]>,
}

impl<'a> AnnexBSegments<'a> {
    fn next_parameter_set(&mut self) -> Option<&'a [u8]> {
        let record = self.configuration?;
        while let Some(nal) = record.nal(self.parameter_index) {
            self.parameter_index += 1;
            if parameter_set(&nal) {
                return Some(nal.source_bytes());
            }
        }
        self.configuration = None;
        None
    }
}

impl<'a> Iterator for AnnexBSegments<'a> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(payload) = self.pending_payload.take() {
            return Some(payload);
        }
        let payload = if let Some(nal) = self.next_parameter_set() {
            nal
        } else {
            let (nal, tail) = next_nal(self.sample, self.length_width).ok()?;
            self.sample = tail;
            nal
        };
        self.pending_payload = Some(payload);
        Some(&START_CODE)
    }
}

/// Converts one complete length-prefixed sample to an [`AvcAnnexBAccessUnit`]. It
/// checks every length field and NAL header of the sample and copies nothing.
#[derive(Debug)]
pub struct AvcAnnexBJob<'a> {
    config: &'a AvcAnnexBConfig,
    sample: &'a [u8],
    rest: Option<&'a [u8]>,
    limits: AvcAnnexBLimits,
    nals: usize,
    bytes: usize,
    idr: bool,
    failure: Option<Error>,
}

impl<'a> AvcAnnexBJob<'a> {
    /// Starts the conversion of `sample`, whose NAL units each follow a length field of
    /// the width the record of `config` declares. Fails when a limit is zero, or the
    /// sample is empty or longer than `limits.maximum_packet_bytes`.
    pub fn new(
        config: &'a AvcAnnexBConfig,
        sample: &'a [u8],
        limits: AvcAnnexBLimits,
    ) -> Result<Self> {
        if limits.maximum_nals == 0
            || limits.maximum_output_bytes == 0
            || limits.maximum_packet_bytes == 0
        {
            return Err(Error::Invalid("zero AVC feed limit"));
        }
        if sample.is_empty() {
            return Err(Error::Invalid("empty AVC access unit"));
        }
        check_limit(sample.len(), limits.maximum_packet_bytes)?;
        Ok(Self {
            config,
            sample,
            rest: Some(sample),
            limits,
            nals: 0,
            bytes: 0,
            idr: false,
            failure: None,
        })
    }

    /// Advances by at most the units in `work` and returns the access unit once, as
    /// [`BuildStep`] describes.
    ///
    /// One work unit covers one checked length field and fixed NAL header.
    /// A malformed tail never publishes a partially converted access unit.
    ///
    /// Fails when `cancel` is set, a length field or NAL unit runs past the end of the
    /// sample, a NAL unit is empty or has `forbidden_zero_bit` set, or a limit is
    /// exceeded. Every later call returns the same error.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<AvcAnnexBAccessUnit<'a>>> {
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
    ) -> Result<BuildStep<AvcAnnexBAccessUnit<'a>>> {
        let Some(mut rest) = self.rest else {
            return Ok(BuildStep::Complete);
        };
        let width = usize::from(self.config.record.sample_length_size().bytes());
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
            self.idr |= NalHeader::parse(Codec::Avc, nal)?.nal_unit_type() == IDR;
            self.bytes = add_nal_bytes(self.bytes, nal.len())?;
            check_limit(self.bytes, self.limits.maximum_output_bytes)?;
            self.rest = Some(tail);
            rest = tail;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut bytes = self.bytes;
        if self.idr {
            bytes = bytes
                .checked_add(self.config.parameter_bytes)
                .ok_or(Error::Overflow)?;
            check_limit(bytes, self.limits.maximum_output_bytes)?;
        }
        self.rest = None;
        Ok(BuildStep::Ready(AvcAnnexBAccessUnit {
            config: self.config,
            sample: self.sample,
            idr: self.idr,
            bytes,
        }))
    }
}

fn next_nal(sample: &[u8], width: usize) -> Result<(&[u8], &[u8])> {
    let (nal, rest) = split_length_prefixed(sample, width).ok_or(Error::Truncated)?;
    if nal.is_empty() {
        return Err(Error::Invalid("empty AVC NAL"));
    }
    Ok((nal, rest))
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        return Err(Error::Limit { requested, maximum });
    }
    Ok(())
}
