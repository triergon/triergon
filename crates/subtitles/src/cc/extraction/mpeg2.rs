//! FFmpeg's MPEG-2 video decoder as far as captions go: which pictures it
//! outputs, in which order, with which caption bytes and times.

use super::captions::Gathered;
use super::queue::FrameQueue;
use crate::cc::text::Text;
use crate::cc::video::mpeg2::{
    self, EXTENSION, FrameRate, GROUP, PICTURE, PictureCoding, PictureHeader, PictureStructure,
    PictureType, SEQUENCE_HEADER, SequenceExtension, StartCode,
};
use triergon_core::{Budget, BudgetVec, Result};

/// A PTS and where the stream bytes it came with start.
#[derive(Clone, Copy, Debug)]
struct Stamp {
    offset: usize,
    pts: Option<i64>,
}

/// A reference picture waiting for output; its bytes are in `held_bytes`.
#[derive(Clone, Copy, Debug)]
struct Held {
    pts: Option<i64>,
    duration: Option<i64>,
}

/// The decoder's state between pictures.
#[derive(Debug)]
struct Decoding {
    /// Timed by PES PTS in `time_base`, or (`None`) as a raw stream.
    time_base: Option<(u32, u32)>,
    stamps: BudgetVec<Stamp>,
    /// Stamps before this index have been taken.
    stamp_head: usize,
    rate: Option<FrameRate>,
    progressive_sequence: bool,
    after_sequence_header: bool,
    first_field: Option<PictureStructure>,
    references: u32,
    /// Raw stream: the decode time of the next picture, and the time of the
    /// next output frame once one has left.
    decode_time: i64,
    raw_next: Option<i64>,
    /// The bytes of the pictures decoded since the last frame took theirs.
    pending: Gathered,
    held: Option<Held>,
    held_bytes: Text,
}

/// MPEG-2 video elementary stream bytes, buffered until whole start-code
/// units (whole pictures) are in.
#[derive(Debug)]
pub(super) struct Mpeg2Decode {
    es: Text,
    /// Stream offset of `es[0]`.
    base: usize,
    /// Position in `es` of the next unit.
    cursor: usize,
    /// Position in `es` from which the end of the unit at `cursor` is still
    /// to be found; the bytes before it were searched.
    searched: usize,
    decoding: Decoding,
}

impl Mpeg2Decode {
    /// `time_base` for PES timing; `None` for a raw stream.
    pub(super) fn new(pool: &Budget, time_base: Option<(u32, u32)>) -> Self {
        Self {
            es: Text::new(pool),
            base: 0,
            cursor: 0,
            searched: 0,
            decoding: Decoding {
                time_base,
                stamps: BudgetVec::new(pool),
                stamp_head: 0,
                rate: None,
                progressive_sequence: false,
                after_sequence_header: false,
                first_field: None,
                references: 0,
                decode_time: 0,
                raw_next: None,
                pending: Gathered::new(pool),
                held: None,
                held_bytes: Text::new(pool),
            },
        }
    }

    /// Takes stream bytes; with PES timing their PTS belongs to the first
    /// picture starting in them.
    pub(super) fn push(
        &mut self,
        data: &[u8],
        pts: Option<i64>,
        out: &mut FrameQueue,
    ) -> Result<()> {
        if self.decoding.time_base.is_some() {
            self.decoding.stamp(self.base + self.es.len(), pts)?;
        }
        self.es.put(data)?;
        self.process(false, out)
    }

    /// The end of the stream: the last units and the reference picture held.
    pub(super) fn finish(&mut self, out: &mut FrameQueue) -> Result<()> {
        self.process(true, out)?;
        let decoding = &mut self.decoding;
        if let Some(last) = decoding.held.take() {
            // A raw stream's last frame has no time.
            let pts = decoding.time_base.and(last.pts);
            out.push(pts, last.duration, decoding.held_bytes.as_bytes())?;
        }
        Ok(())
    }

    /// Decodes every complete unit from the cursor on (at the end, every unit).
    /// Each byte is searched for start codes a bounded number of times, however
    /// the stream is cut into pushes.
    fn process(&mut self, finishing: bool, out: &mut FrameQueue) -> Result<()> {
        loop {
            let es = self.es.as_bytes();
            let Some((start, code)) = mpeg2::next_start_code(es, self.cursor) else {
                // Bytes before a start code are not decoded; only the last
                // three can begin one.
                self.cursor = self.cursor.max(es.len().saturating_sub(3));
                break;
            };
            self.cursor = start;
            // A picture runs to the next picture, GOP or sequence header,
            // anything else to the next start code.
            let ends =
                |next: u8| code != PICTURE || matches!(next, PICTURE | SEQUENCE_HEADER | GROUP);
            let mut at = self.searched.max(start + 4);
            let end = loop {
                match mpeg2::next_start_code(es, at) {
                    Some((offset, next)) if !ends(next) => at = offset + 4,
                    found => break found.map(|(offset, _)| offset),
                }
            };
            let Some(end) = end.or(finishing.then_some(es.len())) else {
                self.searched = at.max(es.len().saturating_sub(3));
                break;
            };
            if code == PICTURE {
                self.decoding
                    .picture(&es[start..end], self.base + start, out)?;
            } else {
                self.decoding.header(StartCode {
                    offset: start,
                    code,
                    data: &es[start + 4..end],
                });
            }
            self.cursor = end;
            self.searched = end;
        }
        if self.cursor > 0 && self.cursor * 2 >= self.es.len() {
            self.es.drop_front(self.cursor);
            self.base += self.cursor;
            self.searched -= self.cursor.min(self.searched);
            self.cursor = 0;
        }
        Ok(())
    }
}

impl Decoding {
    fn stamp(&mut self, offset: usize, pts: Option<i64>) -> Result<()> {
        // Taken stamps leave once they are at least half, so each moves once.
        let len = self.stamps.len();
        if self.stamp_head > 0 && self.stamp_head * 2 >= len {
            self.stamps
                .as_mut_slice()
                .copy_within(self.stamp_head..len, 0);
            self.stamps.truncate(len - self.stamp_head);
            self.stamp_head = 0;
        }
        if self.stamps.len() == self.stamps.capacity() {
            self.stamps.try_reserve_exact(self.stamps.len().max(16))?;
        }
        self.stamps
            .try_push(Stamp { offset, pts })
            .map_err(|rejected| rejected.error)
    }

    /// A sequence header and the sequence extension right after it.
    fn header(&mut self, unit: StartCode<'_>) {
        match unit.code {
            SEQUENCE_HEADER => {
                self.rate = mpeg2::sequence_frame_rate(unit.data);
                self.after_sequence_header = true;
                return;
            }
            EXTENSION if self.after_sequence_header => {
                if let Some(extension) = SequenceExtension::parse(unit.data) {
                    self.rate = self.rate.map(|r| extension.frame_rate(r));
                    self.progressive_sequence = extension.progressive_sequence;
                }
            }
            _ => {}
        }
        self.after_sequence_header = false;
    }

    /// One picture: from its start code to the next picture, GOP or sequence
    /// header, starting at stream offset `offset`.
    fn picture(&mut self, picture: &[u8], offset: usize, out: &mut FrameQueue) -> Result<()> {
        self.after_sequence_header = false;
        // A PTS belongs to the first picture starting in its PES packet.
        let stamps = &self.stamps.as_slice()[self.stamp_head..];
        let taken = stamps.iter().take_while(|s| s.offset <= offset).count();
        let pts = stamps[..taken].last().and_then(|stamp| stamp.pts);
        self.stamp_head += taken;
        let mut units = mpeg2::start_codes(picture);
        let Some(header) = units.next().and_then(|u| PictureHeader::parse(u.data)) else {
            return Ok(());
        };
        let mut coding = PictureCoding::default();
        for unit in units.take_while(|u| !mpeg2::is_slice(u.code)) {
            match unit.code {
                EXTENSION => {
                    if let Some(parsed) = PictureCoding::parse(unit.data) {
                        coding = parsed;
                    }
                }
                mpeg2::USER_DATA => {
                    // FFmpeg reads announced constructs up to the picture's end.
                    let available = &picture[unit.offset + 4..];
                    self.pending
                        .picture_user_data(available, coding.top_field_first)?;
                }
                _ => {}
            }
        }
        // The second field of a frame passes its bytes on.
        if coding.structure != PictureStructure::Frame {
            match self.first_field.take() {
                Some(first) if first != coding.structure => return Ok(()),
                _ => self.first_field = Some(coding.structure),
            }
        }
        let duration = self.duration(coding.display_fields(self.progressive_sequence));
        let decoded_at = self.decode_time;
        self.decode_time = self.decode_time.saturating_add(duration.unwrap_or(0));
        if header.picture_type == PictureType::Bidirectional && self.references < 2 {
            return Ok(());
        }
        let frame = Held { pts, duration };
        if header.picture_type.is_reference() {
            self.references = self.references.saturating_add(1);
            if let Some(previous) = self.held.replace(frame) {
                let at = self.output_time(previous, decoded_at);
                out.push(at, previous.duration, self.held_bytes.as_bytes())?;
            }
            std::mem::swap(&mut self.held_bytes, &mut self.pending.bytes);
        } else {
            let at = self.output_time(frame, decoded_at);
            out.push(at, duration, self.pending.bytes.as_bytes())?;
        }
        self.pending.bytes.clear();
        Ok(())
    }

    /// A frame's time: its PES PTS; in a raw stream the decode time of the
    /// picture that outputs it for the first frame, then the end of the frame
    /// before.
    fn output_time(&mut self, frame: Held, decoded_at: i64) -> Option<i64> {
        if self.time_base.is_some() {
            return frame.pts;
        }
        let at = self.raw_next.unwrap_or(decoded_at);
        self.raw_next = Some(at.saturating_add(frame.duration.unwrap_or(0)));
        Some(at)
    }

    /// `fields` field periods in the clock's time base, truncated.
    fn duration(&self, fields: u32) -> Option<i64> {
        let rate = self.rate?;
        let (num, den) = self.time_base.unwrap_or(super::RAW_MPEG2_TIME_BASE);
        let numerator = i64::from(den) * i64::from(rate.denominator) * i64::from(fields);
        let denominator = i64::from(num) * i64::from(rate.numerator) * 2;
        (denominator > 0).then(|| numerator / denominator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_arriving_in_pieces_is_searched_once() {
        let pool = Budget::new(1 << 24).expect("pool");
        let mut decode = Mpeg2Decode::new(&pool, Some((1, 90_000)));
        let mut out = FrameQueue::new(&pool);
        let header = [
            0, 0, 1, 0xb3, 0x2d, 0x01, 0xe0, 0x24, 0, 0, 1, 0, 0, 8, 0, 0, 0, 0, 1, 1,
        ];
        decode.push(&header, Some(0), &mut out).expect("push");
        for n in 0..2000 {
            decode.push(&[0x55; 184], Some(n), &mut out).expect("push");
            // Only a start code prefix cut at the end is searched again.
            assert!(decode.searched + 3 >= decode.es.len());
        }
        // The picture ends at the next picture, taking the first stamp.
        decode
            .push(&[0, 0, 1, 0, 0, 8], None, &mut out)
            .expect("push");
        let decoding = &decode.decoding;
        assert_eq!(decoding.stamps.len() - decoding.stamp_head, 2001);
    }
}
