//! The `roll` and `prol` recovery groups: sample group descriptions (`sgpd`,
//! [`RecoveryDescriptions`]) holding signed roll distances, and sample-to-group mappings
//! (`sbgp`, [`RecoveryMappings`]) assigning them to runs of samples. [`RecoveryLookup`]
//! finds the entry that applies to one sample of a [`RecoveryScope`], the sample table
//! or one fragment. A distance is what the file states, not a codec delay or proof of
//! decoder recovery.
//!
//! Specification: Apple QuickTime File Format (signed 16-bit distance); MPEG m76694;
//! ISO/IEC 14496-12 FDAM3 draft (fragment-local description indexes); MPEG 2026 draft
//! (version 2 default indexes).
use crate::{BuildStatus, boxes::BoxView, fields};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, WorkBudget};

/// The grouping type of a recovery group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    /// `roll`: roll recovery, whose distance counts the samples to decode for a member
    /// sample to decode correctly.
    Roll,
    /// `prol`: audio pre-roll, with the same distance field.
    Prol,
}
impl RecoveryKind {
    fn parse(value: [u8; 4]) -> Result<Self> {
        match &value {
            b"roll" => Ok(Self::Roll),
            b"prol" => Ok(Self::Prol),
            _ => Err(Error::Unsupported("recovery grouping type")),
        }
    }
}
/// Which `sgpd` versions [`RecoveryDescriptions::parse`] accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverySyntax {
    /// Versions 0 and 1, which state no default index; version 2 fails with
    /// `Error::Unsupported`.
    Versions0And1,
    /// Versions 0 to 2, reading version 2's default description index as the MPEG
    /// 2026 draft text lays it out. The draft is treated as experimental.
    Mpeg2026Draft,
}
/// Bounds on recovery groups; exceeding one fails with `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryLimits {
    /// Most entries in an `sgpd` box; 65,535 by default.
    pub max_descriptions: u32,
    /// Most runs in an `sbgp` box; 1,000,000 by default.
    pub max_runs: u32,
}
impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            max_descriptions: 65535,
            max_runs: 1_000_000,
        }
    }
}
fn limit(value: u32, maximum: u32) -> Result<()> {
    if value > maximum {
        return Err(Error::Limit {
            requested: usize::try_from(value).map_err(|_| Error::Overflow)?,
            maximum: usize::try_from(maximum).map_err(|_| Error::Overflow)?,
        });
    }
    Ok(())
}
/// One recovery description entry: a signed distance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryDescription {
    /// 1-based index of the entry in its `sgpd` box.
    pub index: u32,
    /// `roll_distance`, in samples: negative for the samples before a member sample
    /// that must be decoded first, positive for the samples after it that must be
    /// decoded before output is correct.
    pub distance: i16,
    /// The same 16 bits, unsigned, as stored.
    pub raw_bits: u16,
    /// Absolute byte range of the 2-byte field in the file.
    pub source_range: Range<u64>,
}

/// A parsed recovery sample group description box (`sgpd` of grouping type `roll` or
/// `prol`), kept as the original box; entries are read one at a time through
/// [`locate`](Self::locate).
#[derive(Clone, Debug)]
pub struct RecoveryDescriptions {
    raw: BoxView,
    kind: RecoveryKind,
    version: u8,
    count: u32,
    default: Option<u32>,
    length: u32,
    records: usize,
}
impl RecoveryDescriptions {
    /// Parses an `sgpd` box. Fails with `Error::Unsupported` for nonzero flags, a
    /// version `syntax` does not accept or a grouping type other than `roll` and
    /// `prol`, `Error::Invalid` for another box or trailing bytes, `Error::Truncated`
    /// when the entries do not fit, and `Error::Limit` for more entries than
    /// `limits.max_descriptions`.
    pub fn parse(raw: BoxView, syntax: RecoverySyntax, limits: RecoveryLimits) -> Result<Self> {
        if raw.header().kind != *b"sgpd" {
            return Err(Error::Invalid("not sgpd"));
        }
        let body = raw.payload();
        let (version, flags) = fields::full(body)?;
        if flags != 0 || version > 2 {
            return Err(Error::Unsupported("recovery description version/flags"));
        }
        if version == 2 && syntax != RecoverySyntax::Mpeg2026Draft {
            return Err(Error::Unsupported(
                "recovery version2 requires explicit draft profile",
            ));
        }
        let kind = RecoveryKind::parse(fields::fourcc(body, 4)?)?;
        let length = if version == 0 {
            2
        } else {
            fields::u32_at(body, 8)?
        };
        let default = if version == 2 {
            Some(fields::u32_at(body, 12)?)
        } else {
            None
        };
        let count_at: usize = match version {
            0 => 8,
            1 => 12,
            _ => 16,
        };
        let count = fields::u32_at(body, count_at)?;
        let records = count_at.checked_add(4).ok_or(Error::Overflow)?;
        limit(count, limits.max_descriptions)?;
        if length != 0 {
            fields::exact_rows(
                body,
                records,
                count,
                usize::try_from(length).map_err(|_| Error::Overflow)?,
            )?;
        } else {
            let minimum = usize::try_from(count)
                .map_err(|_| Error::Overflow)?
                .checked_mul(4)
                .and_then(|n| records.checked_add(n))
                .ok_or(Error::Overflow)?;
            if minimum > body.len() {
                return Err(Error::Truncated);
            }
            if count == 0 && records != body.len() {
                return Err(Error::Invalid("recovery description trailing bytes"));
            }
        }
        Ok(Self {
            raw,
            kind,
            version,
            count,
            default,
            length,
            records,
        })
    }
    /// The whole `sgpd` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// The grouping type.
    pub fn kind(&self) -> RecoveryKind {
        self.kind
    }
    /// Box version: 0, 1 or 2.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// `default_length`: bytes per entry, or 0 when each entry states its own length;
    /// 2 for version 0, which has no such field.
    pub fn fixed_entry_length(&self) -> u32 {
        self.length
    }
    /// Number of entries.
    pub fn entry_count(&self) -> u32 {
        self.count
    }
    /// Version 2's `default_sample_description_index`, the entry for samples no
    /// mapping assigns, 0 for none; `None` for versions 0 and 1.
    pub fn default_index(&self) -> Option<u32> {
        self.default
    }
    /// Offset in the box payload, in bytes, of the first entry.
    pub fn validation_start(&self) -> usize {
        self.records
    }
    // Continues a walk at entry `index`, `offset` bytes into the payload. The
    // caller passes only an end that a lookup of the entry before returned.
    pub(crate) fn validation_at(
        &self,
        index: u32,
        offset: usize,
    ) -> Result<RecoveryDescriptionLookup<'_>> {
        if index == 0
            || index > self.count
            || offset < self.records
            || offset > self.raw.payload().len()
        {
            return Err(Error::Invalid("recovery validation continuation"));
        }
        Ok(RecoveryDescriptionLookup {
            descriptions: self,
            target: index,
            row: index - 1,
            offset,
            ready: None,
            failed: None,
        })
    }
    /// A lookup of the 1-based entry `index`; `Error::Invalid` when it is 0 or above
    /// [`entry_count`](Self::entry_count).
    pub fn locate(&self, index: u32) -> Result<RecoveryDescriptionLookup<'_>> {
        if index == 0 || index > self.count {
            return Err(Error::Invalid("recovery description index"));
        }
        let row = if self.length == 0 { 0 } else { index - 1 };
        let offset = usize::try_from(row)
            .map_err(|_| Error::Overflow)?
            .checked_mul(usize::try_from(self.length).map_err(|_| Error::Overflow)?)
            .and_then(|n| self.records.checked_add(n))
            .ok_or(Error::Overflow)?;
        Ok(RecoveryDescriptionLookup {
            descriptions: self,
            target: index,
            row,
            offset,
            ready: None,
            failed: None,
        })
    }
}

/// Reads one entry of a [`RecoveryDescriptions`], one entry per work unit; where
/// entries state their own lengths it walks from the first entry.
#[derive(Debug)]
pub struct RecoveryDescriptionLookup<'a> {
    descriptions: &'a RecoveryDescriptions,
    target: u32,
    row: u32,
    offset: usize,
    ready: Option<RecoveryDescription>,
    failed: Option<Error>,
}
impl RecoveryDescriptionLookup<'_> {
    /// Advances the lookup. Fails with `Error::Truncated` for an entry shorter than 2
    /// bytes or cut off, `Error::Unsupported` when the entry sought is longer than 2
    /// bytes, and `Error::Invalid` for bytes after the last entry. An error is kept and
    /// returned by every later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(BuildStatus::Ready);
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let body = self.descriptions.raw.payload();
            let (length, at) = if self.descriptions.length == 0 {
                (
                    fields::u32_at(body, self.offset)?,
                    self.offset.checked_add(4).ok_or(Error::Overflow)?,
                )
            } else {
                (self.descriptions.length, self.offset)
            };
            let length = usize::try_from(length).map_err(|_| Error::Overflow)?;
            fields::slice(body, at, length)?;
            let end = at.checked_add(length).ok_or(Error::Overflow)?;
            let index = self.row.checked_add(1).ok_or(Error::Overflow)?;
            if length < 2 {
                return Err(Error::Truncated);
            }
            if index == self.target {
                if length != 2 {
                    return Err(Error::Unsupported("recovery entry extension"));
                }
                if index == self.descriptions.count && end != body.len() {
                    return Err(Error::Invalid("recovery description trailing bytes"));
                }
                let raw_bits = fields::u16_at(body, at)?;
                let start = self
                    .descriptions
                    .raw
                    .header()
                    .payload_range()?
                    .start
                    .checked_add(u64::try_from(at).map_err(|_| Error::Overflow)?)
                    .ok_or(Error::Overflow)?;
                self.ready = Some(RecoveryDescription {
                    index,
                    raw_bits,
                    distance: i16::from_be_bytes(raw_bits.to_be_bytes()),
                    source_range: start..start.checked_add(2).ok_or(Error::Overflow)?,
                });
            } else {
                self.row = index;
                self.offset = end;
            }
        }
    }
    /// The entry, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned.
    pub fn finish(mut self) -> Result<RecoveryDescription> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("recovery description lookup not ready"))
    }
}

/// A parsed recovery sample-to-group box (`sbgp` of grouping type `roll` or `prol`),
/// kept as the original box: runs of samples, each with the index of the description
/// that applies to them.
#[derive(Clone, Debug)]
pub struct RecoveryMappings {
    raw: BoxView,
    kind: RecoveryKind,
    parameter: Option<u32>,
    count: u32,
    records: usize,
}
impl RecoveryMappings {
    /// Parses an `sbgp` box. Fails with `Error::Unsupported` for a version above 1,
    /// nonzero flags or a grouping type other than `roll` and `prol`, `Error::Invalid`
    /// for another box or runs that do not fill the box exactly, and `Error::Limit`
    /// for more runs than `limits.max_runs`.
    pub fn parse(raw: BoxView, limits: RecoveryLimits) -> Result<Self> {
        if raw.header().kind != *b"sbgp" {
            return Err(Error::Invalid("not sbgp"));
        }
        let body = raw.payload();
        let (version, flags) = fields::full(body)?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported("recovery mapping version/flags"));
        }
        let kind = RecoveryKind::parse(fields::fourcc(body, 4)?)?;
        let parameter = if version == 1 {
            Some(fields::u32_at(body, 8)?)
        } else {
            None
        };
        let count_at = if version == 1 { 12usize } else { 8usize };
        let count = fields::u32_at(body, count_at)?;
        let records = count_at.checked_add(4).ok_or(Error::Overflow)?;
        fields::exact_rows(body, records, count, 8)?;
        limit(count, limits.max_runs)?;
        Ok(Self {
            raw,
            kind,
            parameter,
            count,
            records,
        })
    }
    /// The whole `sbgp` box.
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    /// The grouping type.
    pub fn kind(&self) -> RecoveryKind {
        self.kind
    }
    /// Number of runs.
    pub fn run_count(&self) -> u32 {
        self.count
    }
    /// Version 1's `grouping_type_parameter`; `None` for version 0.
    pub fn grouping_parameter(&self) -> Option<u32> {
        self.parameter
    }
    /// The 0-based run `row` as stored: its `sample_count` and its
    /// `group_description_index`, where 0 means no group, 1 to 0xFFFF name an entry of
    /// the track's `sgpd`, and 0x10000 + n names entry n of the fragment's own `sgpd`.
    /// `Error::Invalid` when `row` is not below [`run_count`](Self::run_count).
    pub fn run(&self, row: u32) -> Result<(u32, u32)> {
        if row >= self.count {
            return Err(Error::Invalid("recovery run index"));
        }
        let at = usize::try_from(row)
            .map_err(|_| Error::Overflow)?
            .checked_mul(8)
            .and_then(|n| self.records.checked_add(n))
            .ok_or(Error::Overflow)?;
        Ok((
            fields::u32_at(self.raw.payload(), at)?,
            fields::u32_at(
                self.raw.payload(),
                at.checked_add(4).ok_or(Error::Overflow)?,
            )?,
        ))
    }
}

/// Which `sgpd` box a selected description comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryDefinitionOrigin {
    /// The track's own, in its sample table.
    Track,
    /// The fragment's own, in its `traf`; the value is the scope's
    /// [`fragment_ordinal`](RecoveryScope::fragment_ordinal).
    Fragment(u64),
}
/// Where the description index for a sample comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryMappingOrigin {
    /// The `sbgp` run holding the sample.
    Explicit,
    /// Nowhere: the scope has no mapping and no default applies.
    Absent,
    /// The default index of the track's `sgpd`, outside fragments.
    TrackDefault,
    /// The default index of the fragment's own `sgpd`.
    FragmentDefault,
    /// The default index of the track's `sgpd`, in a fragment without an `sgpd` of its
    /// own.
    InheritedTrackDefault,
}
/// Samples of a track whose recovery groups share one set of boxes: those of the
/// sample table, or those of one fragment.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryScope<'a> {
    /// The grouping type the scope's boxes must have.
    pub kind: RecoveryKind,
    /// 1-based ordinal of the scope's first sample in the track; never zero.
    pub first_ordinal: u64,
    /// Number of samples in the scope.
    pub sample_count: u64,
    /// For a fragment, its number as the caller counts it, reported back in
    /// [`RecoveryDefinitionOrigin::Fragment`]; `None` for the sample table.
    pub fragment_ordinal: Option<u64>,
    /// The track's `sgpd` of this kind, from its sample table.
    pub track_descriptions: Option<&'a RecoveryDescriptions>,
    /// The fragment's own `sgpd` of this kind, from its `traf`; fragment scopes only.
    pub local_descriptions: Option<&'a RecoveryDescriptions>,
    /// The scope's `sbgp` of this kind, whose runs must cover all its samples.
    pub mappings: Option<&'a RecoveryMappings>,
}
impl RecoveryScope<'_> {
    pub(crate) fn end(self) -> Result<u64> {
        self.first_ordinal
            .checked_add(self.sample_count)
            .ok_or(Error::Overflow)
    }
    pub(crate) fn validate(self, limits: RecoveryLimits) -> Result<()> {
        if self.first_ordinal == 0 {
            return Err(Error::Invalid("zero recovery scope ordinal"));
        }
        self.end()?;
        if self.local_descriptions.is_some() && self.fragment_ordinal.is_none() {
            return Err(Error::Invalid(
                "local recovery descriptions outside fragment scope",
            ));
        }
        for descriptions in [self.track_descriptions, self.local_descriptions]
            .into_iter()
            .flatten()
        {
            if descriptions.kind != self.kind {
                return Err(Error::Invalid("recovery grouping kind mismatch"));
            }
            limit(descriptions.count, limits.max_descriptions)?;
        }
        if let Some(track) = self.track_descriptions {
            if track.count >= 0x10000 {
                return Err(Error::Invalid("track recovery description namespace"));
            }
            if track
                .default
                .is_some_and(|index| index != 0 && (index >= 0x10000 || index > track.count))
            {
                return Err(Error::Invalid("track recovery default index"));
            }
        }
        if let Some(mapping) = self.mappings {
            if mapping.kind != self.kind {
                return Err(Error::Invalid("recovery grouping kind mismatch"));
            }
            limit(mapping.count, limits.max_runs)?;
            if mapping.parameter.is_some_and(|value| value != 0) {
                return Err(Error::Unsupported("recovery grouping parameter semantics"));
            }
        }
        Ok(())
    }
}

/// The recovery group entry that applies to one sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoverySelection {
    /// The grouping type.
    pub kind: RecoveryKind,
    /// `group_description_index` of the `sbgp` run holding the sample, as stored;
    /// `None` when the scope has no mapping.
    pub raw_mapping_index: Option<u32>,
    /// Where the description index comes from.
    pub mapping_origin: RecoveryMappingOrigin,
    /// Which `sgpd` the description comes from; `None` without a description.
    pub definition_origin: Option<RecoveryDefinitionOrigin>,
    /// The entry that applies; `None` when the sample is in no group.
    pub description: Option<RecoveryDescription>,
}
impl RecoverySelection {
    /// Samples to decode before this one: the magnitude of a zero or negative distance,
    /// or 0 without a description. `Error::Unsupported` for a positive distance.
    pub fn preceding_samples(&self) -> Result<u16> {
        match self.description.as_ref().map(|entry| entry.distance) {
            Some(distance) if distance > 0 => Err(Error::Unsupported(
                "positive recovery distance needs postroll",
            )),
            Some(distance) => Ok(distance.unsigned_abs()),
            None => Ok(0),
        }
    }
    pub(crate) fn absent(kind: RecoveryKind) -> Self {
        Self {
            kind,
            raw_mapping_index: None,
            mapping_origin: RecoveryMappingOrigin::Absent,
            definition_origin: None,
            description: None,
        }
    }
}

fn definition(
    scope: RecoveryScope<'_>,
    raw: u32,
) -> Result<(&RecoveryDescriptions, u32, RecoveryDefinitionOrigin)> {
    if raw == 0 || raw == 0x10000 {
        return Err(Error::Unsupported("recovery fragment index zero"));
    }
    let (table, index, origin) = if raw < 0x10000 {
        (
            scope
                .track_descriptions
                .ok_or(Error::Invalid("missing track recovery description"))?,
            raw,
            RecoveryDefinitionOrigin::Track,
        )
    } else {
        let fragment = scope.fragment_ordinal.ok_or(Error::Invalid(
            "local recovery reference outside fragment scope",
        ))?;
        (
            scope
                .local_descriptions
                .ok_or(Error::Invalid("missing fragment recovery description"))?,
            raw.checked_sub(0x10000).ok_or(Error::Overflow)?,
            RecoveryDefinitionOrigin::Fragment(fragment),
        )
    };
    if index == 0 || index > table.count {
        return Err(Error::Invalid("recovery description index"));
    }
    Ok((table, index, origin))
}

fn default_mapping(scope: RecoveryScope<'_>) -> Result<(RecoveryMappingOrigin, Option<u32>)> {
    if scope.fragment_ordinal.is_some() {
        if let Some(local) = scope.local_descriptions {
            if let Some(index) = local.default {
                return Ok((
                    RecoveryMappingOrigin::FragmentDefault,
                    (index != 0 && index != 0x10000).then_some(index),
                ));
            }
            if scope
                .track_descriptions
                .and_then(|table| table.default)
                .is_some_and(|value| value != 0)
            {
                return Err(Error::Unsupported(
                    "local version1 recovery default inheritance remains unresolved",
                ));
            }
            return Ok((RecoveryMappingOrigin::Absent, None));
        }
        if let Some(index) = scope.track_descriptions.and_then(|table| table.default) {
            return Ok((
                RecoveryMappingOrigin::InheritedTrackDefault,
                (index != 0).then_some(index),
            ));
        }
    } else if let Some(index) = scope.track_descriptions.and_then(|table| table.default) {
        return Ok((
            RecoveryMappingOrigin::TrackDefault,
            (index != 0).then_some(index),
        ));
    }
    Ok((RecoveryMappingOrigin::Absent, None))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Runs,
    Select,
    Description,
    Ready,
}
/// Finds the recovery group entry for one sample of a [`RecoveryScope`]: it reads the
/// scope's `sbgp` runs, one per work unit, then the entry they or a default index name.
#[derive(Debug)]
pub struct RecoveryLookup<'a> {
    scope: RecoveryScope<'a>,
    relative: u64,
    row: u32,
    total: u64,
    raw_index: Option<u32>,
    phase: Phase,
    description: Option<RecoveryDescriptionLookup<'a>>,
    selection: Option<RecoverySelection>,
    failed: Option<Error>,
}
impl<'a> RecoveryLookup<'a> {
    /// A lookup for the 1-based track sample `ordinal`. Fails with `Error::Invalid`
    /// when the sample is outside the scope, the scope's first ordinal is zero, its
    /// boxes have another grouping type, fragment descriptions are given outside a
    /// fragment or the track's entries or default index are out of range; with
    /// `Error::Unsupported` for a nonzero grouping parameter; and with `Error::Limit`
    /// past `limits`.
    pub fn new(scope: RecoveryScope<'a>, ordinal: u64, limits: RecoveryLimits) -> Result<Self> {
        scope.validate(limits)?;
        if ordinal < scope.first_ordinal || ordinal >= scope.end()? {
            return Err(Error::Invalid("recovery sample outside scope"));
        }
        Ok(Self {
            scope,
            relative: ordinal
                .checked_sub(scope.first_ordinal)
                .ok_or(Error::Overflow)?,
            row: 0,
            total: 0,
            raw_index: None,
            phase: if scope.mappings.is_some() {
                Phase::Runs
            } else {
                Phase::Select
            },
            description: None,
            selection: None,
            failed: None,
        })
    }
    /// Advances the lookup. Fails with `Error::Unsupported` when the runs cover fewer
    /// samples than the scope or the default index that applies is ambiguous,
    /// `Error::Invalid` when they cover more or name a missing entry, and as
    /// [`RecoveryDescriptionLookup::poll`]. An error is kept and returned by every
    /// later poll.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.poll_inner(work, cancel);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn poll_inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match self.phase {
                Phase::Runs => {
                    let mappings = self
                        .scope
                        .mappings
                        .ok_or(Error::Invalid("missing recovery mapping"))?;
                    if self.row == mappings.count {
                        if self.total < self.scope.sample_count {
                            return Err(Error::Unsupported("partial recovery map coverage"));
                        }
                        self.phase = Phase::Select;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    let (count, index) = mappings.run(self.row)?;
                    let end = self
                        .total
                        .checked_add(u64::from(count))
                        .ok_or(Error::Overflow)?;
                    if end > self.scope.sample_count {
                        return Err(Error::Invalid("recovery map exceeds scope"));
                    }
                    if index != 0 {
                        definition(self.scope, index)?;
                    }
                    if self.relative >= self.total && self.relative < end {
                        self.raw_index = Some(index);
                    }
                    self.row = self.row.checked_add(1).ok_or(Error::Overflow)?;
                    self.total = end;
                }
                Phase::Select => {
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    let (origin, selected) = if self.scope.mappings.is_some() {
                        let raw = self
                            .raw_index
                            .ok_or(Error::Invalid("recovery target mapping missing"))?;
                        if raw == 0 {
                            if default_mapping(self.scope)?.1.is_some() {
                                return Err(Error::Unsupported(
                                    "explicit zero recovery index with default remains unresolved",
                                ));
                            }
                            (RecoveryMappingOrigin::Explicit, None)
                        } else {
                            (RecoveryMappingOrigin::Explicit, Some(raw))
                        }
                    } else {
                        default_mapping(self.scope)?
                    };
                    let mut selection = RecoverySelection::absent(self.scope.kind);
                    selection.raw_mapping_index = self.raw_index;
                    selection.mapping_origin = origin;
                    if let Some(raw) = selected {
                        let (table, index, definition_origin) = definition(self.scope, raw)?;
                        self.description = Some(table.locate(index)?);
                        selection.definition_origin = Some(definition_origin);
                        self.phase = Phase::Description;
                    } else {
                        self.phase = Phase::Ready;
                    }
                    self.selection = Some(selection);
                }
                Phase::Description => {
                    if self
                        .description
                        .as_mut()
                        .ok_or(Error::Invalid("missing recovery entry lookup"))?
                        .poll(work, cancel)?
                        == BuildStatus::Yield
                    {
                        return Ok(BuildStatus::Yield);
                    }
                    let entry = self
                        .description
                        .take()
                        .ok_or(Error::Invalid("missing recovery entry lookup"))?
                        .finish()?;
                    self.selection
                        .as_mut()
                        .ok_or(Error::Invalid("missing recovery selection"))?
                        .description = Some(entry);
                    self.phase = Phase::Ready;
                }
                Phase::Ready => return Ok(BuildStatus::Ready),
            }
        }
    }
    /// The selection, once [`poll`](Self::poll) has returned `Ready`; `Error::Invalid`
    /// before that, or the error a poll returned.
    pub fn finish(mut self) -> Result<RecoverySelection> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.phase != Phase::Ready {
            return Err(Error::Invalid("recovery association not ready"));
        }
        self.selection
            .take()
            .ok_or(Error::Invalid("missing recovery selection"))
    }
}
