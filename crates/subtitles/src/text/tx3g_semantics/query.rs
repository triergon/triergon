use super::*;
use crate::cancelled;
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, Result, WorkBudget};

/// Finds the [`PointStyle`] at one character and time by binary search over the
/// runs and karaoke entries, without allocating.
#[derive(Debug)]
pub struct PointQuery<'a> {
    semantic: &'a Semantics,
    character: usize,
    tick: Option<u32>,
    search: Search,
}

/// How far the query has got. Each search narrows `low..high`.
#[derive(Clone, Copy, Debug)]
enum Search {
    /// The character's run.
    Run { low: usize, high: usize },
    /// The karaoke entry active at the tick, in the run found.
    Event { run: usize, low: usize, high: usize },
    /// Without a tick: whether karaoke highlighting may cover the character.
    Possible { run: usize, low: usize, high: usize },
    /// Everything the style depends on is found.
    Resolve {
        run: usize,
        event: Option<usize>,
        possible: bool,
    },
    /// The style.
    Ready(PointStyle),
}

impl<'a> PointQuery<'a> {
    /// Queries `character`, an index below [`Semantics::character_count`], at
    /// `relative_tick` track ticks from the start of the sample; `None` is an
    /// unknown time, not zero. Fails with `Error::Invalid` when `character` is
    /// outside the text.
    pub fn new(
        semantic: &'a Semantics,
        character: usize,
        relative_tick: Option<u32>,
    ) -> Result<Self> {
        if character >= semantic.characters {
            return Err(Error::Invalid("tx3g point outside text"));
        }
        Ok(Self {
            semantic,
            character,
            tick: relative_tick,
            search: Search::Run {
                low: 0,
                high: semantic.runs.len(),
            },
        })
    }

    /// Advances by one unit per search step; `Ok(None)` means the work ran
    /// out, so poll again. Once found, the style is returned on every later
    /// call without charging.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<PointStyle>> {
        let semantic = self.semantic;
        let character = self.character;
        loop {
            cancelled(cancel)?;
            if let Search::Ready(style) = self.search {
                return Ok(Some(style));
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            self.search = match self.search {
                Search::Run { low, high } if low == high => {
                    let run = semantic
                        .runs
                        .as_slice()
                        .get(low)
                        .ok_or(Error::Invalid("tx3g point run absent"))?;
                    if !run.characters.contains(&character) {
                        return Err(Error::Invalid("tx3g point run geometry"));
                    }
                    Search::Event {
                        run: low,
                        low: 0,
                        high: semantic.karaoke.len(),
                    }
                }
                Search::Run { low, high } => {
                    let (low, high) = narrow(low, high, |middle| {
                        semantic.runs.as_slice()[middle].characters.end <= character
                    });
                    Search::Run { low, high }
                }
                Search::Event { run, low, high } => match self.tick {
                    None => Search::Possible { run, low, high },
                    Some(tick) if low == high => Search::Resolve {
                        run,
                        event: semantic
                            .karaoke
                            .as_slice()
                            .get(low)
                            .is_some_and(|e| e.start_tick <= tick && tick < e.end_tick)
                            .then_some(low),
                        possible: false,
                    },
                    Some(tick) => {
                        let (low, high) = narrow(low, high, |middle| {
                            semantic.karaoke.as_slice()[middle].end_tick <= tick
                        });
                        Search::Event { run, low, high }
                    }
                },
                Search::Possible { run, low, high } => {
                    if semantic
                        .globals
                        .display
                        .is_none_or(|d| d.continuous_karaoke)
                    {
                        Search::Resolve {
                            run,
                            event: None,
                            possible: semantic
                                .possible_karaoke_end
                                .is_some_and(|end| character < end),
                        }
                    } else if low == high {
                        // Ordered disjoint source ranges mean only this first
                        // candidate can contain the character. Each comparison
                        // is charged; no event-list scan occurs on a query.
                        Search::Resolve {
                            run,
                            event: None,
                            possible: semantic.karaoke.as_slice().get(low).is_some_and(|event| {
                                event.start_tick < event.end_tick
                                    && event.span.characters.contains(&character)
                            }),
                        }
                    } else {
                        let (low, high) = narrow(low, high, |middle| {
                            semantic.karaoke.as_slice()[middle].span.characters.end <= character
                        });
                        Search::Possible { run, low, high }
                    }
                }
                Search::Resolve {
                    run,
                    event,
                    possible,
                } => Search::Ready(self.style(&semantic.runs.as_slice()[run], event, possible)),
                Search::Ready(style) => Search::Ready(style),
            };
        }
    }

    /// The style of the character in `run`, with the karaoke entry active at
    /// the tick, or, without a tick, whether karaoke may cover it.
    fn style(&self, run: &ResolvedRun, event: Option<usize>, possible: bool) -> PointStyle {
        let globals = &self.semantic.globals;
        let karaoke = if let Some(event) = event {
            let span = &self.semantic.karaoke.as_slice()[event].span.characters;
            if self.character >= span.end {
                Truth::No
            } else if self.character >= span.start {
                Truth::Yes
            } else {
                match globals.display.map(|x| x.continuous_karaoke) {
                    Some(true) => Truth::Yes,
                    Some(false) => Truth::No,
                    None => Truth::Unknown,
                }
            }
        } else if self.tick.is_none() && possible {
            Truth::Unknown
        } else {
            Truth::No
        };
        let colour = globals.highlight_colour.as_ref().map(|x| x.value);
        let paint = if karaoke == Truth::Unknown {
            PaintIntent::UnresolvedDynamicExtent
        } else if run.hyperlink.is_some() {
            PaintIntent::HyperlinkTerminalStyle
        } else if karaoke == Truth::Yes {
            PaintIntent::KaraokeForeground(colour)
        } else if run.highlight.is_some() {
            colour.map_or(
                PaintIntent::StaticTerminalInvert,
                PaintIntent::StaticBackground,
            )
        } else {
            PaintIntent::RequestedStyle
        };
        PointStyle {
            base_style: run.style,
            style_origin: run.style_origin,
            font_index: run.font_index,
            hyperlink: run.hyperlink,
            blink_requested: run.blink.is_some(),
            static_highlight: run.highlight.is_some(),
            karaoke,
            paint,
        }
    }
}

/// One step of a binary search of `low..high`, `before` telling whether the
/// target lies after the middle.
fn narrow(low: usize, high: usize, before: impl FnOnce(usize) -> bool) -> (usize, usize) {
    let middle = low + (high - low) / 2;
    if before(middle) {
        (middle + 1, high)
    } else {
        (low, middle)
    }
}
