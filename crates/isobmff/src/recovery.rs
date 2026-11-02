//! Compact source roll/prol descriptions, runs and explicitly scoped defaults.
//! Apple QTFF signed16 values; MPEG m76694 Original binary grammar; ISO FDAM3
//! same-fragment namespace; named MPEG2026 draft profile for version2 defaults.
//! No codec delay, native-clock, cross-fragment cache or decoder recovery claim.
use crate::{BoxView, BuildStatus, fields};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    Roll,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverySyntax {
    Versions0And1,
    Mpeg2026Draft,
}
#[derive(Clone, Copy, Debug)]
pub struct RecoveryLimits {
    pub max_descriptions: u32,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryDescription {
    pub index: u32,
    pub distance: i16,
    pub raw_bits: u16,
    pub source_range: Range<u64>,
}

#[derive(Debug)]
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
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn kind(&self) -> RecoveryKind {
        self.kind
    }
    pub fn version(&self) -> u8 {
        self.version
    }
    pub fn entry_count(&self) -> u32 {
        self.count
    }
    pub fn default_index(&self) -> Option<u32> {
        self.default
    }
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
    pub fn finish(mut self) -> Result<RecoveryDescription> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.ready
            .take()
            .ok_or(Error::Invalid("recovery description lookup not ready"))
    }
}

#[derive(Debug)]
pub struct RecoveryMappings {
    raw: BoxView,
    kind: RecoveryKind,
    parameter: Option<u32>,
    count: u32,
    records: usize,
}
impl RecoveryMappings {
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
    pub fn raw(&self) -> &BoxView {
        &self.raw
    }
    pub fn kind(&self) -> RecoveryKind {
        self.kind
    }
    pub fn run_count(&self) -> u32 {
        self.count
    }
    pub fn grouping_parameter(&self) -> Option<u32> {
        self.parameter
    }
    fn run(&self, row: u32) -> Result<(u32, u32)> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryDefinitionOrigin {
    Track,
    Fragment(u64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryMappingOrigin {
    Explicit,
    Absent,
    TrackDefault,
    FragmentDefault,
    InheritedTrackDefault,
}
#[derive(Clone, Copy, Debug)]
pub struct RecoveryScope<'a> {
    pub kind: RecoveryKind,
    pub first_ordinal: u64,
    pub sample_count: u64,
    pub fragment_ordinal: Option<u64>,
    pub track_descriptions: Option<&'a RecoveryDescriptions>,
    pub local_descriptions: Option<&'a RecoveryDescriptions>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoverySelection {
    pub kind: RecoveryKind,
    pub raw_mapping_index: Option<u32>,
    pub mapping_origin: RecoveryMappingOrigin,
    pub definition_origin: Option<RecoveryDefinitionOrigin>,
    pub description: Option<RecoveryDescription>,
}
impl RecoverySelection {
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
