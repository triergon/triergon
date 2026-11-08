//! Packet times from granule positions.
//!
//! A page's granule position is the end of the last packet completed on it, so a page's
//! packets are timed backwards from it with their durations. The last page of a stream
//! is timed forwards from the page before instead: its granule position may stop short
//! of the samples coded, and the difference is trimmed from the end (Vorbis I section
//! A.2, RFC 7845 section 4.5). Time before zero is never presented: a packet that starts
//! before it carries a start trim, which is how Opus pre-skip and a Vorbis or Opus start
//! offset (a first page whose granule position is smaller than its samples) reach the
//! decoder.
use crate::compat::ReferenceTiming;
use crate::mapping::{Durations, Timeline};
use triergon_core::container::Trim;

/// One media packet of a page while its times are worked out, in link-local ticks.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Slot {
    pub durations: Durations,
    pub pts: Option<i64>,
    pub trim: Trim,
    /// The time FFmpeg reports, from [`crate::compat`].
    pub reference: ReferenceTiming,
}

/// What a page says about the packets it completes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageClock {
    /// The granule position in ticks.
    pub granule: Option<i64>,
    pub end_of_stream: bool,
}

/// The time where a stream's next packet starts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    next: Option<i64>,
}

impl Clock {
    /// A clock at the start of a stream, whose first sample is at `start` ticks.
    pub(crate) fn at(start: Option<i64>) -> Clock {
        Clock { next: start }
    }

    /// Where the stream's last timed packet ended.
    pub(crate) fn end(&self) -> Option<i64> {
        self.next
    }

    /// A page that delivers no media packet still states where its packets ended.
    pub(crate) fn pass(&mut self, page: PageClock) {
        if page.granule.is_some() {
            self.next = page.granule;
        }
    }

    /// Time the packets of one page.
    pub(crate) fn page<S: AsMut<Slot>>(
        &mut self,
        timeline: &Timeline,
        page: PageClock,
        slots: &mut [S],
    ) {
        let Some(last) = slots.len().checked_sub(1) else {
            return;
        };
        let duration = |slot: &Slot| slot.durations.exact.map(i64::from);
        // Packet ends, held in `pts` until the starts are known.
        let forwards = page.end_of_stream && self.next.is_some() || page.granule.is_none();
        if let (false, Some(granule)) = (forwards, page.granule) {
            let mut end = Some(granule);
            for slot in slots.iter_mut().rev().map(AsMut::as_mut) {
                slot.pts = end;
                end = end.zip(duration(slot)).map(|(end, d)| end - d);
            }
        }
        let mut end = self.next;
        for slot in slots.iter_mut().map(AsMut::as_mut) {
            let forward = end.zip(duration(slot)).map(|(end, d)| end + d);
            slot.pts = if forwards {
                forward
            } else {
                slot.pts.or(forward)
            };
            end = slot.pts;
        }
        if timeline.trims()
            && page.end_of_stream
            && let (Some(granule), Some(end)) = (page.granule, slots[last].as_mut().pts)
        {
            let mut excess = end - granule;
            for slot in slots.iter_mut().rev().map(AsMut::as_mut) {
                let Some(d) = duration(slot).filter(|_| excess > 0) else {
                    break;
                };
                let trimmed = d.min(excess);
                slot.trim.end = Some(trimmed);
                excess -= trimmed;
            }
        }
        self.next = match (slots[last].as_mut().pts, page.granule) {
            (Some(end), Some(granule)) if page.end_of_stream => Some(end.min(granule)),
            (end, granule) => end.or(granule),
        };
        for slot in slots.iter_mut().map(AsMut::as_mut) {
            slot.pts = slot.pts.zip(duration(slot)).map(|(end, d)| end - d);
            if timeline.trims()
                && let (Some(pts), Some(d)) = (slot.pts, duration(slot))
                && pts < 0
            {
                slot.trim.start = Some(d.min(-pts));
            }
        }
    }
}

impl AsMut<Slot> for Slot {
    fn as_mut(&mut self) -> &mut Slot {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(durations: &[u32]) -> Vec<Slot> {
        durations
            .iter()
            .map(|&d| Slot {
                durations: Durations {
                    exact: Some(d),
                    standalone: Some(d),
                    carried: Some(d),
                },
                ..Slot::default()
            })
            .collect()
    }

    const OPUS: Timeline = Timeline::Opus { pre_skip: 312 };

    #[test]
    fn pages_time_backwards_from_their_granule() {
        let mut clock = Clock::at(Some(-312));
        let mut page = slots(&[960, 960]);
        clock.page(
            &OPUS,
            PageClock {
                granule: Some(1608),
                end_of_stream: false,
            },
            &mut page,
        );
        assert_eq!(page[0].pts, Some(-312));
        assert_eq!(
            page[0].trim,
            Trim {
                start: Some(312),
                end: None
            }
        );
        assert_eq!(page[1].pts, Some(648));
        assert_eq!(page[1].trim, Trim::default());
        // A gap before the next page does not move it: its granule decides.
        let mut page = slots(&[960]);
        clock.page(
            &OPUS,
            PageClock {
                granule: Some(9000),
                end_of_stream: false,
            },
            &mut page,
        );
        assert_eq!(page[0].pts, Some(8040));
        assert_eq!(clock.end(), Some(9000));
    }

    #[test]
    fn the_last_page_trims_what_its_granule_leaves_out() {
        let mut clock = Clock::at(Some(0));
        let mut page = slots(&[960]);
        clock.page(
            &OPUS,
            PageClock {
                granule: Some(960),
                end_of_stream: false,
            },
            &mut page,
        );
        let mut page = slots(&[960, 960]);
        clock.page(
            &OPUS,
            PageClock {
                granule: Some(1984),
                end_of_stream: true,
            },
            &mut page,
        );
        assert_eq!((page[0].pts, page[1].pts), (Some(960), Some(1920)));
        assert_eq!(page[1].trim.end, Some(896));
        assert_eq!(page[0].trim.end, None);
        assert_eq!(clock.end(), Some(1984));
        // A trim longer than the last packet reaches back.
        let mut clock = Clock::at(Some(0));
        let mut page = slots(&[960, 960]);
        clock.page(
            &OPUS,
            PageClock {
                granule: Some(500),
                end_of_stream: true,
            },
            &mut page,
        );
        assert_eq!((page[0].trim.end, page[1].trim.end), (Some(460), Some(960)));
        // FLAC states no trims.
        let mut clock = Clock::at(Some(0));
        let mut page = slots(&[4096]);
        clock.page(
            &Timeline::Flac,
            PageClock {
                granule: Some(100),
                end_of_stream: true,
            },
            &mut page,
        );
        assert_eq!(page[0].trim, Trim::default());
    }

    #[test]
    fn unknown_durations_leave_times_unknown_not_guessed() {
        let mut clock = Clock::at(None);
        let mut page = slots(&[1024, 1024]);
        page[0].durations.exact = None;
        clock.page(
            &Timeline::Flac,
            PageClock {
                granule: Some(5000),
                end_of_stream: false,
            },
            &mut page,
        );
        assert_eq!(page[1].pts, Some(3976));
        assert_eq!(page[0].pts, None);
        let mut page = slots(&[10]);
        clock.page(
            &Timeline::Flac,
            PageClock {
                granule: None,
                end_of_stream: false,
            },
            &mut page,
        );
        assert_eq!(page[0].pts, Some(5000));
    }
}
