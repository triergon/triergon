//! Delivery from a source without random access.
//!
//! The movie box came first and the open stopped at the first media box. Samples leave in
//! the order their bytes arrive, which is the reader's order on any source: the bytes
//! before a sample are passed over, and a sample that starts before the read position is
//! refused, because only a range request could reach it. After each media box the root
//! boxes that follow are read one at a time: a `moof` is indexed into its tracks, the next
//! `mdat` continues delivery, any other box is passed.
//!
//! Nothing is kept for the length of the source: a `moof` and its indexes are released
//! once their samples have been delivered.
use super::{Output, Read, Reader, ReaderEvent, ReaderLimits, Reading, Track, take_earliest};
use crate::boxes::{BoxEvent, BoxHeader, BoxReader};
use crate::fragment::Trex;
use crate::open::{MoofWalk, fragment_defaults};
use std::sync::atomic::AtomicBool;
use triergon_core::container::Event as Neutral;
use triergon_core::{BudgetVec, Error, InputCursor, Result, SourceLocation, Step, WorkBudget};

/// The refusal of a sample only a range request could reach.
const BEHIND: Error = Error::Unsupported("sample before the read position needs random access");
const OUTSIDE: Error = Error::Invalid("sample outside media extent");

/// Where forward delivery stands among the root boxes.
#[derive(Debug)]
pub(super) struct Forward {
    /// The fragment defaults of the movie's `trex` boxes.
    trex: BudgetVec<Trex>,
    /// End of the last media box passed. A sample that starts before it may lie in a media
    /// box the cursor has left.
    passed: u64,
    at: At,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Keep the root reader inline without allocating at each media box"
)]
enum At {
    /// Delivering the samples that lie in this media box.
    Media(BoxHeader),
    /// Passing the rest of this media box, which holds no sample still to deliver.
    Rest(BoxHeader),
    /// Reading the root boxes that follow a media box.
    Roots {
        reader: BoxReader,
        root: Root,
    },
    /// The source has ended; a sample still indexed has no media.
    Ending,
    End,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Keep the fragment walk inline without allocating at each fragment"
)]
enum Root {
    /// The next header is to be read.
    Next,
    /// This header was read and its box has yet to be read or passed.
    Announced(BoxHeader),
    /// The fragments of a `moof` are being admitted to their tracks.
    Moof(MoofWalk),
}

fn passed(header: BoxHeader) -> Output {
    Step::Ready(Neutral::Other(ReaderEvent::Box(header)))
}

/// Whether `sample` lies after the media box `media`, in a box still to come. A sample
/// without bytes at the box's last offset belongs to the box.
fn follows(media: &BoxHeader, sample: &crate::Sample) -> Result<bool> {
    let range = &sample.range;
    Ok(media
        .end()?
        .is_some_and(|end| range.start > end || (range.start == end && range.end != end)))
}

impl Forward {
    /// Delivery from `media`, the first media box, which the cursor has just entered.
    pub(super) fn new(media: BoxHeader, trex: &BudgetVec<Trex>) -> Result<Self> {
        Ok(Self {
            trex: trex.try_clone()?,
            passed: 0,
            at: At::Media(media),
        })
    }

    /// One bounded step: a packet, a root box that was passed, a request or the end;
    /// `None` to step again.
    // Out of line, so that the reader's step keeps the code it has for a source with
    // random access.
    #[inline(never)]
    pub(super) fn poll(
        &mut self,
        input: &mut InputCursor,
        tracks: &mut [Track],
        reading: &mut Option<Reading>,
        limits: &ReaderLimits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Output>> {
        match &mut self.at {
            At::Media(media) => {
                let media = *media;
                let mut current = if let Some(current) = reading.take() {
                    current
                } else {
                    for track in tracks.iter_mut() {
                        if !track.fill(work)? {
                            return Ok(Some(Step::Yield));
                        }
                    }
                    let Some((track, sample)) = take_earliest(tracks) else {
                        self.at = At::Rest(media);
                        return Ok(None);
                    };
                    if follows(&media, &sample)? {
                        tracks[track].head = Some(sample);
                        self.at = At::Rest(media);
                        return Ok(None);
                    }
                    if !work.try_charge(1) {
                        tracks[track].head = Some(sample);
                        return Ok(Some(Step::Yield));
                    }
                    self.reachable(&media, &sample, input.position(), limits)?;
                    Reading {
                        track,
                        sample,
                        buffer: None,
                    }
                };
                // The bytes between the read position and the sample belong to no sample
                // still to deliver. A sample without bytes moves nothing.
                let range = &current.sample.range;
                let gap = if range.start == range.end {
                    0
                } else {
                    range.start.saturating_sub(input.position())
                };
                let step = if gap == 0 {
                    Reader::read_payload(input, &mut current, work, cancel)
                } else {
                    let most = usize::try_from(gap).unwrap_or(usize::MAX);
                    input.read_discard(most, work, cancel).and_then(|step| {
                        Ok(Read::Pending(match step {
                            Step::Ready(_) => None,
                            Step::Yield => Some(Step::Yield),
                            Step::NeedInput(request) => Some(Step::NeedInput(request)),
                            Step::End(_) => return Err(Error::Truncated),
                        }))
                    })
                };
                match step {
                    Ok(Read::Payload(payload)) => {
                        Reader::finish_packet(tracks, current, payload).map(Some)
                    }
                    Ok(Read::Pending(output)) => {
                        *reading = Some(current);
                        Ok(output)
                    }
                    Err(error) => {
                        *reading = Some(current);
                        Err(error)
                    }
                }
            }
            At::Rest(media) => {
                let mut media = *media;
                let Some(end) = media.end()? else {
                    // The box runs to the end of the source, where its size is known.
                    return Ok(match input.read_discard(usize::MAX, work, cancel)? {
                        Step::Ready(_) => None,
                        Step::Yield => Some(Step::Yield),
                        Step::NeedInput(request) => Some(Step::NeedInput(request)),
                        Step::End(end) => {
                            media.size =
                                Some(end.offset.checked_sub(media.start).ok_or(Error::Overflow)?);
                            self.at = At::End;
                            Some(passed(media))
                        }
                    });
                };
                let left = end.saturating_sub(input.position());
                if left != 0 {
                    let most = usize::try_from(left).unwrap_or(usize::MAX);
                    return Ok(match input.read_discard(most, work, cancel)? {
                        Step::Ready(_) => None,
                        Step::Yield => Some(Step::Yield),
                        Step::NeedInput(request) => Some(Step::NeedInput(request)),
                        Step::End(_) => return Err(Error::Truncated),
                    });
                }
                if !work.try_charge(1) {
                    return Ok(Some(Step::Yield));
                }
                let reader = BoxReader::new(input, limits.open.metadata_bytes)?;
                self.passed = end;
                self.at = At::Roots {
                    reader,
                    root: Root::Next,
                };
                Ok(Some(passed(media)))
            }
            At::Roots { reader, root } => match root {
                Root::Next => Ok(Some(match reader.poll(input, work, cancel)? {
                    Step::Yield => Step::Yield,
                    Step::NeedInput(request) => Step::NeedInput(request),
                    Step::End(_) => {
                        self.at = At::Ending;
                        return Ok(None);
                    }
                    Step::Ready(BoxEvent::Header(header)) => {
                        *root = Root::Announced(header);
                        Step::Yield
                    }
                    Step::Ready(BoxEvent::Metadata(moof)) => {
                        for track in tracks.iter_mut() {
                            track.release_delivered();
                        }
                        *root = Root::Moof(MoofWalk::new(&moof, limits.open.format)?);
                        passed(*moof.header())
                    }
                    Step::Ready(BoxEvent::Skipped(header)) => passed(header),
                })),
                Root::Announced(header) => {
                    if !work.try_charge(1) {
                        return Ok(Some(Step::Yield));
                    }
                    match &header.kind {
                        b"mdat" => {
                            // The reader stops behind the header: at the first media byte.
                            self.at = At::Media(*header);
                            return Ok(None);
                        }
                        b"moof" => reader.read_metadata(input)?,
                        b"moov" => return Err(Error::Invalid("duplicate structural movie box")),
                        _ => reader.skip(input)?,
                    }
                    *root = Root::Next;
                    Ok(Some(Step::Yield))
                }
                Root::Moof(walk) => {
                    let trex = self.trex.as_slice();
                    if walk.poll(
                        tracks,
                        |id| fragment_defaults(trex, id),
                        input.budget(),
                        limits.open.format,
                        false,
                        work,
                    )? {
                        *root = Root::Next;
                    }
                    Ok(Some(Step::Yield))
                }
            },
            At::Ending => {
                for track in tracks.iter_mut() {
                    if !track.fill(work)? {
                        return Ok(Some(Step::Yield));
                    }
                }
                if let Some((_, sample)) = take_earliest(tracks) {
                    return Err(self.unreached(&sample));
                }
                self.at = At::End;
                Ok(None)
            }
            At::End => Ok(Some(Step::End(SourceLocation {
                source: input.source(),
                offset: input.position(),
            }))),
        }
    }

    /// Why a sample that starts before the media the cursor can still read is not read:
    /// it may lie in a media box already passed, or in none.
    fn unreached(&self, sample: &crate::Sample) -> Error {
        if sample.range.start < self.passed {
            BEHIND
        } else {
            OUTSIDE
        }
    }

    /// Checks that `sample` lies in the media box `media` and that its bytes are still to
    /// come at `position`.
    fn reachable(
        &self,
        media: &BoxHeader,
        sample: &crate::Sample,
        position: u64,
        limits: &ReaderLimits,
    ) -> Result<()> {
        let start = media
            .start
            .checked_add(u64::from(media.header_size))
            .ok_or(Error::Overflow)?;
        if sample.range.start < start {
            return Err(self.unreached(sample));
        }
        sample.require_within(start..media.end()?.unwrap_or(u64::MAX))?;
        if limits.payload_length(sample)? != 0 && sample.range.start < position {
            return Err(BEHIND);
        }
        Ok(())
    }
}
