//! The caption bytes of pictures: the user data and SEI formats FFmpeg reads
//! and the constructs it makes of them.
//!
//! FFmpeg's MPEG-2 decoder keeps at most [`PICTURE_LIMIT`] caption bytes for
//! the next frame: caption user data that would take them past it is left
//! out, and later user data that fits is still kept. Its H.264 and HEVC
//! decoders have no such limit.

use crate::cc::text::Text;
use crate::cc::user_data::{
    Scte20Captions, atsc_captions, dvd_captions, scte20_captions, t35_atsc_user_data,
};
use triergon_core::{Budget, Result};

/// The caption bytes FFmpeg's MPEG-2 decoder keeps for one frame (`3 *
/// A53_MAX_CC_COUNT`).
pub(super) const PICTURE_LIMIT: usize = 6000;

/// The caption bytes of one picture or access unit, gathered in a reused buffer.
#[derive(Debug)]
pub(super) struct Gathered {
    pub(super) bytes: Text,
    /// The caption user data format the stream locked to with its first
    /// caption user data.
    format: Option<Format>,
}

/// The caption user data formats of MPEG-2 pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Atsc,
    Scte20,
    Dvd,
    Dish,
}

/// The A/53 constructs FFmpeg takes: `process_cc_data_flag` set, at least one
/// construct and all of them in `available`.
fn atsc_constructs(available: &[u8]) -> Option<&[u8]> {
    let captions = atsc_captions(available)?;
    (captions.process_cc_data && captions.cc_count > 0 && captions.is_complete())
        .then_some(captions.cc_data)
}

/// The caption user data of one picture, in the formats FFmpeg reads.
#[derive(Clone, Debug)]
enum PictureCaptions<'a> {
    /// A/53 constructs as sent.
    Atsc(&'a [u8]),
    /// At least one pair announced.
    Scte20(Scte20Captions<'a>),
    /// Whole DVD caption blocks (two words each) and `caption_odd_field_first`.
    Dvd {
        blocks: &'a [u8],
        odd_field_first: bool,
    },
    /// Dish Network: one or two field 1 pairs.
    Dish { pairs: [[u8; 2]; 2], count: usize },
}

impl<'a> PictureCaptions<'a> {
    /// Recognises user data, `available` running from its payload to the end of
    /// the picture. FFmpeg reads that far when a header announces more than
    /// the user data holds.
    fn read(available: &'a [u8]) -> Option<Self> {
        if let Some(constructs) = atsc_constructs(available) {
            return Some(Self::Atsc(constructs));
        }
        if let Some(pairs) = scte20_captions(available) {
            return (pairs.remaining() > 0).then_some(Self::Scte20(pairs));
        }
        if let Some(dvd) = dvd_captions(available).filter(|_| available.len() >= 11) {
            // The announced counts are often wrong: blocks run while a block's
            // first word has the word marker.
            let blocks = dvd
                .words
                .chunks_exact(6)
                .take_while(|block| block[0] & 0xfe == 0xfe)
                .count();
            return (blocks > 0).then(|| Self::Dvd {
                blocks: &dvd.words[..blocks * 6],
                odd_field_first: dvd.odd_field_first,
            });
        }
        dish_pairs(available).map(|(pairs, count)| Self::Dish { pairs, count })
    }

    /// The bytes FFmpeg makes of them: three per announced SCTE 20 pair,
    /// present or not.
    fn len(&self) -> usize {
        match self {
            Self::Atsc(constructs) => constructs.len(),
            Self::Scte20(pairs) => 3 * pairs.remaining(),
            Self::Dvd { blocks, .. } => blocks.len(),
            Self::Dish { count, .. } => 3 * count,
        }
    }

    fn format(&self) -> Format {
        match self {
            Self::Atsc(_) => Format::Atsc,
            Self::Scte20(_) => Format::Scte20,
            Self::Dvd { .. } => Format::Dvd,
            Self::Dish { .. } => Format::Dish,
        }
    }

    /// The constructs FFmpeg turns the captions into.
    fn write(self, out: &mut Text, top_field_first: bool) -> Result<()> {
        match self {
            Self::Atsc(constructs) => out.put(constructs),
            Self::Scte20(pairs) => {
                // A pair cut short and those after it are zero.
                let announced = pairs.remaining();
                let mut written = 0;
                for pair in pairs {
                    let second_field = (pair.field == 2) == top_field_first;
                    let construct = match pair.field {
                        0 => [0; 3],
                        _ => [0x04 | u8::from(second_field), pair.data[0], pair.data[1]],
                    };
                    out.put(&construct)?;
                    written += 1;
                }
                for _ in written..announced {
                    out.put(&[0; 3])?;
                }
                Ok(())
            }
            Self::Dvd {
                blocks,
                odd_field_first,
            } => {
                // Field 1 data only where a word says so and stands where the
                // first field would; everything else as field 2.
                for block in blocks.chunks_exact(6) {
                    let first = if block[0] == 0xff && odd_field_first {
                        0xfc
                    } else {
                        0xfd
                    };
                    let second = if block[3] == 0xff && !odd_field_first {
                        0xfc
                    } else {
                        0xfd
                    };
                    out.put(&[first, block[1], block[2], second, block[4], block[5]])?;
                }
                Ok(())
            }
            Self::Dish { pairs, count } => {
                for pair in &pairs[..count] {
                    out.put(&[0xfc, pair[0], pair[1]])?;
                }
                Ok(())
            }
        }
    }
}

/// FFmpeg's reading of Dish Network caption user data (`05 02`, no public
/// specification): the caption type at byte 7, or for type 5 at byte 14. Type
/// 2 carries one pair (bytes 2 and 3 after the type byte) and a following type
/// byte; the pair is sent twice when that byte is 4 and the pair is a control
/// code. Type 4 carries two pairs.
fn dish_pairs(available: &[u8]) -> Option<([[u8; 2]; 2], usize)> {
    if available.len() < 12 || available[..2] != [0x05, 0x02] {
        return None;
    }
    let at = if available[7] == 0x05 { 14 } else { 7 };
    let rest = available.get(at..)?;
    match rest {
        [0x02, _, a, b, next, ..] => {
            let repeated = *next == 0x04 && a & 0x7f < 0x20;
            Some(([[*a, *b], [*a, *b]], if repeated { 2 } else { 1 }))
        }
        [0x04, _, a, b, c, d, ..] => Some(([[*a, *b], [*c, *d]], 2)),
        _ => None,
    }
}

/// Gathered bytes and format as they were, to go back to.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mark {
    len: usize,
    format: Option<Format>,
}

impl Gathered {
    pub(super) fn new(pool: &Budget) -> Self {
        Self {
            bytes: Text::new(pool),
            format: None,
        }
    }

    pub(super) fn mark(&self) -> Mark {
        Mark {
            len: self.bytes.len(),
            format: self.format,
        }
    }

    /// Goes back to `mark`, dropping the bytes gathered since.
    pub(super) fn rollback(&mut self, mark: Mark) {
        self.bytes.truncate(mark.len);
        self.format = mark.format;
    }

    /// The A/53 constructs of a T.35 SEI payload.
    pub(super) fn sei(&mut self, payload: &[u8]) -> Result<()> {
        match t35_atsc_user_data(payload).and_then(atsc_constructs) {
            Some(constructs) => self.bytes.put(constructs),
            None => Ok(()),
        }
    }

    /// A picture's caption user data, the bytes for the next frame starting
    /// at `pending`. The stream locks to the format of its first caption user
    /// data that is kept; user data of another format is ignored.
    pub(super) fn picture_user_data(
        &mut self,
        available: &[u8],
        top_field_first: bool,
        pending: usize,
    ) -> Result<()> {
        let Some(captions) = PictureCaptions::read(available) else {
            return Ok(());
        };
        let format = captions.format();
        let kept = self.bytes.len() - pending;
        if self.format.is_some_and(|locked| locked != format)
            || kept + captions.len() > PICTURE_LIMIT
        {
            return Ok(());
        }
        self.format = Some(format);
        captions.write(&mut self.bytes, top_field_first)
    }
}
