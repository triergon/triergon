//! Preparation: one subtitle track's packets in demux order, read through the
//! Triergon container readers before any timed interval.
//!
//! The input is read in bounded windows, from a file or from memory, and only
//! the selected track's packets are kept, so preparation holds little more
//! than those packets. Matroska and ISO BMFF go through [`AnyReader`], MPEG-TS
//! and M2TS through [`ts`].

pub mod ts;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon::isobmff::compat::mov::edits::{EditCursor, EditLimits};
use triergon::isobmff::compat::mov::packets::CursorStatus;
use triergon::{
    AnyReader, Budget, Bytes, CodecId, ContainerReader, Event, InputWindow, MediaKind, PacketInfo,
    SourceCapabilities, SourceId, Step, TimeBase, TrackId, TrackInfo, WorkBudget,
};
use triergon_mpegts::PacketFormat;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One demuxed packet, copied out of the reader.
#[derive(Clone, Debug)]
pub struct Packet {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
    pub data: Vec<u8>,
}

/// One selected track: its configuration, packet clock and packets.
#[derive(Clone, Debug)]
pub struct Track {
    pub configuration: Vec<u8>,
    pub time_base: TimeBase,
    pub packets: Vec<Packet>,
}

/// The largest window handed to a reader at once: small enough that the
/// window and its copy reuse the same heap pages from request to request.
const WINDOW: usize = 64 * 1024;

/// Random-access input: a file or bytes in memory.
pub trait Input {
    fn length(&self) -> u64;
    /// Fill `buffer` from `offset`, or as much of it as the input holds.
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize>;
}

impl<T: AsRef<[u8]> + ?Sized> Input for &T {
    fn length(&self) -> u64 {
        self.as_ref().len() as u64
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        let tail = self
            .as_ref()
            .get(usize::try_from(offset)?..)
            .unwrap_or_default();
        let n = tail.len().min(buffer.len());
        buffer[..n].copy_from_slice(&tail[..n]);
        Ok(n)
    }
}

impl Input for File {
    fn length(&self) -> u64 {
        self.metadata().map_or(0, |m| m.len())
    }
    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        self.seek(SeekFrom::Start(offset))?;
        let mut filled = 0;
        while filled < buffer.len() {
            match self.read(&mut buffer[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(filled)
    }
}

/// The first bytes of `file`, enough to tell its format.
pub fn head(file: &Path) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    File::open(file)?.take(1024).read_to_end(&mut head)?;
    Ok(head)
}

/// The `ordinal`th track (from 0) whose codec is `codec` in a Matroska, ISO
/// BMFF or transport stream file, on the reference demuxer's clock: Matroska
/// packets in ticks of the segment's TimestampScale with the DTS equal to the
/// PTS, ISO BMFF subtitle tracks through their edit list ([`edited_packets`]),
/// transport streams as [`ts`] reads them.
pub fn read_file_track(file: &Path, codec: CodecId<'_>, ordinal: usize) -> Result<Track> {
    let selection = Selection::Codec(codec);
    if let Some(format) = PacketFormat::detect(&head(file)?) {
        return Ok(ts::read(file, format, selection, ordinal)?.0);
    }
    let (track, scale, _) = read(File::open(file)?, selection, ordinal)?;
    match scale {
        Some(scale) => matroska_clock(track, scale),
        None => Ok(track),
    }
}

/// The `ordinal`th video track (from 0, whatever its codec) of a Matroska, ISO
/// BMFF or transport stream file, on the clock [`read_file_track`] reads it
/// with (an ISO BMFF video track's edit list is not applied), with its codec
/// when it is H.264 or HEVC, or in a transport stream MPEG-2 video.
pub fn read_file_video_track(
    file: &Path,
    ordinal: usize,
) -> Result<(Track, Option<CodecId<'static>>)> {
    if let Some(format) = PacketFormat::detect(&head(file)?) {
        return ts::read(file, format, Selection::Video, ordinal);
    }
    let (track, scale, codec) = read(File::open(file)?, Selection::Video, ordinal)?;
    let track = match scale {
        Some(scale) => matroska_clock(track, scale)?,
        None => track,
    };
    Ok((track, codec))
}

/// Which tracks a reader counts.
#[derive(Clone, Copy)]
pub enum Selection<'a> {
    Codec(CodecId<'a>),
    Video,
}

/// The track, for Matroska the segment's TimestampScale, and the track's codec
/// when it is H.264 or HEVC.
fn read(
    mut input: impl Input,
    selection: Selection<'_>,
    ordinal: usize,
) -> Result<(Track, Option<u64>, Option<CodecId<'static>>)> {
    let pool = Budget::new(1 << 30)?;
    let length = input.length();
    let capabilities = SourceCapabilities {
        length: Some(length),
        random_access: true,
    };
    let mut head = [0; 64];
    let n = input.read_at(0, &mut head)?;
    let mut reader = AnyReader::open(&head[..n], &pool, SourceId(1), capabilities)?;
    let cancel = AtomicBool::new(false);
    let mut selected: Option<TrackId> = None;
    let mut packets = Vec::new();
    let mut time_base = None;
    let mut window = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(1 << 20), &cancel)? {
            Step::Ready(Event::Packet(p)) => {
                let id = match selected {
                    Some(id) => id,
                    None => *selected.insert(select(&reader, selection, ordinal)?),
                };
                if p.track() == id {
                    time_base = Some(p.time_base());
                    packets.push(Packet {
                        pts: p.pts(),
                        dts: p.dts(),
                        duration: p.duration(),
                        data: p.payload().to_vec(),
                    });
                }
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(r) => {
                if r.offset >= length {
                    reader.finish(r.source, r.token)?;
                } else {
                    let size =
                        usize::try_from((length - r.offset).min(r.maximum as u64))?.min(WINDOW);
                    window.resize(size, 0);
                    let got = input.read_at(r.offset, &mut window)?;
                    let bytes = Bytes::copy_from_slice(&pool, &window[..got])?;
                    reader.provide(&InputWindow::new(r.source, r.token, r.offset, bytes)?)?;
                }
            }
            Step::Backpressure(p) => return Err(format!("reader backpressure {p:?}").into()),
            Step::End(_) => break,
        }
    }
    let id = match selected {
        Some(id) => id,
        None => select(&reader, selection, ordinal)?,
    };
    let track = (0..reader.track_count())
        .filter_map(|i| reader.track(i))
        .find(|t| t.id() == id)
        .ok_or("no such track")?;
    let configuration = track.configuration().to_vec();
    let codec = match track.codec() {
        CodecId::H264 => Some(CodecId::H264),
        CodecId::Hevc => Some(CodecId::Hevc),
        _ => None,
    };
    let timestamp_scale = match &reader {
        AnyReader::Matroska(m) => Some(m.timestamp_scale_ns()),
        _ => None,
    };
    if let AnyReader::Isobmff(mp4) = &reader
        && track.kind() == MediaKind::Subtitle
        && let Some(edited) = edited_packets(mp4, id, &mut input)?
    {
        let track = Track {
            configuration,
            ..edited
        };
        return Ok((track, None, codec));
    }
    let track = Track {
        configuration,
        time_base: time_base.unwrap_or(TimeBase::NANOSECONDS),
        packets,
    };
    Ok((track, timestamp_scale, codec))
}

/// The id of the `ordinal`th track the selection counts.
fn select(reader: &AnyReader, selection: Selection<'_>, ordinal: usize) -> Result<TrackId> {
    Ok((0..reader.track_count())
        .filter_map(|i| reader.track(i))
        .filter(|t| match selection {
            Selection::Codec(codec) => t.codec() == codec,
            Selection::Video => t.kind() == MediaKind::Video,
        })
        .nth(ordinal)
        .ok_or("no such track")?
        .id())
}

/// The packets of an ISO BMFF track with an edit list, repeated, trimmed and
/// shifted as FFmpeg's MOV demuxer emits them, on the track's media clock;
/// `None` without an edit list.
fn edited_packets(
    reader: &triergon::isobmff::Reader,
    id: TrackId,
    input: &mut impl Input,
) -> Result<Option<Track>> {
    let track = reader
        .tracks()
        .iter()
        .find(|t| t.id() == id)
        .ok_or("no such track")?;
    let Some(edits) = track.edits() else {
        return Ok(None);
    };
    let media = track.header().time_base()?;
    let movie = reader
        .movie_header()
        .ok_or("no movie header")?
        .time_base()?;
    let mut cursor = EditCursor::new(track.table(), edits, media, movie, EditLimits::default())?;
    let mut packets = Vec::new();
    loop {
        match cursor.poll(&mut WorkBudget::new(1 << 20))? {
            CursorStatus::Yield => {}
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let view = cursor.take_occurrence()?.demux;
                let mut data =
                    vec![0; usize::try_from(view.source_range.end - view.source_range.start)?];
                if input.read_at(view.source_range.start, &mut data)? != data.len() {
                    return Err("sample beyond the file".into());
                }
                packets.push(Packet {
                    pts: view.pts,
                    dts: view.dts,
                    duration: Some(view.duration),
                    data,
                });
            }
        }
    }
    Ok(Some(Track {
        configuration: Vec::new(),
        time_base: media,
        packets,
    }))
}

/// Matroska packets on the reference's clock: ticks of the segment's
/// TimestampScale (`scale` nanoseconds) rather than nanoseconds, the DTS equal
/// to the PTS and durations in ticks.
fn matroska_clock(mut track: Track, scale: u64) -> Result<Track> {
    if track.time_base != TimeBase::NANOSECONDS {
        return Err("unexpected Matroska packet clock".into());
    }
    let scale = i64::try_from(scale)?;
    let ticks = |ns: i64| ns.div_euclid(scale);
    for packet in &mut track.packets {
        packet.pts = packet.pts.map(ticks);
        packet.dts = packet.pts;
        packet.duration = packet.duration.map(ticks);
    }
    track.time_base = TimeBase::new(1, 1_000_000_000 / u64::try_from(scale)?)?;
    Ok(track)
}

/// `value` in `base` units as microseconds, rounded to nearest with ties away
/// from zero, as the reference's packet-time rescale does.
pub fn microseconds(value: i64, base: TimeBase) -> Option<i64> {
    let num = i128::from(value) * i128::from(base.numerator()) * 1_000_000;
    let den = i128::from(base.denominator());
    let half = den / 2;
    let q = if num >= 0 {
        (num + half) / den
    } else {
        (num - half) / den
    };
    i64::try_from(q).ok()
}

/// The `sub-input` lines for `track`, byte for byte as the reference prints them.
pub fn write_input(out: &mut impl std::io::Write, track: &Track) -> std::io::Result<()> {
    let hex = triergon_harness_protocol::hex;
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"subtitle-input\",\"time_base\":[{},{}],\"configuration\":\"{}\",\"packets\":{}}}",
        track.time_base.numerator(),
        track.time_base.denominator(),
        hex(&track.configuration),
        track.packets.len()
    )?;
    let value = |v: Option<i64>| v.map_or("null".to_owned(), |v| v.to_string());
    for (i, p) in track.packets.iter().enumerate() {
        writeln!(
            out,
            "{{\"input\":{i},\"pts\":{},\"dts\":{},\"duration\":{},\"data\":\"{}\"}}",
            value(p.pts),
            value(p.dts),
            p.duration.unwrap_or(0),
            hex(&p.data)
        )?;
    }
    Ok(())
}
