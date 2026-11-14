use super::{Descriptors, Section};
use crate::{ELEMENTARY_PID_RANGE, ParseLimits, SourceRange, limit};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    pub table_id: u8,
    pub extension: u16,
    pub version: u8,
    pub current_next: bool,
    pub section_number: u8,
    pub last_section_number: u8,
}

fn header(section: &Section, id: u8) -> Result<SectionHeader> {
    let data = section.bytes().as_ref();
    if data.len() < 12 || data[0] != id || section.crc_remainder() != Some(0) {
        return Err(Error::Invalid("PSI table identity/CRC"));
    }
    if data[1] & 0xf0 != 0xb0 || data[5] & 0xc0 != 0xc0 || data[6] > data[7] {
        return Err(Error::Invalid("PSI syntax/reserved/section number"));
    }
    Ok(SectionHeader {
        table_id: id,
        extension: u16::from_be_bytes([data[3], data[4]]),
        version: (data[5] >> 1) & 31,
        current_next: data[5] & 1 != 0,
        section_number: data[6],
        last_section_number: data[7],
    })
}

#[derive(Debug)]
pub enum TableStep<T> {
    Output(T),
    Yield,
    Done,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramAssociation {
    pub program_number: u16,
    pub pid: u16,
}
impl ProgramAssociation {
    pub fn is_network_pid(self) -> bool {
        self.program_number == 0
    }
}

#[derive(Debug)]
pub struct PatSection {
    pub header: SectionHeader,
    pub source: Bytes,
    pub programs: BudgetVec<ProgramAssociation>,
    pub source_ranges: BudgetVec<SourceRange>,
}

#[derive(Debug)]
pub struct PatReader {
    budget: Budget,
    source: Bytes,
    header: SectionHeader,
    position: usize,
    programs: BudgetVec<ProgramAssociation>,
    source_ranges: BudgetVec<SourceRange>,
    done: bool,
}

impl PatReader {
    pub fn new(section: &Section, budget: &Budget, limits: ParseLimits) -> Result<Self> {
        limits.validate()?;
        if !section.bytes().belongs_to(budget) {
            return Err(Error::Invalid("foreign PAT byte pool"));
        }
        let header = header(section, 0)?;
        let length = section.bytes().len();
        if !(length - 12).is_multiple_of(4) {
            return Err(Error::Invalid("PAT entry alignment"));
        }
        let count = (length - 12) / 4;
        if count > limits.programs {
            return Err(limit(count, limits.programs));
        }
        let mut source_ranges = BudgetVec::new(budget);
        source_ranges.try_extend_from_slice(section.source_ranges())?;
        Ok(Self {
            budget: budget.clone(),
            source: section.bytes().clone(),
            header,
            position: 8,
            programs: BudgetVec::new(budget),
            source_ranges,
            done: false,
        })
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<PatSection>> {
        if self.done {
            return Ok(TableStep::Done);
        }
        while self.position < self.source.len() - 4 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            let data = &self.source.as_ref()[self.position..self.position + 4];
            let program_number = u16::from_be_bytes([data[0], data[1]]);
            let pid = u16::from_be_bytes([data[2], data[3]]);
            if pid & 0xe000 != 0xe000 || !(0x10..=0x1ffe).contains(&(pid & 0x1fff)) {
                return Err(Error::Invalid("PAT PID/reserved bits"));
            }
            // At most the explicitly bounded number of programmes; no hash map.
            if self
                .programs
                .as_slice()
                .iter()
                .any(|entry| entry.program_number == program_number)
            {
                return Err(Error::Invalid("duplicate PAT program"));
            }
            self.programs
                .try_push(ProgramAssociation {
                    program_number,
                    pid: pid & 0x1fff,
                })
                .map_err(|error| error.error)?;
            self.position += 4;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.done = true;
        Ok(TableStep::Output(PatSection {
            header: self.header,
            source: self.source.clone(),
            programs: std::mem::replace(&mut self.programs, BudgetVec::new(&self.budget)),
            source_ranges: std::mem::replace(&mut self.source_ranges, BudgetVec::new(&self.budget)),
        }))
    }
}

#[derive(Clone, Debug)]
pub struct ElementaryStream {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptor_bytes: Bytes,
    pub(super) namespace: super::DescriptorNamespace,
    pub(super) dvb: super::dvb::DvbSummary,
}
impl ElementaryStream {
    pub fn descriptors(&self) -> Descriptors<'_> {
        Descriptors::new(self.descriptor_bytes.as_ref())
    }
    /// An entry read outside [`PmtReader`], without a descriptor namespace.
    pub(crate) fn unchecked(stream_type: u8, pid: u16, descriptor_bytes: Bytes) -> Self {
        Self {
            stream_type,
            pid,
            descriptor_bytes,
            namespace: super::DescriptorNamespace::Unspecified,
            dvb: super::dvb::DvbSummary::default(),
        }
    }
}

#[derive(Debug)]
pub struct ProgramMap {
    pub header: SectionHeader,
    pub source: Bytes,
    pub pcr_pid: u16,
    pub program_descriptors: Bytes,
    /// Caller-supplied interpretation, distinct from raw on-wire facts.
    pub descriptor_namespace: super::DescriptorNamespace,
    pub streams: BudgetVec<ElementaryStream>,
    pub source_ranges: BudgetVec<SourceRange>,
}

#[derive(Debug)]
pub struct PmtReader {
    budget: Budget,
    limits: ParseLimits,
    source: Bytes,
    header: SectionHeader,
    pcr_pid: u16,
    program_descriptors: Bytes,
    position: usize,
    descriptor_end: usize,
    descriptor_start: usize,
    namespace: super::DescriptorNamespace,
    pending: Option<ElementaryStream>,
    program_phase: bool,
    streams: BudgetVec<ElementaryStream>,
    source_ranges: BudgetVec<SourceRange>,
    descriptor_count: usize,
    seen: [u64; 128],
    done: bool,
}

impl PmtReader {
    pub fn new(section: &Section, budget: &Budget, limits: ParseLimits) -> Result<Self> {
        Self::new_with_namespace(
            section,
            budget,
            limits,
            super::DescriptorNamespace::Unspecified,
        )
    }
    /// Context is immutable for this parse; a private tag alone does not select DVB.
    pub fn new_with_namespace(
        section: &Section,
        budget: &Budget,
        limits: ParseLimits,
        namespace: super::DescriptorNamespace,
    ) -> Result<Self> {
        limits.validate()?;
        if !section.bytes().belongs_to(budget) {
            return Err(Error::Invalid("foreign PMT byte pool"));
        }
        let header = header(section, 2)?;
        let source = section.bytes().clone();
        let data = source.as_ref();
        if data.len() < 16
            || header.extension == 0
            || header.section_number != 0
            || header.last_section_number != 0
        {
            return Err(Error::Invalid("PMT single-section program identity"));
        }
        let pcr = u16::from_be_bytes([data[8], data[9]]);
        let length = u16::from_be_bytes([data[10], data[11]]);
        if pcr & 0xe000 != 0xe000 || length & 0xfc00 != 0xf000 {
            return Err(Error::Invalid("PMT reserved bits"));
        }
        let end = 12 + usize::from(length & 0x03ff);
        if end > data.len() - 4 {
            return Err(Error::Invalid("PMT program-info length"));
        }
        let mut source_ranges = BudgetVec::new(budget);
        source_ranges.try_extend_from_slice(section.source_ranges())?;
        Ok(Self {
            budget: budget.clone(),
            limits,
            header,
            pcr_pid: pcr & 0x1fff,
            program_descriptors: source.slice(12..end)?,
            source,
            position: 12,
            descriptor_end: end,
            descriptor_start: 12,
            namespace,
            pending: None,
            program_phase: true,
            streams: BudgetVec::new(budget),
            source_ranges,
            descriptor_count: 0,
            seen: [0; 128],
            done: false,
        })
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<TableStep<ProgramMap>> {
        if self.done {
            return Ok(TableStep::Done);
        }
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.position < self.descriptor_end {
                if !work.try_charge(1) {
                    return Ok(TableStep::Yield);
                }
                if self.descriptor_count == self.limits.descriptors {
                    return Err(limit(
                        self.descriptor_count.saturating_add(1),
                        self.limits.descriptors,
                    ));
                }
                let descriptor =
                    Descriptors::new(&self.source.as_ref()[self.position..self.descriptor_end])
                        .next()
                        .ok_or(Error::Invalid("descriptor disappeared"))??;
                descriptor.validate_selected_syntax()?;
                if matches!(self.namespace, super::DescriptorNamespace::Dvb(_)) {
                    // Each TLV is at most 257 bytes. Service counting is O(1);
                    // no uncharged second descriptor walk or metadata allocation.
                    descriptor.validate_dvb_audio_syntax()?;
                    if descriptor.tag == 0x59 {
                        descriptor.dvb_subtitling()?;
                    }
                    if let Some(stream) = self.pending.as_mut() {
                        stream
                            .dvb
                            .observe(descriptor, self.position - self.descriptor_start)?;
                    }
                }
                if descriptor.tag == 0x38
                    && (self.program_phase
                        || !self
                            .pending
                            .as_ref()
                            .is_some_and(|stream| matches!(stream.stream_type, 0x24 | 0x25 | 0x31)))
                {
                    return Err(Error::Invalid("HEVC descriptor stream type"));
                }
                self.position += descriptor.source.len();
                self.descriptor_count += 1;
                continue;
            }
            if self.program_phase {
                self.program_phase = false;
            }
            if let Some(stream) = self.pending.as_ref() {
                self.streams.try_reserve_exact(1)?;
                let slot = usize::from(stream.pid);
                self.seen[slot / 64] |= 1_u64 << (slot % 64);
                let stream = self
                    .pending
                    .take()
                    .ok_or(Error::Invalid("missing pending stream"))?;
                self.streams.try_push(stream).map_err(|error| error.error)?;
            }
            if self.position == self.source.len() - 4 {
                self.done = true;
                return Ok(TableStep::Output(ProgramMap {
                    header: self.header,
                    source: self.source.clone(),
                    pcr_pid: self.pcr_pid,
                    program_descriptors: self.program_descriptors.clone(),
                    descriptor_namespace: self.namespace,
                    streams: std::mem::replace(&mut self.streams, BudgetVec::new(&self.budget)),
                    source_ranges: std::mem::replace(
                        &mut self.source_ranges,
                        BudgetVec::new(&self.budget),
                    ),
                }));
            }
            if !work.try_charge(1) {
                return Ok(TableStep::Yield);
            }
            if self.streams.len() == self.limits.streams {
                return Err(limit(
                    self.streams.len().saturating_add(1),
                    self.limits.streams,
                ));
            }
            let end = self.source.len() - 4;
            if self.position + 5 > end {
                return Err(Error::Invalid("PMT elementary header"));
            }
            let data = &self.source.as_ref()[self.position..];
            let pid = u16::from_be_bytes([data[1], data[2]]);
            let length = u16::from_be_bytes([data[3], data[4]]);
            if pid & 0xe000 != 0xe000
                || length & 0xfc00 != 0xf000
                || !ELEMENTARY_PID_RANGE.contains(&(pid & 0x1fff))
            {
                return Err(Error::Invalid("PMT elementary PID/reserved bits"));
            }
            let pid = pid & 0x1fff;
            if self.seen[usize::from(pid) / 64] & (1_u64 << (pid % 64)) != 0 {
                return Err(Error::Invalid("duplicate PMT elementary PID"));
            }
            let info_start = self.position + 5;
            let info_end = info_start + usize::from(length & 0x03ff);
            if info_end > end {
                return Err(Error::Invalid("PMT elementary-info length"));
            }
            self.pending = Some(ElementaryStream {
                stream_type: data[0],
                pid,
                descriptor_bytes: self.source.slice(info_start..info_end)?,
                namespace: self.namespace,
                dvb: super::dvb::DvbSummary::default(),
            });
            self.position = info_start;
            self.descriptor_end = info_end;
            self.descriptor_start = info_start;
        }
    }
}
