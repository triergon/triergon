//! Source-description transitions and track creation tags in a separately named
//! reference receiver operation. No sample/configuration bytes are reconstructed.
use crate::{BoxView, EncodedStream, Sample, SampleTable, StreamView, TrackHeader, fields};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, InputCursor, InputToken, Result, SourceId, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct EncodedConfigProfile;
#[derive(Clone, Debug)]
pub struct EncodedCreation {
    raw: BoxView,
    source_seconds: u64,
    demux: Option<[u8; 27]>,
}
impl EncodedCreation {
    /// Fixed bounded field action: at most14 year-search and12 month steps.
    /// The raw media header remains owned; movie/track creation fields are not
    /// substituted. Older ambiguous epochs stay explicitly unsupported.
    pub fn from_source(header: &TrackHeader, _profile: EncodedConfigProfile) -> Result<Self> {
        let raw = header.raw_mdhd.clone();
        let (version, flags) = fields::full(raw.payload())?;
        if flags != 0 {
            return Err(Error::Unsupported("encoded creation header flags"));
        }
        let seconds = match version {
            0 => u64::from(fields::u32_at(raw.payload(), 4)?),
            1 => fields::u64_at(raw.payload(), 4)?,
            _ => return Err(Error::Unsupported("encoded creation header version")),
        };
        let demux = if seconds == 0 {
            None
        } else {
            Some(calendar(seconds)?)
        };
        Ok(Self {
            raw,
            source_seconds: seconds,
            demux,
        })
    }
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn source_seconds(&self) -> u64 {
        self.source_seconds
    }
    pub fn demux(&self) -> Option<&[u8; 27]> {
        self.demux.as_ref()
    }
}
fn calendar(seconds: u64) -> Result<[u8; 27]> {
    if seconds < 2_082_844_800 {
        return Err(Error::Unsupported(
            "encoded creation ambiguous pre-Unix epoch profile",
        ));
    }
    calendar_bounded(seconds, 1970)
}
pub(crate) fn calendar_qt_seconds(seconds: u64) -> Result<[u8; 27]> {
    calendar_bounded(seconds, 1904)
}
fn calendar_bounded(seconds: u64, minimum_year: u64) -> Result<[u8; 27]> {
    let days = seconds / 86400;
    let prior = |year: u64| {
        let y = year - 1;
        365 * y + y / 4 - y / 100 + y / 400
    };
    let origin = prior(1904);
    if days >= prior(10000) - origin {
        return Err(Error::Unsupported(
            "encoded creation four-digit year profile",
        ));
    }
    let (mut lo, mut hi) = (minimum_year, 10000u64);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if prior(mid) - origin <= days {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let year = lo;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut left = days - (prior(year) - origin);
    let mut month = 1;
    for n in months {
        if left < n {
            break;
        }
        left -= n;
        month += 1;
    }
    let mut out = *b"0000-00-00T00:00:00.000000Z";
    let mut digits = |at: usize, width: usize, mut value: u64| {
        for i in (at..at + width).rev() {
            out[i] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    };
    digits(0, 4, year);
    digits(5, 2, month);
    digits(8, 2, left + 1);
    digits(11, 2, (seconds % 86400) / 3600);
    digits(14, 2, (seconds % 3600) / 60);
    digits(17, 2, seconds % 60);
    Ok(out)
}

/// Validate the exact retained source descriptions, independently of which
/// description happens to provide initial public extradata.
pub(crate) fn validate_configs(
    table: &SampleTable,
    configs: &[EncodedStream],
    pool: &Budget,
) -> Result<()> {
    validate_range_configs(table, configs, pool)?;
    configs[0].kind().require_regular_codec()
}

pub(crate) fn validate_range_configs(
    table: &SampleTable,
    configs: &[EncodedStream],
    pool: &Budget,
) -> Result<()> {
    if configs.is_empty() || configs.len() != table.descriptions().len() {
        return Err(Error::Invalid("encoded description coverage"));
    }
    if configs.len() > 64 {
        return Err(Error::Limit {
            requested: configs.len(),
            maximum: 64,
        });
    }
    let first = &configs[0];
    for (declared, config) in table.descriptions().iter().zip(configs) {
        let actual = config.source_description();
        if !actual.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if actual.index != declared.index
            || actual.format != declared.format
            || actual.data_reference_index != declared.data_reference_index
            || actual.raw().header() != declared.raw().header()
            || actual.raw().bytes().as_ref().as_ptr() != declared.raw().bytes().as_ref().as_ptr()
            || actual.raw().bytes().len() != declared.raw().bytes().len()
        {
            return Err(Error::Invalid("encoded description source association"));
        }
        if config.kind() != first.kind() || config.demux().time_base != first.demux().time_base {
            return Err(Error::Unsupported(
                "encoded codec/clock description transition",
            ));
        }
    }
    if configs.len() > 1 && !first.kind().is_video() {
        return Err(Error::Unsupported(
            "encoded audio description transition not established",
        ));
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct EncodedInitial {
    demux: StreamView,
    source_initial_description: u32,
    initial_config: BoxView,
    metadata_description: BoxView,
}
impl EncodedInitial {
    /// Supply the actual first validated source sample. Metadata/encoder remain
    /// from STSD's first declared entry; extradata follows its first used entry.
    pub fn new(
        table: &SampleTable,
        configs: &[EncodedStream],
        first: &Sample,
        input: &InputCursor,
        _profile: EncodedConfigProfile,
    ) -> Result<Self> {
        validate_configs(table, configs, input.budget())?;
        if first.ordinal != 1 {
            return Err(Error::Invalid("encoded initial sample ordinal"));
        }
        let at = usize::try_from(
            first
                .description_index
                .checked_sub(1)
                .ok_or(Error::Invalid("encoded initial description zero"))?,
        )
        .map_err(|_| Error::Overflow)?;
        let used = configs
            .get(at)
            .ok_or(Error::Invalid("encoded initial description absent"))?;
        let mut demux = configs[0].demux().clone();
        demux.extradata = used.demux().extradata.clone();
        Ok(Self {
            demux,
            source_initial_description: first.description_index,
            initial_config: used.configuration_box().clone(),
            metadata_description: configs[0].source_description().raw().clone(),
        })
    }
    pub fn demux(&self) -> &StreamView {
        &self.demux
    }
    pub fn source_initial_description(&self) -> u32 {
        self.source_initial_description
    }
    pub fn initial_configuration(&self) -> &BoxView {
        &self.initial_config
    }
    pub fn metadata_description(&self) -> &BoxView {
        &self.metadata_description
    }
}
#[derive(Clone, Debug)]
pub enum EncodedConfigStep {
    Yield,
    Ready(Option<Bytes>),
}
#[derive(Debug)]
pub struct EncodedConfigSequence<'a> {
    configs: &'a [EncodedStream],
    pool: Budget,
    source: SourceId,
    token: InputToken,
    ordinal: u64,
    count: u64,
    description: Option<u32>,
    failed: Option<Error>,
}
impl<'a> EncodedConfigSequence<'a> {
    pub fn new(
        table: &SampleTable,
        configs: &'a [EncodedStream],
        input: &InputCursor,
        _profile: EncodedConfigProfile,
    ) -> Result<Self> {
        validate_configs(table, configs, input.budget())?;
        Ok(Self {
            configs,
            pool: input.budget().clone(),
            source: input.source(),
            token: input.token(),
            ordinal: 0,
            count: table.sample_count(),
            description: None,
            failed: None,
        })
    }
    /// One real source event at a time; work0 leaves it wholly unconsumed. The
    /// owned result is the original charged config span, including equal-byte
    /// transitions. No packet repeat, coalescing or post-serialization repair.
    pub fn observe(
        &mut self,
        sample: &Sample,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EncodedConfigStep> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let answer = self.inner(sample, input, work, cancel);
        if let Err(e) = answer {
            self.failed = Some(e);
        }
        answer
    }
    fn inner(
        &mut self,
        sample: &Sample,
        input: &mut InputCursor,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EncodedConfigStep> {
        if !self.pool.same_pool(input.budget()) {
            return Err(Error::ForeignPool);
        }
        if input.source() != self.source {
            return Err(Error::SourceMismatch);
        }
        if input.generation() != self.token.generation() {
            return Err(Error::StaleGeneration {
                expected: self.token.generation(),
                received: input.generation(),
            });
        }
        input.read_into(&mut [], work, cancel)?;
        if !work.try_charge(1) {
            return Ok(EncodedConfigStep::Yield);
        }
        if sample.ordinal > self.count || self.ordinal.checked_add(1) != Some(sample.ordinal) {
            return Err(Error::Invalid("encoded config source event order"));
        }
        let at = usize::try_from(
            sample
                .description_index
                .checked_sub(1)
                .ok_or(Error::Invalid("encoded config description zero"))?,
        )
        .map_err(|_| Error::Overflow)?;
        let config = self
            .configs
            .get(at)
            .ok_or(Error::Invalid("encoded config description absent"))?;
        let changed = self
            .description
            .is_some_and(|old| old != sample.description_index);
        self.ordinal = sample.ordinal;
        self.description = Some(sample.description_index);
        Ok(EncodedConfigStep::Ready(if changed {
            Some(config.demux().extradata.clone())
        } else {
            None
        }))
    }
}
