//! The packets FFmpeg's demuxers hand its `dvdsub` decoder: VobSub
//! `.idx`/`.sub` pairs, program streams and the MP4 palette configuration.
use super::program_stream::{PADDING_STREAM, PRIVATE_STREAM_1, PRIVATE_STREAM_2, Pes, Unit, Units};
use super::vobsub::{Line, Lines};
use triergon_core::{Budget, BudgetVec, Error, Result};
use triergon_pixel::{ChannelRounding, YcbcrMatrix};

/// One subpicture unit an index names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VobSubEntry {
    /// The packet time: the timestamp plus the last delay before it.
    pub milliseconds: i64,
    /// Where the unit's first packet starts in the `.sub` file.
    pub filepos: u64,
    /// Where the next unit of the same stream starts, which ends this one;
    /// `None` for the last.
    pub end: Option<u64>,
    /// The subpicture stream number (the track index).
    pub substream: u8,
}

/// A VobSub index as FFmpeg's `vobsub` demuxer reads it.
///
/// * The configuration is every line before the first `id:` line except
///   blank lines, comments and `langidx:` and `delay:` lines, each ended by LF.
/// * A stream is created by the first timestamp of a track index, so streams
///   are numbered in that order and tracks without timestamps have none.
///   Repeated `id:` lines for one index add to the same stream.
/// * A delay applies to every later timestamp, across tracks, until the next.
/// * A stream's units are ordered by file position, then by time.
#[derive(Debug)]
pub struct VobSubIndex {
    configuration: BudgetVec<u8>,
    streams: BudgetVec<u8>,
    entries: BudgetVec<(usize, VobSubEntry)>,
}

impl VobSubIndex {
    /// Read an index. A malformed `id:`, `delay:` or `timestamp:` line fails
    /// the whole index, as it fails the reference's demuxer.
    pub fn parse(budget: &Budget, text: &[u8]) -> Result<Self> {
        let mut index = Self {
            configuration: BudgetVec::new(budget),
            streams: BudgetVec::new(budget),
            entries: BudgetVec::new(budget),
        };
        let mut track = None;
        let mut seen_track = false;
        let mut delay = 0_i64;
        for (raw, line) in Lines::new(text) {
            match line? {
                Line::Setting(_) if !seen_track => {
                    index.configuration.try_extend_from_slice(raw)?;
                    index.configuration.try_extend_from_slice(b"\n")?;
                }
                Line::Track { index: number, .. } => {
                    seen_track = true;
                    track = Some(number.unwrap_or(0));
                }
                Line::Delay(milliseconds) => delay = milliseconds,
                Line::Timestamp {
                    milliseconds,
                    filepos,
                } => {
                    let Some(substream) = track else {
                        continue;
                    };
                    let known = index
                        .streams
                        .as_slice()
                        .iter()
                        .position(|&s| s == substream);
                    let stream = if let Some(stream) = known {
                        stream
                    } else {
                        index.streams.try_push(substream).map_err(|e| e.error)?;
                        index.streams.len() - 1
                    };
                    let entry = VobSubEntry {
                        milliseconds: milliseconds.checked_add(delay).ok_or(Error::Overflow)?,
                        filepos,
                        end: None,
                        substream,
                    };
                    index
                        .entries
                        .try_push((stream, entry))
                        .map_err(|e| e.error)?;
                }
                _ => {}
            }
        }
        let entries = index.entries.as_mut_slice();
        entries.sort_unstable_by_key(|(stream, e)| (*stream, e.filepos, e.milliseconds));
        for i in 1..entries.len() {
            if entries[i - 1].0 == entries[i].0 {
                entries[i - 1].1.end = Some(entries[i].1.filepos);
            }
        }
        Ok(index)
    }

    /// The text the reference hands its decoder as configuration.
    pub fn configuration(&self) -> &[u8] {
        self.configuration.as_slice()
    }

    /// The track index of each stream, in stream order.
    pub fn streams(&self) -> &[u8] {
        self.streams.as_slice()
    }

    /// The units of stream `stream`, in the order the reference reads them.
    pub fn entries(&self, stream: usize) -> impl Iterator<Item = VobSubEntry> + '_ {
        self.entries
            .as_slice()
            .iter()
            .filter(move |(s, _)| *s == stream)
            .map(|(_, entry)| *entry)
    }
}

impl VobSubEntry {
    /// Gather the unit's packet into `out` from `sub`, the `.sub` bytes from
    /// [`filepos`](Self::filepos) on. Returns `false` when not even one packet
    /// can be read there, in which case the reference delivers nothing.
    ///
    /// Reading follows FFmpeg: PES packets are taken in order until one would
    /// end beyond [`end`](Self::end) or belongs to another subpicture stream.
    /// Padding, navigation and empty packets are skipped; the data of any other
    /// stream's packet is appended as if it were the subpicture's.
    pub fn read(&self, sub: &[u8], out: &mut BudgetVec<u8>) -> Result<bool> {
        out.clear();
        let limit = self.end.map(|end| end.saturating_sub(self.filepos));
        let mut read = false;
        for unit in Units::new(sub) {
            let Unit::Pes(pes) = unit else {
                continue;
            };
            let (substream, data) = match pes.subpicture() {
                Some((substream, data)) => (Some(substream), data),
                None if pes.stream_id == PRIVATE_STREAM_1 => {
                    (None, pes.payload.get(1..).unwrap_or_default())
                }
                None => (None, pes.payload),
            };
            if data.is_empty() || matches!(pes.stream_id, PADDING_STREAM | PRIVATE_STREAM_2) {
                continue;
            }
            read = true;
            let beyond = limit.is_some_and(|limit| pes.span.end as u64 > limit);
            if beyond || substream.is_some_and(|n| n != self.substream) {
                break;
            }
            out.try_extend_from_slice(data)?;
        }
        Ok(read)
    }
}

/// The subpicture streams of a program stream in order of first appearance:
/// FFmpeg's `mpegps` stream order among DVD subtitle streams.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubpictureStreams {
    order: [u8; 32],
    count: usize,
}

impl SubpictureStreams {
    /// Take note of one PES packet.
    pub fn note(&mut self, pes: &Pes<'_>) {
        if let Some((number, _)) = pes.subpicture()
            && !self.as_slice().contains(&number)
        {
            self.order[self.count] = number;
            self.count += 1;
        }
    }

    /// The stream numbers seen, in order.
    pub fn as_slice(&self) -> &[u8] {
        &self.order[..self.count]
    }
}

/// A packet out of [`SubpictureParser`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedUnit<'a> {
    /// The time stamp of the first PES packet since the previous unit.
    pub pts: Option<u64>,
    pub data: &'a [u8],
}

/// FFmpeg's DVD subtitle parser, which joins the PES packets of one
/// subpicture stream into units for the decoder.
///
/// A unit starts with its 16-bit size (or, when that is zero, the 32-bit size
/// after it) and collects packets until it holds that many bytes. A packet that
/// would overfill it discards the unit; the next packet starts a new one. A
/// packet too short for the size word is passed on as it is. Empty packets are
/// ignored, and an unfinished unit at the end of the stream is dropped.
#[derive(Debug)]
pub struct SubpictureParser {
    unit: BudgetVec<u8>,
    expected: usize,
    /// Whether a packet arrived since the last unit, and its time stamp.
    started: bool,
    pts: Option<u64>,
}

impl SubpictureParser {
    pub fn new(budget: &Budget) -> Self {
        Self {
            unit: BudgetVec::new(budget),
            expected: 0,
            started: false,
            pts: None,
        }
    }

    /// Add one PES packet's subpicture data.
    pub fn push<'a>(
        &'a mut self,
        data: &'a [u8],
        pts: Option<u64>,
    ) -> Result<Option<ParsedUnit<'a>>> {
        if data.is_empty() {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            self.pts = pts;
        }
        let first_pts = self.pts;
        if self.expected == 0 {
            self.expected = match *data {
                [0, 0, a, b, c, d, ..] => u32::from_be_bytes([a, b, c, d]) as usize,
                [a, b, ..] if [a, b] != [0, 0] => usize::from(u16::from_be_bytes([a, b])),
                _ => {
                    self.started = false;
                    return Ok(Some(ParsedUnit {
                        pts: first_pts,
                        data,
                    }));
                }
            };
            self.unit.clear();
        }
        if self.unit.len() + data.len() > self.expected {
            self.expected = 0;
            self.unit.clear();
            return Ok(None);
        }
        self.unit.try_extend_from_slice(data)?;
        if self.unit.len() < self.expected {
            return Ok(None);
        }
        self.expected = 0;
        self.started = false;
        Ok(Some(ParsedUnit {
            pts: first_pts,
            data: self.unit.as_slice(),
        }))
    }
}

/// The configuration FFmpeg's MP4 demuxer makes from the decoder-specific
/// information of an `mp4s` DVD subtitle track: its sixteen `(0, Y, Cr, Cb)`
/// colour-table entries as a `palette:` line. `None` without sixteen entries.
pub fn mp4_configuration(specific: &[u8]) -> Option<[u8; 136]> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let entries = specific.get(..64)?;
    let mut text = [b' '; 136];
    text[..9].copy_from_slice(b"palette: ");
    for (i, entry) in entries.chunks_exact(4).enumerate() {
        let rgb = YcbcrMatrix::BT601.rgb(entry[1], entry[2], entry[3], ChannelRounding::HalfDown);
        let at = 9 + i * 8;
        for (k, shift) in [20, 16, 12, 8, 4, 0].into_iter().enumerate() {
            text[at + k] = HEX[(rgb >> shift & 15) as usize];
        }
        if i < 15 {
            text[at + 6] = b',';
        }
    }
    text[135] = b'\n';
    Some(text)
}
