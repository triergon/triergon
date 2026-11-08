//! FFmpeg's `libzvbi_teletextdec` decoder in its text and ASS formats.
//!
//! A packet's teletext units go through the [`Assembler`]; the pages they
//! complete build on libzvbi's page memory ([`memory`]), are selected
//! ([`selection`]), presented with libzvbi's choice of X/28 and M/29 packets
//! ([`settings`]), laid out as libzvbi lays out a page ([`grid`]), written as
//! text ([`text`]) or ASS ([`ass`]) by the [`writer`] and queued ([`queue`]).

mod ass;
mod grid;
mod memory;
mod out;
mod queue;
mod selection;
mod settings;
mod text;
mod writer;

pub use selection::PageSelection;

use super::data_unit::{
    DATA_UNIT_BYTES, DataField, DataUnitKind, TeletextUnit, is_teletext_data_identifier,
};
use super::page::{Assembler, Reception};
use memory::{Completed, PageMemory};
use out::Out;
use queue::Queue;
use selection::SubtitlePages;
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use writer::Writer;

/// Teletext data units one packet is decoded from; the rest are ignored.
const UNITS_PER_PACKET: usize = 64;
/// Complete pages kept for pages sent without C4 (erase).
const PAGE_MEMORY: usize = 1024;
/// Work charged per data unit and per page written.
const UNIT_WORK: u64 = 64;
const PAGE_WORK: u64 = 2048;

/// The output format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// `txt_format=text`: the page as plain text in the `Default` style.
    #[default]
    Text,
    /// `txt_format=ass`: the page in the `Teletext` style, or a subtitle
    /// page's boxed text in the `Subtitle` style.
    Ass,
}

/// The decoder options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub format: Format,
    pub pages: PageSelection,
    /// `txt_default_region`: the default character set designation (0 to 87)
    /// combined with each page's national option bits; `None` is designation 16.
    pub default_region: Option<u8>,
    /// `txt_chop_top`: leave out the header row.
    pub chop_top: bool,
    /// `txt_chop_spaces`: trim and drop blank text lines.
    pub chop_spaces: bool,
    /// `txt_duration` in milliseconds; `None` displays until the next subtitle.
    pub duration: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: Format::Text,
            pages: PageSelection::All,
            default_region: None,
            chop_top: true,
            chop_spaces: true,
            duration: None,
        }
    }
}

/// One subtitle: a page, or an empty event that clears the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subtitle<'a> {
    /// Presentation time in microseconds, from the packet that completed the page.
    pub pts: Option<i64>,
    /// `end_display_time` in milliseconds.
    pub end_display: u32,
    /// The ASS event line (`ReadOrder,Layer,Style,...,Text`), absent when empty.
    pub ass: Option<&'a str>,
}

/// Why the decoder refused a packet, as FFmpeg's decoder returns an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The payload is not a whole EN 300 472 PES data field, or a teletext
    /// data unit has the wrong length (`AVERROR_INVALIDDATA`).
    InvalidData,
    /// A page arrived while the output queue was full (`ENOSYS`); it is lost.
    QueueFull,
}

/// The result of one decode call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded<'a> {
    Subtitle(Subtitle<'a>),
    Nothing,
    Refused(Refusal),
}

/// FFmpeg's teletext decoder: PES data fields in, at most one subtitle per call.
///
/// Each call decodes the first 64 teletext units of its packet. Every selected
/// page those units complete is laid out, written and queued (up to 25); the
/// call then returns the oldest queued subtitle. A call that found the queue
/// full returns [`Refusal::QueueFull`] instead.
#[derive(Debug)]
pub struct Decoder {
    selection: PageSelection,
    end_display: u32,
    assembler: Assembler,
    memory: PageMemory,
    subtitle_pages: SubtitlePages,
    /// The pages the current packet completed.
    completed: BudgetVec<Completed>,
    queue: Queue,
    writer: Writer,
}

impl Decoder {
    /// Work one [`decode`](Self::decode) call may need at most.
    pub const MAX_PACKET_WORK: u64 = UNITS_PER_PACKET as u64 * (UNIT_WORK + 8 * PAGE_WORK);

    pub fn new(pool: &Budget, options: Options) -> Result<Self> {
        if options.default_region.is_some_and(|r| r > 87) {
            return Err(Error::Invalid("teletext default region"));
        }
        Ok(Self {
            writer: Writer::new(
                options.format,
                options.default_region.unwrap_or(16),
                options.chop_top,
                options.chop_spaces,
            ),
            selection: options.pages,
            end_display: options.duration.unwrap_or(u32::MAX),
            assembler: Assembler::new(),
            memory: PageMemory::new(pool, PAGE_MEMORY),
            subtitle_pages: SubtitlePages::NONE,
            completed: BudgetVec::new(pool),
            queue: Queue::new(pool)?,
        })
    }

    /// The ASS header of the subtitles.
    #[must_use]
    pub fn header(&self) -> &'static str {
        match self.writer.format {
            Format::Text => TEXT_HEADER,
            Format::Ass => ASS_HEADER,
        }
    }

    /// Decodes one PES payload (an empty one drains) with its presentation
    /// time in microseconds.
    pub fn decode(
        &mut self,
        payload: &[u8],
        pts: Option<i64>,
        work: &mut WorkBudget,
    ) -> Result<Decoded<'_>> {
        if work.remaining() < Self::MAX_PACKET_WORK {
            return Err(Error::Backpressure {
                requested: usize::try_from(Self::MAX_PACKET_WORK).unwrap_or(usize::MAX),
                available: usize::try_from(work.remaining()).unwrap_or(usize::MAX),
            });
        }
        if let Some(field) = DataField::new(payload) {
            // The payload and the 45-byte PES header fill whole transport packets.
            if !(payload.len() + 45).is_multiple_of(184) {
                return Ok(Decoded::Refused(Refusal::InvalidData));
            }
            if is_teletext_data_identifier(field.data_identifier) {
                let units = field.unit_bytes().chunks_exact(DATA_UNIT_BYTES);
                let malformed = units
                    .clone()
                    .any(|unit| DataUnitKind::from_id(unit[0]).is_teletext() && unit[1] != 0x2C);
                if malformed {
                    return Ok(Decoded::Refused(Refusal::InvalidData));
                }
                let teletext = units
                    .filter_map(|unit| TeletextUnit::parse(unit.try_into().ok()?))
                    .take(UNITS_PER_PACKET);
                let Self {
                    assembler,
                    memory,
                    subtitle_pages,
                    completed,
                    ..
                } = self;
                let mut spent = 0;
                for unit in teletext {
                    spent += UNIT_WORK;
                    assembler.push(&unit.packet, |reception| match reception {
                        Reception::Header(header) => {
                            memory.begin(header);
                            subtitle_pages.observe(header);
                            Ok(())
                        }
                        Reception::Presentation { page, magazine } => {
                            memory.presentation_received(page.header.page.magazine, magazine);
                            Ok(())
                        }
                        Reception::Page { page, magazine } => {
                            let completed_page = memory.complete(page.clone(), magazine)?;
                            if !completed_page.page.header.page.is_decimal() {
                                return Ok(());
                            }
                            spent += PAGE_WORK;
                            completed.try_push(completed_page).map_err(|e| e.error)
                        }
                    })?;
                }
                work.take(spent);
            }
        }
        if self.write_completed(pts)? {
            return Ok(Decoded::Refused(Refusal::QueueFull));
        }
        let end_display = self.end_display;
        Ok(match self.queue.pop_front() {
            Some(slot) => Decoded::Subtitle(Subtitle {
                pts: slot.pts,
                end_display,
                ass: std::str::from_utf8(slot.text.as_slice())
                    .ok()
                    .filter(|text| !text.is_empty()),
            }),
            None => Decoded::Nothing,
        })
    }
}

impl Decoder {
    /// Writes and queues the selected pages the packet completed; reports
    /// whether one was lost to a full queue.
    fn write_completed(&mut self, pts: Option<i64>) -> Result<bool> {
        let mut lost = false;
        for Completed {
            page,
            presentations,
        } in self.completed.as_slice()
        {
            let number = page.header.page;
            let subtitle = self.subtitle_pages.contains(number);
            if !self.selection.selects(number, subtitle) {
                continue;
            }
            let Some(slot) = self.queue.back() else {
                lost = true;
                continue;
            };
            self.writer
                .write(page, presentations, subtitle, &mut Out(&mut slot.text))?;
            slot.pts = pts;
            self.queue.push_back();
        }
        self.completed.clear();
        Ok(lost)
    }
}

const TEXT_HEADER: &str = concat!(
    "[Script Info]\n",
    "; Script generated by FFmpeg/Lavc63.1.100\n",
    "ScriptType: v4.00+\n",
    "PlayResX: 384\n",
    "PlayResY: 288\n",
    "ScaledBorderAndShadow: yes\n",
    "YCbCr Matrix: None\n",
    "\n",
    "[V4+ Styles]\n",
    "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n",
    "Style: Default,Arial,16,&Hffffff,&Hffffff,&H0,&H0,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n",
    "\n",
    "[Events]\n",
    "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
);

const ASS_HEADER: &str = concat!(
    "[Script Info]\n",
    "; Script generated by FFmpeg/Lavc63.1.100\n",
    "ScriptType: v4.00+\n",
    "PlayResX: 384\n",
    "PlayResY: 288\n",
    "ScaledBorderAndShadow: yes\n",
    "YCbCr Matrix: None\n",
    "\n",
    "[V4+ Styles]\n",
    "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n",
    "Style: Default,Arial,16,&Hffffff,&Hffffff,&H0,&H0,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n",
    "Style: Teletext,Monospace,11,&Hffffff,&Hffffff,&H0,&H0,0,0,0,0,160,100,0,0,3,0.1,0,5,1,1,1,0\n",
    "Style: Subtitle,Monospace,16,&Hffffff,&Hffffff,&H0,&H0,0,0,0,0,100,100,0,0,1,1,1,8,48,48,20,0\n",
    "\n",
    "[Events]\n",
    "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
);
