//! A field-at-a-time grammar machine, derived from primary tables 22–26.
//! Each accepted value is one completed grammar field, not a reparsed token.
use super::Depth;
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug)]
enum Grammar {
    Literal,
    TwoFirst,
    TwoSecond,
    TwoChoice,
    FourFirst,
    FourSecond,
    FourChoice,
    EightFirst,
    Count {
        bits: u8,
        base: u16,
        colour: bool,
        end_on_zero: bool,
        minimum: u16,
    },
    Colour {
        length: u16,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Symbol {
    Run { length: u16, index: u8 },
    End,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Code {
    pub(crate) depth: Depth,
    grammar: Grammar,
}

impl Code {
    pub(crate) fn new(depth: Depth) -> Self {
        Self {
            depth,
            grammar: Grammar::Literal,
        }
    }

    pub(crate) fn width(self) -> u8 {
        match self.grammar {
            Grammar::Literal | Grammar::Colour { .. } => self.depth.bits(),
            Grammar::TwoFirst
            | Grammar::TwoSecond
            | Grammar::FourFirst
            | Grammar::FourSecond
            | Grammar::EightFirst => 1,
            Grammar::TwoChoice | Grammar::FourChoice => 2,
            Grammar::Count { bits, .. } => bits,
        }
    }

    fn run(&mut self, length: u16, index: u8) -> Option<Symbol> {
        self.grammar = Grammar::Literal;
        Some(Symbol::Run { length, index })
    }

    fn end(&mut self) -> Option<Symbol> {
        self.grammar = Grammar::Literal;
        Some(Symbol::End)
    }

    pub(crate) fn accept(&mut self, value: u8) -> Result<Option<Symbol>> {
        let symbol = match self.grammar {
            Grammar::Literal if value != 0 => self.run(1, value),
            Grammar::Literal => {
                self.grammar = match self.depth {
                    Depth::Two => Grammar::TwoFirst,
                    Depth::Four => Grammar::FourFirst,
                    Depth::Eight => Grammar::EightFirst,
                };
                None
            }
            Grammar::TwoFirst => {
                self.grammar = if value == 1 {
                    Grammar::Count {
                        bits: 3,
                        base: 3,
                        colour: true,
                        end_on_zero: false,
                        minimum: 0,
                    }
                } else {
                    Grammar::TwoSecond
                };
                None
            }
            Grammar::TwoSecond if value == 1 => self.run(1, 0),
            Grammar::TwoSecond => {
                self.grammar = Grammar::TwoChoice;
                None
            }
            Grammar::TwoChoice => match value {
                0 => self.end(),
                1 => self.run(2, 0),
                2 | 3 => {
                    self.grammar = if value == 2 {
                        Grammar::Count {
                            bits: 4,
                            base: 12,
                            colour: true,
                            end_on_zero: false,
                            minimum: 0,
                        }
                    } else {
                        Grammar::Count {
                            bits: 8,
                            base: 29,
                            colour: true,
                            end_on_zero: false,
                            minimum: 0,
                        }
                    };
                    None
                }
                _ => return Err(Error::Invalid("DVB 2-bit grammar")),
            },
            Grammar::FourFirst => {
                self.grammar = if value == 0 {
                    Grammar::Count {
                        bits: 3,
                        base: 2,
                        colour: false,
                        end_on_zero: true,
                        minimum: 0,
                    }
                } else {
                    Grammar::FourSecond
                };
                None
            }
            Grammar::FourSecond => {
                self.grammar = if value == 0 {
                    Grammar::Count {
                        bits: 2,
                        base: 4,
                        colour: true,
                        end_on_zero: false,
                        minimum: 0,
                    }
                } else {
                    Grammar::FourChoice
                };
                None
            }
            Grammar::FourChoice => match value {
                0 => self.run(1, 0),
                1 => self.run(2, 0),
                2 | 3 => {
                    self.grammar = if value == 2 {
                        Grammar::Count {
                            bits: 4,
                            base: 9,
                            colour: true,
                            end_on_zero: false,
                            minimum: 0,
                        }
                    } else {
                        Grammar::Count {
                            bits: 8,
                            base: 25,
                            colour: true,
                            end_on_zero: false,
                            minimum: 0,
                        }
                    };
                    None
                }
                _ => return Err(Error::Invalid("DVB 4-bit grammar")),
            },
            Grammar::EightFirst => {
                self.grammar = Grammar::Count {
                    bits: 7,
                    base: 0,
                    colour: value == 1,
                    end_on_zero: value == 0,
                    minimum: if value == 1 { 3 } else { 0 },
                };
                None
            }
            Grammar::Count {
                base,
                colour,
                end_on_zero,
                minimum,
                ..
            } => {
                if end_on_zero && value == 0 {
                    return Ok(self.end());
                }
                let length = base.checked_add(u16::from(value)).ok_or(Error::Overflow)?;
                if length < minimum {
                    return Err(Error::Invalid("DVB 8-bit explicit run below three"));
                }
                if colour {
                    self.grammar = Grammar::Colour { length };
                    None
                } else {
                    self.run(length, 0)
                }
            }
            Grammar::Colour { length } => self.run(length, value),
        };
        Ok(symbol)
    }
}
