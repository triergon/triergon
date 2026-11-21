//! A document's packets: the cues of
//! [`compat::document::Reader`](crate::compat::document::Reader), with the
//! format, stream, positions and return codes FFmpeg's demuxer API reports.
//!
//! The file name selects WebVTT when it ends in `.vtt`, in any case, and so
//! does a document whose first line, after a UTF-8 byte-order mark, starts
//! with `WEBVTT`; anything else reads as SubRip. A SubRip document fails to
//! open unless its first line that is not empty is a number of at most 20
//! digits and the line after it, if there is one, a timing line; here lines
//! end at LF, less a CR before it. A packet's position is where its SubRip
//! timing line starts, one byte beyond the line's start in the bytes of a
//! UTF-16 document, and for WebVTT the end of the line after its block, the
//! empty one that ends it, or the end of the document.

use super::{Discovery, FormatFacts, Limits, Packet, Progress, ProgressKind, SideData, Stream};
use crate::compat::document::{
    Cue, Limits as ReaderLimits, Reader, Status, Syntax, TimestampFieldWidth,
};
use crate::text::demuxer::subrip_timing;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

/// The return codes FFmpeg's API gives, as recorded: invalid data, end of
/// file.
const INVALID_DATA: i32 = -1_094_995_529;
const END_OF_FILE: i32 = -541_478_725;

/// Bytes of a UTF-16 byte-order mark.
const UTF16_MARK: usize = 2;

/// Everything a document operation reports besides its input.
#[derive(Debug)]
pub(super) struct Parts {
    pub(super) format: Option<FormatFacts>,
    pub(super) stream: Option<Stream>,
    pub(super) packets: BudgetVec<Packet>,
    pub(super) progress: BudgetVec<Progress>,
    pub(super) error_code: Option<i32>,
}

/// A document being read into packets.
#[derive(Debug)]
pub(super) struct Document {
    pool: Budget,
    syntax: Syntax,
    discovery: Discovery,
    reader: Reader,
    /// A cue read but not yet a packet, kept when making it failed.
    cue: Option<Cue>,
    packets: BudgetVec<Packet>,
}

impl Document {
    pub(super) fn new(
        pool: &Budget,
        limits: Limits,
        discovery: Discovery,
        file_name: &[u8],
        payload: &Bytes,
    ) -> Result<Self> {
        let syntax = syntax(file_name, payload.as_ref());
        let limits = ReaderLimits {
            document_bytes: limits.total_packet_bytes,
            cues: limits.packets,
            text_bytes: limits.total_packet_bytes,
        };
        Ok(Self {
            pool: pool.clone(),
            syntax,
            discovery,
            reader: Reader::new(pool, payload.clone(), syntax, limits)?,
            cue: None,
            packets: BudgetVec::new(pool),
        })
    }

    /// Reads `payload`, the document this was made for, until `work` runs out
    /// (`false`) or every cue is a packet (`true`).
    pub(super) fn advance(
        &mut self,
        payload: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            let cue = match self.cue.take() {
                Some(cue) => cue,
                None => match self.reader.poll(work, cancel)? {
                    Status::Yield => return Ok(false),
                    Status::Finished => return Ok(true),
                    Status::Cue(cue) => cue,
                },
            };
            if let Err(error) = self.push(&cue, payload) {
                self.cue = Some(cue);
                return Err(error);
            }
        }
    }

    fn push(&mut self, cue: &Cue, payload: &[u8]) -> Result<()> {
        let mut side_data = BudgetVec::new(&self.pool);
        for (side_data_type, bytes) in [(16, &cue.identifier), (17, &cue.settings)] {
            if let Some(bytes) = bytes {
                side_data
                    .try_push(SideData {
                        side_data_type,
                        bytes: bytes.clone(),
                    })
                    .map_err(|error| error.error)?;
            }
        }
        let position = match self.syntax {
            Syntax::SubRip => self.subrip_position(cue.offset),
            Syntax::WebVtt => webvtt_position(payload, cue.offset),
        };
        let packet = Packet {
            ordinal: u32::try_from(self.packets.len()).map_err(|_| Error::Overflow)?,
            stream: 0,
            data: cue.text.clone(),
            position_raw: i64::try_from(position).map_err(|_| Error::Overflow)?,
            flags: 1,
            buffer_is_null: false,
            pts_raw: cue.start_ms,
            dts_raw: cue.start_ms,
            duration_raw: cue.duration_ms,
            packet_time_base: [0, 1],
            stream_time_base: [1, 1000],
            side_data,
        };
        self.packets.try_push(packet).map_err(|error| error.error)
    }

    /// The position of a SubRip cue whose timing line starts at `offset` in
    /// the text the reader read.
    fn subrip_position(&self, offset: usize) -> usize {
        let Some(line_ends) = self.reader.line_ends() else {
            return offset;
        };
        let line = line_ends.partition_point(|&(converted, _)| converted <= offset);
        let start = line
            .checked_sub(1)
            .map_or(UTF16_MARK, |before| line_ends[before].1);
        start + 1
    }

    /// The operation's outcome, once [`Self::advance`] has returned `true`.
    pub(super) fn finish(&mut self) -> Result<Parts> {
        let mut progress = BudgetVec::new(&self.pool);
        let opens = self.syntax == Syntax::WebVtt || subrip_opens(self.reader.source().as_ref());
        if !opens {
            progress
                .try_push(Progress {
                    kind: ProgressKind::FormatOpen,
                    ordinal: None,
                    returned: Some(INVALID_DATA),
                })
                .map_err(|error| error.error)?;
            return Ok(Parts {
                format: None,
                stream: None,
                packets: BudgetVec::new(&self.pool),
                progress,
                error_code: Some(INVALID_DATA),
            });
        }
        let reads = self.packets.len();
        progress.try_reserve_exact(reads + 3)?;
        let discovered = (self.discovery == Discovery::StreamInfo).then_some(0);
        let calls = [
            (ProgressKind::FormatOpen, None, Some(0)),
            (ProgressKind::Discovery, None, discovered),
        ]
        .into_iter()
        .chain((0..=reads).map(|read| {
            let returned = if read == reads { END_OF_FILE } else { 0 };
            (ProgressKind::ReadFrame, Some(read), Some(returned))
        }));
        for (kind, ordinal, returned) in calls {
            let ordinal = ordinal
                .map(u32::try_from)
                .transpose()
                .map_err(|_| Error::Overflow)?;
            progress
                .try_push(Progress {
                    kind,
                    ordinal,
                    returned,
                })
                .map_err(|error| error.error)?;
        }
        let (name, codec_id, codec_name) = match self.syntax {
            Syntax::SubRip => ("srt", 94225, "subrip"),
            Syntax::WebVtt => ("webvtt", 94226, "webvtt"),
        };
        Ok(Parts {
            format: Some(FormatFacts {
                name,
                probe_size: 5_000_000,
                max_analyze_duration: 0,
            }),
            stream: Some(Stream {
                index: 0,
                codec_type: 3,
                codec_id,
                codec_name,
                codec_tag: 0,
                time_base: [1, 1000],
                start_raw: i64::MIN,
                duration_raw: i64::MIN,
                disposition: 0,
                extradata: None,
                coded_side_data: BudgetVec::new(&self.pool),
                metadata: BudgetVec::new(&self.pool),
            }),
            packets: std::mem::replace(&mut self.packets, BudgetVec::new(&self.pool)),
            progress,
            error_code: None,
        })
    }
}

const UTF8_MARK: &[u8] = b"\xef\xbb\xbf";

/// The syntax FFmpeg's probing selects for a document.
fn syntax(file_name: &[u8], document: &[u8]) -> Syntax {
    let named = file_name
        .len()
        .checked_sub(4)
        .is_some_and(|at| file_name[at..].eq_ignore_ascii_case(b".vtt"));
    let text = document.strip_prefix(UTF8_MARK).unwrap_or(document);
    if named || text.starts_with(b"WEBVTT") {
        Syntax::WebVtt
    } else {
        Syntax::SubRip
    }
}

/// Whether a SubRip document opens; `text` is what the reader read.
fn subrip_opens(text: &[u8]) -> bool {
    let text = text.strip_prefix(UTF8_MARK).unwrap_or(text);
    // A last LF ends the last line; no empty line follows it.
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    let mut lines = text
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let Some(number) = lines.find(|line| !line.is_empty()) else {
        return true;
    };
    number.len() <= 20
        && number.iter().all(u8::is_ascii_digit)
        && lines
            .next()
            .is_none_or(|timing| subrip_timing(timing, TimestampFieldWidth::Bits32).is_some())
}

/// The position of a WebVTT cue whose block ends at `end`: after the second
/// LF of the line breaks there, or where they end.
fn webvtt_position(document: &[u8], end: usize) -> usize {
    let breaks = document[end..]
        .iter()
        .position(|&byte| byte != b'\r' && byte != b'\n')
        .map_or(document.len(), |length| end + length);
    (end..breaks)
        .filter(|&at| document[at] == b'\n')
        .nth(1)
        .map_or(breaks, |at| at + 1)
}
