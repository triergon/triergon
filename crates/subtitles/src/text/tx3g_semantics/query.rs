use super::super::unicode::cancelled;
use super::*;
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, Result, WorkBudget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Run,
    Event,
    Possible,
    Resolve,
    Ready,
}
/// Resumable logarithmic lookup. Relative ticks are supplied explicitly in the
/// same track units as the sample. None means unknown, never zero milliseconds.
#[derive(Debug)]
pub struct PointQuery<'a> {
    semantic: &'a Semantics,
    character: usize,
    tick: Option<u32>,
    low: usize,
    high: usize,
    run: Option<usize>,
    event: Option<usize>,
    possible: bool,
    phase: Phase,
    result: Option<PointStyle>,
}
impl<'a> PointQuery<'a> {
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
            low: 0,
            high: semantic.runs.len(),
            run: None,
            event: None,
            possible: false,
            phase: Phase::Run,
            result: None,
        })
    }
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<PointStyle>> {
        loop {
            cancelled(cancel)?;
            if self.phase == Phase::Ready {
                return Ok(self.result);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            match self.phase {
                Phase::Run => {
                    if self.low == self.high {
                        let run = self
                            .semantic
                            .runs
                            .as_slice()
                            .get(self.low)
                            .ok_or(Error::Invalid("tx3g point run absent"))?;
                        if !run.characters.contains(&self.character) {
                            return Err(Error::Invalid("tx3g point run geometry"));
                        }
                        self.run = Some(self.low);
                        self.low = 0;
                        self.high = self.semantic.karaoke.len();
                        self.phase = Phase::Event;
                    } else {
                        let middle = self.low + (self.high - self.low) / 2;
                        if self.semantic.runs.as_slice()[middle].characters.end <= self.character {
                            self.low = middle + 1;
                        } else {
                            self.high = middle;
                        }
                    }
                }
                Phase::Event => {
                    let Some(tick) = self.tick else {
                        self.phase = Phase::Possible;
                        continue;
                    };
                    if self.low == self.high {
                        if self
                            .semantic
                            .karaoke
                            .as_slice()
                            .get(self.low)
                            .is_some_and(|e| e.start_tick <= tick && tick < e.end_tick)
                        {
                            self.event = Some(self.low);
                        }
                        self.phase = Phase::Resolve;
                    } else {
                        let middle = self.low + (self.high - self.low) / 2;
                        if self.semantic.karaoke.as_slice()[middle].end_tick <= tick {
                            self.low = middle + 1;
                        } else {
                            self.high = middle;
                        }
                    }
                }
                Phase::Possible => {
                    if self
                        .semantic
                        .globals
                        .display
                        .is_none_or(|d| d.continuous_karaoke)
                    {
                        self.possible = self
                            .semantic
                            .possible_karaoke_end
                            .is_some_and(|end| self.character < end);
                        self.phase = Phase::Resolve;
                    } else if self.low == self.high {
                        // Ordered disjoint source ranges mean only this first
                        // candidate can contain the character. Each comparison
                        // is charged; no event-list scan occurs on a query.
                        self.possible =
                            self.semantic
                                .karaoke
                                .as_slice()
                                .get(self.low)
                                .is_some_and(|event| {
                                    event.start_tick < event.end_tick
                                        && event.span.characters.contains(&self.character)
                                });
                        self.phase = Phase::Resolve;
                    } else {
                        let middle = self.low + (self.high - self.low) / 2;
                        if self.semantic.karaoke.as_slice()[middle].span.characters.end
                            <= self.character
                        {
                            self.low = middle + 1;
                        } else {
                            self.high = middle;
                        }
                    }
                }
                Phase::Resolve => {
                    let run = &self.semantic.runs.as_slice()
                        [self.run.ok_or(Error::Invalid("tx3g point run absent"))?];
                    let karaoke = if let Some(event) = self.event {
                        let span = &self.semantic.karaoke.as_slice()[event].span.characters;
                        if self.character >= span.end {
                            Truth::No
                        } else if self.character >= span.start {
                            Truth::Yes
                        } else {
                            match self.semantic.globals.display.map(|x| x.continuous_karaoke) {
                                Some(true) => Truth::Yes,
                                Some(false) => Truth::No,
                                None => Truth::Unknown,
                            }
                        }
                    } else if self.tick.is_none() && self.possible {
                        Truth::Unknown
                    } else {
                        Truth::No
                    };
                    let colour = self
                        .semantic
                        .globals
                        .highlight_colour
                        .as_ref()
                        .map(|x| x.value);
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
                    let result = PointStyle {
                        base_style: run.style,
                        style_origin: run.style_origin,
                        font_index: run.font_index,
                        hyperlink: run.hyperlink,
                        blink_requested: run.blink.is_some(),
                        static_highlight: run.highlight.is_some(),
                        karaoke,
                        paint,
                    };
                    self.result = Some(result);
                    self.phase = Phase::Ready;
                }
                Phase::Ready => unreachable!(),
            }
        }
    }
}
