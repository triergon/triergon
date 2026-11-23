//! Program, codec and descriptor interpretation for the compatible demux view.
//!
//! [`ProgramReader`] reads the PAT and PMT sections the demuxer accepts and
//! reports each [`Program`] with its elementary streams. [`codec`] is the
//! codec the demuxer opens for a PMT entry, [`is_video`] whether it opens a
//! video stream, and [`configuration`] the decoder configuration it derives
//! from the entry's DVB subtitling or teletext descriptor. The standard
//! readings are [`TransportStream`] and [`ElementaryStream::codec`].
//!
//! [`TransportStream`]: crate::stream::TransportStream
//! [`ElementaryStream::codec`]: crate::psi::ElementaryStream::codec
use crate::psi::{
    Descriptor, ElementaryStream, Section, SectionAssembler, descriptor_tag, fields, stream_type,
};
use crate::transport::{TransportPacket, pid};
use crate::{AssemblyStep, ParseLimits, limit};
use std::sync::atomic::AtomicBool;
use triergon_core::container::CodecId;
use triergon_core::{Budget, BudgetVec, Error, Result, SourceId, WorkBudget};

/// One PMT the demuxer accepted.
#[derive(Debug)]
pub struct Program {
    /// The 16-bit program number in the PMT table extension.
    pub number: u16,
    /// `None` when the PMT names PID 0x1fff (no PCR).
    pub pcr_pid: Option<u16>,
    /// The PMT's elementary stream entries, in section order.
    pub streams: BudgetVec<ElementaryStream>,
}

/// Reads the PAT and the PMTs it names from a program's transport packets, as
/// the demuxer does.
///
/// A section is accepted when its CRC is valid and it is current; reserved
/// bits are not checked, and a damaged section is dropped up to the next unit
/// start. Every program of the latest PAT is read, and a PMT is reported when
/// its section differs from the last one accepted for that program.
#[derive(Debug)]
pub struct ProgramReader {
    budget: Budget,
    limits: ParseLimits,
    pat: SectionAssembler,
    /// The CRC of the last PAT section accepted.
    pat_last: Option<[u8; 4]>,
    pmts: BudgetVec<PmtRoute>,
    /// The assembler the last push fed: `None`, the PAT or a PMT index.
    active: Option<Route>,
}

#[derive(Debug)]
struct PmtRoute {
    program: u16,
    pid: u16,
    assembler: SectionAssembler,
    /// The CRC of the last section accepted.
    last: Option<[u8; 4]>,
}

#[derive(Clone, Copy, Debug)]
enum Route {
    Pat,
    Pmt(usize),
}

impl ProgramReader {
    /// A reader of the PAT on PID 0 and the PMTs it names, charging its state to `budget`.
    ///
    /// Fails when `limits` is invalid.
    pub fn new(budget: Budget, limits: ParseLimits) -> Result<Self> {
        Ok(Self {
            pat: SectionAssembler::new(0, budget.clone(), limits)?,
            pat_last: None,
            pmts: BudgetVec::new(&budget),
            budget,
            limits: limits.validate()?,
            active: None,
        })
    }

    /// Feed a carrier on the PAT or a listed PMT PID. Drain [`Self::poll`] before
    /// pushing again, or the pending first carrier makes the next push invalid.
    /// Invalid or truncated table data is discarded. Other assembly errors propagate.
    pub fn push(&mut self, packet: &TransportPacket) -> Result<()> {
        if self.active.is_some() {
            return Err(Error::Invalid("PSI output not taken"));
        }
        let route = if packet.pid == 0 {
            Route::Pat
        } else if let Some(index) = self
            .pmts
            .as_slice()
            .iter()
            .position(|route| route.pid == packet.pid)
        {
            Route::Pmt(index)
        } else {
            return Ok(());
        };
        let Some(payload) = packet.payload.clone() else {
            return Ok(());
        };
        let entity = packet.source_id.unwrap_or(SourceId(0));
        let assembler = self.assembler(route);
        if let Err(rejected) = assembler.push(
            payload,
            packet.payload_unit_start,
            entity,
            packet.payload_source_offset,
        ) {
            assembler.discard();
            if !matches!(rejected.error, Error::Invalid(_) | Error::Truncated) {
                return Err(rejected.error);
            }
            return Ok(());
        }
        self.active = Some(route);
        Ok(())
    }

    /// Return the next accepted PMT from the last carrier, or `None` when drained.
    /// Table scope limits and allocation failures propagate.
    pub fn poll(&mut self) -> Result<Option<Program>> {
        let cancel = AtomicBool::new(false);
        while let Some(route) = self.active {
            let assembler = self.assembler(route);
            match assembler.poll(&mut WorkBudget::new(u64::MAX), &cancel) {
                Ok(AssemblyStep::Output(section)) => {
                    let accepted = section.crc_remainder() == Some(0)
                        && section.bytes().as_ref().get(5).is_some_and(|b| b & 1 != 0);
                    if !accepted {
                        continue;
                    }
                    let program = match route {
                        Route::Pat => {
                            self.read_pat(&section)?;
                            None
                        }
                        Route::Pmt(index) => self.read_pmt(index, &section)?,
                    };
                    if program.is_some() {
                        return Ok(program);
                    }
                }
                Ok(AssemblyStep::Yield) => {}
                Ok(AssemblyStep::NeedPayload | AssemblyStep::Finished) => self.active = None,
                Err(Error::Invalid(_) | Error::Truncated) => {
                    assembler.discard();
                    self.active = None;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn assembler(&mut self, route: Route) -> &mut SectionAssembler {
        match route {
            Route::Pat => &mut self.pat,
            Route::Pmt(index) => &mut self.pmts.as_mut_slice()[index].assembler,
        }
    }

    /// Routes the PMT PIDs of a changed PAT section. A program it keeps keeps
    /// its last PMT; a PMT section in progress is dropped.
    fn read_pat(&mut self, section: &Section) -> Result<()> {
        let data = section.bytes().as_ref();
        if data.len() < 12 || data[0] != 0 {
            return Ok(());
        }
        let crc = data[data.len() - 4..].try_into().ok();
        if self.pat_last == crc {
            return Ok(());
        }
        self.pat_last = crc;
        let mut pmts = BudgetVec::new(&self.budget);
        for entry in data[8..data.len() - 4].chunks_exact(4) {
            let program = u16::from_be_bytes([entry[0], entry[1]]);
            let pid = fields::pid([entry[2], entry[3]]);
            if program == 0 || pid == pid::PAT || pid == pid::NULL {
                continue;
            }
            if pmts.len() == self.limits.programs {
                return Err(limit(pmts.len() + 1, self.limits.programs));
            }
            let last = self
                .pmts
                .as_slice()
                .iter()
                .find(|route| route.program == program && route.pid == pid)
                .and_then(|route| route.last);
            let route = PmtRoute {
                program,
                pid,
                assembler: SectionAssembler::new(pid, self.budget.clone(), self.limits)?,
                last,
            };
            pmts.try_push(route).map_err(|error| error.error)?;
        }
        self.pmts = pmts;
        Ok(())
    }

    /// The program of a changed PMT section for its route.
    fn read_pmt(&mut self, index: usize, section: &Section) -> Result<Option<Program>> {
        let data = section.bytes().as_ref();
        let route = &mut self.pmts.as_mut_slice()[index];
        if data.len() < 16
            || data[0] != 2
            || u16::from_be_bytes([data[3], data[4]]) != route.program
        {
            return Ok(None);
        }
        let crc = data[data.len() - 4..].try_into().ok();
        if route.last == crc {
            return Ok(None);
        }
        route.last = crc;
        let end = data.len() - 4;
        let pcr_pid = fields::pid([data[8], data[9]]);
        let info = fields::length([data[10], data[11]]);
        let mut position = 12 + info;
        let mut streams = BudgetVec::new(&self.budget);
        while position + 5 <= end {
            let entry = &data[position..position + 5];
            let start = position + 5;
            let stop = (start + fields::length([entry[3], entry[4]])).min(end);
            if streams.len() == self.limits.streams {
                return Err(limit(streams.len() + 1, self.limits.streams));
            }
            streams
                .try_push(ElementaryStream::unchecked(
                    entry[0],
                    fields::pid([entry[1], entry[2]]),
                    section.bytes().slice(start..stop)?,
                ))
                .map_err(|error| error.error)?;
            position = stop;
        }
        Ok(Some(Program {
            number: route.program,
            pcr_pid: (pcr_pid != pid::NULL).then_some(pcr_pid),
            streams,
        }))
    }
}

/// The codec the demuxer opens for `stream`: [`ElementaryStream::codec`],
/// the EN 300 468 descriptors on PES private data whatever the namespace
/// ([`ElementaryStream::dvb_descriptor_codec`]), and the user-private stream
/// types 0x81 (AC-3), 0x87 (E-AC-3) and 0x90 (PGS).
pub fn codec(stream: &ElementaryStream) -> Option<CodecId> {
    match stream.stream_type {
        stream_type::ATSC_AC3 => Some(CodecId::Ac3),
        stream_type::ATSC_EAC3 => Some(CodecId::Eac3),
        stream_type::HDMV_PGS => Some(CodecId::Pgs),
        stream_type::PES_PRIVATE_DATA => stream.dvb_descriptor_codec(),
        _ => stream.codec(),
    }
}

/// Whether the demuxer opens `stream` as a video stream: MPEG-1 and MPEG-2
/// video, MPEG-4 visual, AVC, MVC, HEVC, VVC, CAVS, Dirac, AVS2 and VC-1, by
/// stream type.
pub fn is_video(stream: &ElementaryStream) -> bool {
    matches!(
        stream.stream_type,
        stream_type::MPEG1_VIDEO
            | stream_type::MPEG2_VIDEO
            | stream_type::MPEG4_VISUAL
            | stream_type::H264
            | stream_type::H264_MVC
            | stream_type::HEVC
            | stream_type::VVC
            | stream_type::CAVS
            | stream_type::DIRAC
            | stream_type::AVS2
            | stream_type::VC1
    )
}

/// The configuration the demuxer gives the decoder of `stream`, from the first
/// descriptor of its codec's tag: per DVB subtitling service the composition
/// page, the ancillary page (both big endian) and the subtitling type; per
/// teletext page its type and magazine byte and its page number. Empty for
/// other codecs.
pub fn configuration(stream: &ElementaryStream) -> impl Iterator<Item = u8> + '_ {
    let services = first(stream, CodecId::DvbSubtitle, descriptor_tag::DVB_SUBTITLING)
        .flat_map(|descriptor| descriptor.data.chunks_exact(8))
        .flat_map(|service| [service[4], service[5], service[6], service[7], service[3]]);
    let pages = first(stream, CodecId::DvbTeletext, descriptor_tag::DVB_TELETEXT)
        .flat_map(|descriptor| descriptor.data.chunks_exact(5))
        .flat_map(|page| [page[3], page[4]]);
    services.chain(pages)
}

/// The first descriptor tagged `tag` when `stream` carries `wanted`.
fn first(
    stream: &ElementaryStream,
    wanted: CodecId,
    tag: u8,
) -> impl Iterator<Item = Descriptor<'_>> {
    (codec(stream) == Some(wanted))
        .then(|| {
            stream
                .descriptors()
                .map_while(Result::ok)
                .find(|descriptor| descriptor.tag == tag)
        })
        .flatten()
        .into_iter()
}
