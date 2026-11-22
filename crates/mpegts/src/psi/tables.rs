use super::{Descriptors, Section};
use crate::{ELEMENTARY_PID_RANGE, ParseLimits, SourceRange, limit};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// A PMT section other than section 0 of 0.
pub(crate) const PMT_PROGRAM_IDENTITY: &str = "PMT single-section program identity";

/// The long-form header fields shared by PAT and PMT sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    /// The 8-bit table ID: 0 for a PAT, 2 for a PMT.
    pub table_id: u8,
    /// The 16-bit table ID extension: the transport stream ID of a PAT, the program number of a
    /// PMT.
    pub extension: u16,
    /// The 5-bit version number.
    pub version: u8,
    /// The current/next indicator: `true` for the table in force, `false` for the next one.
    pub current_next: bool,
    /// The 8-bit number of this section.
    pub section_number: u8,
    /// The 8-bit number of the last section of the table; never below `section_number`.
    pub last_section_number: u8,
}

fn header(section: &Section, id: u8) -> Result<SectionHeader> {
    let data = section.bytes().as_ref();
    // Sharing PSI field views added 10 instructions to reordered progressive TS playback.
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

/// One step of incremental table parsing.
#[derive(Debug)]
pub enum TableStep<T> {
    /// The parsed table, returned once.
    Output(T),
    /// The work budget ran out; poll again with more work.
    Yield,
    /// Nothing more to return: the table was already returned, or the input completed no table.
    Done,
}
/// One program loop entry of a PAT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramAssociation {
    /// The 16-bit program number; 0 names the network PID.
    pub program_number: u16,
    /// The 13-bit PID of the program's PMT, or of the network information table for program 0.
    pub pid: u16,
}
impl ProgramAssociation {
    /// Whether this entry names the network PID (program number 0) rather than a PMT.
    pub fn is_network_pid(self) -> bool {
        self.program_number == 0
    }
}

/// One parsed PAT section.
#[derive(Debug)]
pub struct PatSection {
    /// The section header; its extension is the transport stream ID.
    pub header: SectionHeader,
    /// The whole section bytes, CRC included.
    pub source: Bytes,
    /// The program loop entries in section order; program numbers are unique.
    pub programs: BudgetVec<ProgramAssociation>,
    /// Where the section bytes lie in the source, one range per contiguous source run.
    pub source_ranges: BudgetVec<SourceRange>,
}

/// Incremental parser of one PAT section's program loop.
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
    /// A parser of `section`, charging its tables to `budget`.
    ///
    /// Fails with `Invalid` when the section uses another byte pool, is not a long-form PAT section
    /// with a valid CRC and reserved bits, or has a partial program entry, and with `Limit` when it
    /// lists more programs than `limits` allows.
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
    /// Parses program entries within `work` and returns the section once complete, then `Done`.
    ///
    /// Fails with `Invalid` for a PID outside 0x0010..=0x1FFE, unset reserved bits or a duplicate
    /// program number, with `Cancelled` when `cancel` is set, and when the budget refuses the
    /// table.
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

/// One elementary stream entry of a PMT.
#[derive(Clone, Debug)]
pub struct ElementaryStream {
    /// The 8-bit stream type.
    pub stream_type: u8,
    /// The 13-bit elementary PID.
    pub pid: u16,
    /// The entry's descriptor loop bytes; may be empty.
    pub descriptor_bytes: Bytes,
    pub(super) namespace: super::DescriptorNamespace,
    pub(super) dvb: super::dvb::DvbSummary,
}
impl ElementaryStream {
    /// Iterates the entry's descriptors.
    pub fn descriptors(&self) -> Descriptors<'_> {
        Descriptors::new(self.descriptor_bytes.as_ref())
    }
    /// An entry read outside [`PmtReader`], without a descriptor namespace.
    pub(crate) fn unchecked(stream_type: u8, pid: u16, descriptor_bytes: Bytes) -> Self {
        Self::in_namespace(
            stream_type,
            pid,
            descriptor_bytes,
            super::DescriptorNamespace::Unspecified,
        )
    }
    /// An entry read outside [`PmtReader`] under `namespace`, for its codec and kind.
    pub(crate) fn in_namespace(
        stream_type: u8,
        pid: u16,
        descriptor_bytes: Bytes,
        namespace: super::DescriptorNamespace,
    ) -> Self {
        Self {
            stream_type,
            pid,
            descriptor_bytes,
            namespace,
            dvb: super::dvb::DvbSummary::default(),
        }
    }
}

/// One parsed single-section PMT.
#[derive(Debug)]
pub struct ProgramMap {
    /// The section header; its extension is the program number.
    pub header: SectionHeader,
    /// The whole section bytes, CRC included.
    pub source: Bytes,
    /// The 13-bit PID that carries the program's PCR.
    pub pcr_pid: u16,
    /// The program info descriptor loop bytes; may be empty.
    pub program_descriptors: Bytes,
    /// Caller-supplied interpretation, distinct from raw on-wire facts.
    pub descriptor_namespace: super::DescriptorNamespace,
    /// The elementary stream entries in section order; their PIDs are unique.
    pub streams: BudgetVec<ElementaryStream>,
    /// Where the section bytes lie in the source, one range per contiguous source run.
    pub source_ranges: BudgetVec<SourceRange>,
}

#[derive(Debug)]
enum Loop {
    Program,
    Stream(ElementaryStream),
    Between,
}

/// Incremental parser of one PMT section's descriptor and elementary stream loops.
#[derive(Debug)]
#[repr(C)]
pub struct PmtReader {
    descriptor_loop: Loop,
    streams: BudgetVec<ElementaryStream>,
    source_ranges: BudgetVec<SourceRange>,
    header: SectionHeader,
    budget: Budget,
    source: Bytes,
    program_descriptors: Bytes,
    limits: ParseLimits,
    position: usize,
    descriptor_end: usize,
    descriptor_start: usize,
    descriptor_count: usize,
    pcr_pid: u16,
    done: bool,
    namespace: super::DescriptorNamespace,
}

impl PmtReader {
    /// A parser of `section` with no descriptor namespace; see
    /// [`new_with_namespace`](Self::new_with_namespace).
    ///
    /// Fails with `Invalid` when the section uses another byte pool, is not a valid PMT section 0
    /// of 0
    /// for a nonzero program, or has unset reserved bits or an overlong program info length.
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
            return Err(Error::Invalid(PMT_PROGRAM_IDENTITY));
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
            descriptor_loop: Loop::Program,
            streams: BudgetVec::new(budget),
            source_ranges,
            descriptor_count: 0,
            done: false,
        })
    }
    /// Parses descriptors and stream entries within `work` and returns the map once complete, then
    /// `Done`.
    ///
    /// Fails with `Invalid` for malformed entries or descriptors, a reserved or duplicate
    /// elementary PID
    /// or an HEVC descriptor outside an HEVC stream, with `Limit` beyond the stream or descriptor
    /// limits, with `Cancelled` when `cancel` is set, and when the budget refuses the table.
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
                    if let Loop::Stream(stream) = &mut self.descriptor_loop {
                        stream
                            .dvb
                            .observe(descriptor, self.position - self.descriptor_start)?;
                    }
                }
                if descriptor.tag == 0x38
                    && !matches!(
                        &self.descriptor_loop,
                        Loop::Stream(stream) if matches!(stream.stream_type, 0x24 | 0x25 | 0x31)
                    )
                {
                    return Err(Error::Invalid("HEVC descriptor stream type"));
                }
                self.position += descriptor.source.len();
                self.descriptor_count += 1;
                continue;
            }
            match self.descriptor_loop {
                Loop::Program => self.descriptor_loop = Loop::Between,
                Loop::Stream(_) => {
                    self.streams.try_reserve_exact(1)?;
                    let Loop::Stream(stream) =
                        std::mem::replace(&mut self.descriptor_loop, Loop::Between)
                    else {
                        return Err(Error::Invalid("missing pending stream"));
                    };
                    self.streams.try_push(stream).map_err(|error| error.error)?;
                }
                Loop::Between => {}
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
            if self
                .streams
                .as_slice()
                .iter()
                .any(|stream| stream.pid == pid)
            {
                return Err(Error::Invalid("duplicate PMT elementary PID"));
            }
            let info_start = self.position + 5;
            let info_end = info_start + usize::from(length & 0x03ff);
            if info_end > end {
                return Err(Error::Invalid("PMT elementary-info length"));
            }
            self.descriptor_loop = Loop::Stream(ElementaryStream {
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
