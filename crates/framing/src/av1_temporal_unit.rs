//! Section 5.2 temporal units from complete container samples.
//!
//! AV1 ISOBMFF v1.3.0 §2.4 permits an omitted size on the last OBU.
//! The selected receiver also needs a delimiter and in-band configuration at
//! random access. Preserve the source sample and expose the output in spans.
use crate::av1::push_leb_byte;
use crate::{Av1Configuration, Av1Obu, Av1ObuExtension, Av1ObuHeader, BuildStep};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

const SEQUENCE_HEADER: u8 = 1;
const TEMPORAL_DELIMITER: u8 = 2;
const METADATA: u8 = 5;
const DELIMITER: &[u8] = &[0x12, 0];

/// Checked, reusable view of the configuration needed at random access.
#[derive(Clone, Copy)]
pub struct Av1TemporalUnitConfig<'a> {
    obus: &'a [Av1Obu],
    length: usize,
}

impl<'a> Av1TemporalUnitConfig<'a> {
    pub fn new(configuration: &'a Av1Configuration) -> Result<Self> {
        if !configuration.fields().syntax_issues().is_empty() {
            return Err(Error::Invalid("AV1 configuration record flags"));
        }
        let obus = configuration.config_obus();
        let has_sequence = obus
            .first()
            .is_some_and(|obu| obu.facts().header.obu_type() == SEQUENCE_HEADER);
        let obus = if has_sequence { obus } else { &[] };
        let mut length = 0usize;
        for obu in obus.iter().filter(|obu| receiver_configuration(obu)) {
            if !obu.facts().header.syntax_issues().is_empty() {
                return Err(Error::Invalid("AV1 configuration OBU header"));
            }
            length = length
                .checked_add(obu.wire_bytes().len())
                .ok_or(Error::Overflow)?;
        }
        Ok(Self { obus, length })
    }

    fn segments(self) -> impl Iterator<Item = &'a [u8]> {
        self.obus
            .iter()
            .filter(|obu| receiver_configuration(obu))
            .map(|obu| obu.wire_bytes().as_ref())
    }
}

fn receiver_configuration(obu: &Av1Obu) -> bool {
    matches!(obu.facts().header.obu_type(), SEQUENCE_HEADER | METADATA)
}

#[derive(Clone, Copy, Debug)]
pub struct Av1TemporalUnitLimits {
    pub maximum_sample_bytes: usize,
    pub maximum_output_bytes: usize,
    pub maximum_obus: usize,
}

#[derive(Clone, Copy)]
struct RepairedHeader {
    start: usize,
    payload: usize,
    prefix: [u8; 7],
    length: usize,
}

impl RepairedHeader {
    fn new(sample: &[u8], start: usize, payload: usize) -> Result<Self> {
        let mut prefix = [0; 7];
        let mut length = payload - start;
        prefix[..length].copy_from_slice(&sample[start..payload]);
        prefix[0] |= 2;
        let mut size = u32::try_from(sample.len() - payload).map_err(|_| Error::Overflow)?;
        loop {
            let digit = (size & 127) as u8;
            size >>= 7;
            prefix[length] = digit | if size == 0 { 0 } else { 128 };
            length += 1;
            if size == 0 {
                break;
            }
        }
        Ok(Self {
            start,
            payload,
            prefix,
            length,
        })
    }
}

/// Borrows the source sample and any inserted configuration.
/// Iterating the output never copies a source or configuration payload.
pub struct Av1TemporalUnit<'a> {
    source: &'a [u8],
    delimiter_end: usize,
    configuration: Option<Av1TemporalUnitConfig<'a>>,
    repaired: Option<RepairedHeader>,
    length: usize,
}

impl Av1TemporalUnit<'_> {
    pub fn source(&self) -> &[u8] {
        self.source
    }

    pub const fn byte_len(&self) -> usize {
        self.length
    }

    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        let (delimiter, body) = self.source.split_at(self.delimiter_end);
        let delimiter = if delimiter.is_empty() {
            DELIMITER
        } else {
            delimiter
        };
        let (unchanged, header, payload) = match &self.repaired {
            Some(repaired) => (
                &self.source[self.delimiter_end..repaired.start],
                &repaired.prefix[..repaired.length],
                &self.source[repaired.payload..],
            ),
            None => (body, &[][..], &[][..]),
        };
        std::iter::once(delimiter)
            .chain(
                self.configuration
                    .into_iter()
                    .flat_map(Av1TemporalUnitConfig::segments),
            )
            .chain([unchanged, header, payload])
            .filter(|span| !span.is_empty())
    }
}

/// A resumable envelope scan for a complete one-temporal-unit MP4 or Matroska
/// sample. Each charge covers one OBU header and size field (at most 10 bytes).
/// Payloads are skipped by length, including a size-less final OBU.
pub struct Av1TemporalUnitJob<'a> {
    source: Option<&'a [u8]>,
    configuration: Option<Av1TemporalUnitConfig<'a>>,
    limits: Av1TemporalUnitLimits,
    offset: usize,
    obus: usize,
    delimiter_end: usize,
    has_sequence: bool,
    repaired: Option<RepairedHeader>,
    failure: Option<Error>,
}

impl<'a> Av1TemporalUnitJob<'a> {
    pub fn new(
        source: &'a [u8],
        configuration: Option<Av1TemporalUnitConfig<'a>>,
        random_access: bool,
        limits: Av1TemporalUnitLimits,
    ) -> Result<Self> {
        if limits.maximum_obus == 0
            || limits.maximum_sample_bytes == 0
            || limits.maximum_output_bytes == 0
        {
            return Err(Error::Invalid("zero AV1 temporal unit limit"));
        }
        if source.is_empty() {
            return Err(Error::Invalid("empty AV1 sample"));
        }
        check_limit(source.len(), limits.maximum_sample_bytes)?;
        Ok(Self {
            source: Some(source),
            configuration: configuration.filter(|config| random_access && config.length != 0),
            limits,
            offset: 0,
            obus: 0,
            delimiter_end: 0,
            has_sequence: false,
            repaired: None,
            failure: None,
        })
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1TemporalUnit<'a>>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.scan(work, cancel);
        if let Err(error) = result {
            self.source = None;
            self.failure = Some(error);
        }
        result
    }

    fn scan(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<BuildStep<Av1TemporalUnit<'a>>> {
        let Some(source) = self.source else {
            return Ok(BuildStep::Complete);
        };
        while self.offset < source.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(BuildStep::Yield);
            }
            let start = self.offset;
            self.obus = self.obus.checked_add(1).ok_or(Error::Overflow)?;
            check_limit(self.obus, self.limits.maximum_obus)?;
            let header = read_header(source, start)?;
            let mut payload = start + 1 + usize::from(header.extension_present());
            let end = if header.has_size_field() {
                let (size, width) = read_size(&source[payload..])?;
                payload += width;
                payload
                    .checked_add(size)
                    .filter(|end| *end <= source.len())
                    .ok_or(Error::Truncated)?
            } else {
                source.len()
            };
            match header.obu_type() {
                TEMPORAL_DELIMITER
                    if start != 0
                        || payload != end
                        || header.extension_present()
                        || !header.has_size_field() =>
                {
                    return Err(Error::Invalid(
                        "AV1 temporal delimiter placement or contents",
                    ));
                }
                TEMPORAL_DELIMITER => {
                    self.delimiter_end = end;
                }
                SEQUENCE_HEADER => self.has_sequence = true,
                _ => {}
            }
            if !header.has_size_field() {
                self.repaired = Some(RepairedHeader::new(source, start, payload)?);
            }
            self.offset = end;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.obus == 1 && self.delimiter_end != 0 {
            return Err(Error::Invalid(
                "AV1 temporal unit contains only a delimiter",
            ));
        }
        let configuration = self.configuration.filter(|_| !self.has_sequence);
        let extra = (if self.delimiter_end == 0 {
            DELIMITER.len()
        } else {
            0
        })
        .checked_add(configuration.map_or(0, |config| config.length))
        .ok_or(Error::Overflow)?;
        let extra = extra
            .checked_add(
                self.repaired
                    .map_or(0, |r| r.length - (r.payload - r.start)),
            )
            .ok_or(Error::Overflow)?;
        let length = source.len().checked_add(extra).ok_or(Error::Overflow)?;
        check_limit(length, self.limits.maximum_output_bytes)?;
        self.source = None;
        Ok(BuildStep::Ready(Av1TemporalUnit {
            source,
            delimiter_end: self.delimiter_end,
            configuration,
            repaired: self.repaired,
            length,
        }))
    }
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        return Err(Error::Limit { requested, maximum });
    }
    Ok(())
}

fn read_header(source: &[u8], start: usize) -> Result<Av1ObuHeader> {
    let raw_byte = source[start];
    let extension = if raw_byte & 4 == 0 {
        None
    } else {
        Some(Av1ObuExtension {
            raw_byte: *source.get(start + 1).ok_or(Error::Truncated)?,
        })
    };
    let header = Av1ObuHeader {
        raw_byte,
        extension,
    };
    if !header.syntax_issues().is_empty() {
        return Err(Error::Invalid("AV1 sample OBU header"));
    }
    Ok(header)
}

fn read_size(source: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0u64;
    let mut width = 0u8;
    for &byte in source.iter().take(8) {
        if push_leb_byte(&mut value, &mut width, byte)? {
            return Ok((
                usize::try_from(value).map_err(|_| Error::Overflow)?,
                usize::from(width),
            ));
        }
    }
    Err(Error::Truncated)
}
