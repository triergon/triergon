//! MPEG-TS and M2TS: one elementary stream's packets as the reference's
//! `mpegts` demuxer hands them to the stream's decoder.
//!
//! The file is read in bounded windows and framed by [`TransportFramer`];
//! packets it rejects are skipped. [`ProgramReader`] reads the PAT and PMTs as
//! the demuxer does, and the selected stream is the Nth one the selection
//! counts, in the order the PMTs introduce them, by the demuxer's reading of a
//! PMT entry ([`stream`]). Its transport packets are cut into PES packets by
//! [`PesCutter`] and their timestamps placed by [`DemuxClock`]; a teletext
//! packet takes the PTS the program clock bounds
//! ([`CutPes::pcr_bounded_pts`]), and a DVB subtitle PES data field is cut into
//! codec packets by [`PesParser`]. Only the selected stream's packets stay
//! resident.

use super::{Input, Packet, Result, Selection, Track};
use std::fs::File;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon::{CodecId, TimeBase};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, Step,
    WorkBudget,
};
use triergon_mpegts::compat::pes::{CutPes, DemuxClock, PesCutter};
use triergon_mpegts::compat::stream::{self, Program, ProgramReader};
use triergon_mpegts::psi::ElementaryStream;
use triergon_mpegts::{PacketFormat, ParseLimits, TransportFramer};
use triergon_subtitles::compat::dvb::{PesParser, PesProgress};

/// The largest window handed to the framer at once.
const WINDOW: usize = 64 * 1024;

/// The `ordinal`th stream (from 0) the selection counts in a transport stream
/// of `format` packets, on a 90 kHz clock, with the codec the demuxer opens
/// for it.
pub fn read(
    file: &Path,
    format: PacketFormat,
    selection: Selection<'_>,
    ordinal: usize,
) -> Result<(Track, Option<CodecId<'static>>)> {
    let mut packets = Vec::new();
    let stream = for_each_packet(file, format, selection, ordinal, &mut |_, packet| {
        packets.push(packet);
        Ok(())
    })?;
    let track = Track {
        configuration: stream.configuration,
        time_base: TimeBase::new(1, 90_000)?,
        packets,
    };
    Ok((track, stream.codec))
}

/// A selected stream: the codec the demuxer opens for it and its
/// configuration.
pub struct Stream {
    pub codec: Option<CodecId<'static>>,
    pub configuration: Vec<u8>,
}

/// Hands each packet of the stream [`read`] selects to `packet_sink` as it is cut,
/// with the stream, and returns the stream. Only the packet being cut stays
/// resident.
pub fn for_each_packet(
    file: &Path,
    format: PacketFormat,
    selection: Selection<'_>,
    ordinal: usize,
    packet_sink: &mut dyn FnMut(&Stream, Packet) -> Result<()>,
) -> Result<Stream> {
    let pool = Budget::new(1 << 30)?;
    let mut file = File::open(file)?;
    let length = file.length();
    let capabilities = SourceCapabilities {
        length: Some(length),
        random_access: false,
    };
    let mut input = InputCursor::new(&pool, SourceId(1), capabilities, WINDOW)?;
    let mut framer = TransportFramer::new(pool.clone(), format);
    let mut programs = ProgramReader::new(pool.clone(), ParseLimits::default())?;
    let mut finder = Finder {
        selection,
        ordinal,
        known: BudgetVec::new(&pool),
    };
    let cancel = AtomicBool::new(false);
    let mut window = vec![0; WINDOW];
    let mut selected: Option<Selected> = None;
    loop {
        let packet = match framer.poll(&mut input, &mut WorkBudget::new(1 << 20), &cancel) {
            Ok(Step::Ready(packet)) => packet,
            Ok(Step::NeedInput(request)) if request.offset < length => {
                let size = request.maximum.min(WINDOW);
                let read = file.read_at(request.offset, &mut window[..size])?;
                let bytes = Bytes::copy_from_slice(&pool, &window[..read])?;
                input.provide_owned(InputWindow::new(
                    request.source,
                    request.token,
                    request.offset,
                    bytes,
                )?)?;
                continue;
            }
            Ok(Step::NeedInput(request)) => {
                input.finish(request.source, request.token)?;
                continue;
            }
            Ok(Step::End(_)) => break,
            Ok(_) => continue,
            Err(Error::Invalid(_)) => {
                framer.discard();
                continue;
            }
            Err(Error::Truncated) => break,
            Err(error) => return Err(error.into()),
        };
        programs.push(&packet)?;
        while let Some(program) = programs.poll()? {
            if selected.is_none() {
                selected = finder
                    .find(&program)?
                    .map(|es| Selected::new(es, program.number, &pool))
                    .transpose()?;
            }
            if let Some(selected) = selected.as_mut()
                && selected.program == program.number
            {
                selected.cutter.set_pcr_pid(program.pcr_pid);
            }
        }
        if let Some(selected) = selected.as_mut() {
            selected.cutter.push(&packet)?;
            selected.drain(packet_sink)?;
        }
    }
    let mut selected = selected.ok_or("no such stream")?;
    selected.cutter.finish()?;
    selected.drain(packet_sink)?;
    Ok(selected.stream)
}

/// Counts the streams the PMTs introduce until the selected one.
struct Finder<'a> {
    selection: Selection<'a>,
    ordinal: usize,
    /// The PIDs of the streams seen so far.
    known: BudgetVec<u16>,
}

impl Finder<'_> {
    /// The selected stream, if `program` introduces it.
    fn find<'p>(&mut self, program: &'p Program) -> Result<Option<&'p ElementaryStream>> {
        for es in program.streams.as_slice() {
            if self.known.as_slice().contains(&es.pid) {
                continue;
            }
            self.known.try_push(es.pid).map_err(|e| e.error)?;
            let counted = match self.selection {
                Selection::Codec(codec) => stream::codec(es) == Some(codec),
                Selection::Video => stream::is_video(es),
            };
            if counted {
                if self.ordinal == 0 {
                    return Ok(Some(es));
                }
                self.ordinal -= 1;
            }
        }
        Ok(None)
    }
}

/// The selected stream, its cutter and clock.
struct Selected {
    program: u16,
    stream: Stream,
    cutter: PesCutter,
    clock: DemuxClock,
    /// DVB subtitles only: the data-field parser.
    dvb: Option<PesParser>,
}

impl Selected {
    fn new(es: &ElementaryStream, program: u16, pool: &Budget) -> Result<Self> {
        let codec = stream::codec(es);
        Ok(Self {
            program,
            stream: Stream {
                codec,
                configuration: stream::configuration(es).collect(),
            },
            cutter: PesCutter::new(es.pid, pool.clone(), ParseLimits::default())?,
            clock: DemuxClock::new(program)?,
            dvb: (codec == Some(CodecId::DvbSubtitle)).then(|| PesParser::new(pool)),
        })
    }

    /// Hands the PES packets the cutter completed to `sink`.
    fn drain(&mut self, sink: &mut dyn FnMut(&Stream, Packet) -> Result<()>) -> Result<()> {
        while let Some(pes) = self.cutter.pop() {
            let packet = self.packet(&pes)?;
            sink(&self.stream, packet)?;
        }
        Ok(())
    }

    fn packet(&mut self, pes: &CutPes) -> Result<Packet> {
        let (pts, dts) = if self.stream.codec == Some(CodecId::DvbTeletext) {
            (pes.pcr_bounded_pts(), None)
        } else {
            (pes.pts_raw33, pes.dts_raw33)
        };
        let (pts, dts) = self.clock.place(pts, dts)?;
        let Some(parser) = self.dvb.as_mut() else {
            return Ok(Packet {
                pts: pts.map(i64::try_from).transpose()?,
                dts: dts.map(i64::try_from).transpose()?,
                duration: None,
                data: pes.data.to_vec(),
            });
        };
        parser.provide(&pes.data, pts)?;
        let cancel = AtomicBool::new(false);
        let packet = loop {
            match parser.step(&mut WorkBudget::new(u64::MAX), &cancel)? {
                PesProgress::Packet(packet) => break packet,
                PesProgress::Yield => {}
                PesProgress::NeedPacket => return Err("dvb: PES parser stalled".into()),
            }
        };
        let pts = packet.codec_pts().map(i64::try_from).transpose()?;
        Ok(Packet {
            pts,
            dts: pts,
            duration: None,
            data: packet.codec().to_vec(),
        })
    }
}
