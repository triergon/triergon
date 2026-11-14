//! FFmpeg's MPEG-2 video decoder as far as captions go: which pictures it
//! outputs, in which order, with which caption bytes and times.
//!
//! A push or the finish either succeeds or leaves the decoder and its output
//! as they were: while it runs, buffers are only appended to and the rest of
//! the state is a copy, and what it no longer needs is dropped once it has
//! succeeded.

use super::captions::{Gathered, Mark};
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

/// A reference picture waiting for output, with its bytes in `captions`.
#[derive(Clone, Copy, Debug)]
struct Held {
    pts: Option<i64>,
    duration: Option<i64>,
    start: usize,
    end: usize,
}

/// The decoder's state between pictures, besides its buffers.
#[derive(Clone, Copy, Debug, Default)]
struct Progress {
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
    held: Option<Held>,
    /// Where in `captions` the bytes of the pictures decoded since the last
    /// frame took theirs start.
    pending: usize,
}

#[derive(Debug)]
struct Decoding {
    /// Timed by PES PTS in `time_base`, or (`None`) as a raw stream.
    time_base: Option<(u32, u32)>,
    stamps: BudgetVec<Stamp>,
    /// The bytes of the held picture, then those pending; before and between
    /// them, bytes of frames that have left.
    captions: Gathered,
    progress: Progress,
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

/// The decoder and its output before a step, to go back to.
#[derive(Clone, Copy, Debug)]
struct Checkpoint {
    es: usize,
    cursor: usize,
    searched: usize,
    stamps: usize,
    progress: Progress,
    captions: Mark,
    frames: usize,
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
                captions: Gathered::new(pool),
                progress: Progress::default(),
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
        self.step(out, |decode, out| {
            if decode.decoding.time_base.is_some() {
                let offset = decode.base + decode.es.len();
                decode.decoding.stamp(offset, pts)?;
            }
            decode.es.put(data)?;
            decode.process(false, out)
        })
    }

    /// The end of the stream: the last units and the reference picture held.
    pub(super) fn finish(&mut self, out: &mut FrameQueue) -> Result<()> {
        self.step(out, |decode, out| {
            decode.process(true, out)?;
            decode.decoding.release_held(out)
        })
    }

    /// Runs `step`, then drops what is no longer needed; when it fails, the
    /// decoder and `out` are left as they were.
    fn step(
        &mut self,
        out: &mut FrameQueue,
        step: impl FnOnce(&mut Self, &mut FrameQueue) -> Result<()>,
    ) -> Result<()> {
        let checkpoint = Checkpoint {
            es: self.es.len(),
            cursor: self.cursor,
            searched: self.searched,
            stamps: self.decoding.stamps.len(),
            progress: self.decoding.progress,
            captions: self.decoding.captions.mark(),
            frames: out.len(),
        };
        let result = step(self, out);
        if result.is_ok() {
            self.settle();
        } else {
            self.es.truncate(checkpoint.es);
            self.cursor = checkpoint.cursor;
            self.searched = checkpoint.searched;
            let decoding = &mut self.decoding;
            decoding.stamps.truncate(checkpoint.stamps);
            decoding.progress = checkpoint.progress;
            decoding.captions.rollback(checkpoint.captions);
            out.truncate(checkpoint.frames);
        }
        result
    }

    /// Drops the stream bytes decoded once they are at least half of them.
    fn settle(&mut self) {
        if self.cursor > 0 && self.cursor * 2 >= self.es.len() {
            self.es.drop_front(self.cursor);
            self.base += self.cursor;
            self.searched -= self.cursor.min(self.searched);
            self.cursor = 0;
        }
        self.decoding.settle();
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
                return Ok(());
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
                return Ok(());
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
    }
}

impl Decoding {
    fn stamp(&mut self, offset: usize, pts: Option<i64>) -> Result<()> {
        if self.stamps.len() == self.stamps.capacity() {
            self.stamps.try_reserve_exact(self.stamps.len().max(16))?;
        }
        self.stamps
            .try_push(Stamp { offset, pts })
            .map_err(|rejected| rejected.error)
    }

    /// A sequence header and the sequence extension right after it.
    fn header(&mut self, unit: StartCode<'_>) {
        let progress = &mut self.progress;
        match unit.code {
            SEQUENCE_HEADER => {
                progress.rate = mpeg2::sequence_frame_rate(unit.data);
                progress.after_sequence_header = true;
                return;
            }
            EXTENSION if progress.after_sequence_header => {
                if let Some(extension) = SequenceExtension::parse(unit.data) {
                    progress.rate = progress.rate.map(|r| extension.frame_rate(r));
                    progress.progressive_sequence = extension.progressive_sequence;
                }
            }
            _ => {}
        }
        progress.after_sequence_header = false;
    }

    /// One picture: from its start code to the next picture, GOP or sequence
    /// header, starting at stream offset `offset`.
    fn picture(&mut self, picture: &[u8], offset: usize, out: &mut FrameQueue) -> Result<()> {
        self.progress.after_sequence_header = false;
        // A PTS belongs to the first picture starting in its PES packet.
        let stamps = &self.stamps.as_slice()[self.progress.stamp_head..];
        let taken = stamps.iter().take_while(|s| s.offset <= offset).count();
        let pts = stamps[..taken].last().and_then(|stamp| stamp.pts);
        self.progress.stamp_head += taken;
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
                    let (top_field_first, pending) =
                        (coding.top_field_first, self.progress.pending);
                    self.captions
                        .picture_user_data(available, top_field_first, pending)?;
                }
                _ => {}
            }
        }
        let time_base = self.time_base;
        let progress = &mut self.progress;
        // The second field of a frame passes its bytes on.
        if coding.structure != PictureStructure::Frame {
            match progress.first_field.take() {
                Some(first) if first != coding.structure => return Ok(()),
                _ => progress.first_field = Some(coding.structure),
            }
        }
        let fields = coding.display_fields(progress.progressive_sequence);
        let duration = progress.duration(time_base, fields);
        let decoded_at = progress.decode_time;
        progress.decode_time = progress.decode_time.saturating_add(duration.unwrap_or(0));
        if header.picture_type == PictureType::Bidirectional && progress.references < 2 {
            return Ok(());
        }
        let bytes = self.captions.bytes.as_bytes();
        if header.picture_type.is_reference() {
            progress.references = progress.references.saturating_add(1);
            let frame = Held {
                pts,
                duration,
                start: progress.pending,
                end: bytes.len(),
            };
            if let Some(previous) = progress.held.replace(frame) {
                let at =
                    progress.output_time(time_base, previous.pts, previous.duration, decoded_at);
                out.push(at, previous.duration, &bytes[previous.start..previous.end])?;
            }
        } else {
            let at = progress.output_time(time_base, pts, duration, decoded_at);
            out.push(at, duration, &bytes[progress.pending..])?;
        }
        progress.pending = bytes.len();
        Ok(())
    }

    /// Outputs the reference picture held, at the end of the stream.
    fn release_held(&mut self, out: &mut FrameQueue) -> Result<()> {
        if let Some(last) = self.progress.held.take() {
            // A raw stream's last frame has no time.
            let pts = self.time_base.and(last.pts);
            let bytes = &self.captions.bytes.as_bytes()[last.start..last.end];
            out.push(pts, last.duration, bytes)?;
        }
        Ok(())
    }

    /// Drops taken stamps and the bytes of frames that have left once they
    /// are at least half of their buffer, so each moves a bounded number of
    /// times.
    fn settle(&mut self) {
        let progress = &mut self.progress;
        let len = self.stamps.len();
        if progress.stamp_head > 0 && progress.stamp_head * 2 >= len {
            self.stamps
                .as_mut_slice()
                .copy_within(progress.stamp_head..len, 0);
            self.stamps.truncate(len - progress.stamp_head);
            progress.stamp_head = 0;
        }
        let bytes = &mut self.captions.bytes;
        let len = bytes.len();
        let held = progress.held.map_or(0..0, |held| held.start..held.end);
        let live = held.len() + (len - progress.pending);
        if len - live > 0 && len - live >= live {
            bytes.copy_within(held.clone(), 0);
            bytes.copy_within(progress.pending..len, held.len());
            bytes.truncate(live);
            if let Some(held) = &mut progress.held {
                held.end -= held.start;
                held.start = 0;
            }
            progress.pending = held.len();
        }
    }
}

impl Progress {
    /// A frame's time: its PES PTS; in a raw stream the decode time of the
    /// picture that outputs it for the first frame, then the end of the frame
    /// before.
    fn output_time(
        &mut self,
        time_base: Option<(u32, u32)>,
        pts: Option<i64>,
        duration: Option<i64>,
        decoded_at: i64,
    ) -> Option<i64> {
        if time_base.is_some() {
            return pts;
        }
        let at = self.raw_next.unwrap_or(decoded_at);
        self.raw_next = Some(at.saturating_add(duration.unwrap_or(0)));
        Some(at)
    }

    /// `fields` field periods in the clock's time base, truncated.
    fn duration(&self, time_base: Option<(u32, u32)>, fields: u32) -> Option<i64> {
        let rate = self.rate?;
        let (num, den) = time_base.unwrap_or(super::RAW_MPEG2_TIME_BASE);
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
        assert_eq!(decoding.stamps.len() - decoding.progress.stamp_head, 2001);
    }
}
