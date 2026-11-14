//! The instruction set: the stack machine, its graphics state, and every
//! instruction, as the TrueType instruction set describes it (OpenType, "The
//! TrueType Instruction Set"; Apple's TrueType Reference Manual, chapter 5)
//! and as the reference runs it in backward compatibility mode (observed
//! rules, each naming its probe under harness/fonts/probes).

use super::math::{self, Round, Vector};
use super::zone::{ON_CURVE, TOUCHED_X, TOUCHED_Y, Zone};
use crate::outline::Point;
use triergon_core::{BudgetVec, Error, Result, WorkBudget};

/// The deepest nesting of function calls. Observed: 32 calls may be in
/// progress, and a 33rd ends the program (`call-depth`).
const MAX_CALL_DEPTH: usize = 32;

/// Where code lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Program {
    /// `fpgm`.
    Font,
    /// `prep`, the control value program.
    Control,
    /// A glyph's instructions.
    Glyph,
}

/// A run of instructions: a function or instruction definition's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Code {
    pub(super) program: Program,
    pub(super) start: u32,
    pub(super) end: u32,
}

/// Function definitions by number: any number may be defined, up to a
/// capacity of distinct numbers, which `maxp` sets (see `Profile::read`).
#[derive(Debug)]
pub(super) struct Functions {
    /// Numbers below the capacity, by number.
    numbered: BudgetVec<Option<Code>>,
    /// Larger numbers, in order.
    others: BudgetVec<(u32, Code)>,
    defined: usize,
    capacity: usize,
}

impl Functions {
    pub(super) fn new(budget: &triergon_core::Budget, capacity: usize) -> Result<Self> {
        Ok(Self {
            numbered: BudgetVec::try_filled(budget, None, capacity)?,
            others: BudgetVec::new(budget),
            defined: 0,
            capacity,
        })
    }

    fn get(&self, number: i32) -> Option<Code> {
        let number = number as u32;
        if let Some(code) = self.numbered.as_slice().get(number as usize) {
            return *code;
        }
        let others = self.others.as_slice();
        others
            .binary_search_by_key(&number, |&(n, _)| n)
            .ok()
            .map(|i| others[i].1)
    }

    /// Defines function `number`; `None` when that would define more
    /// numbers than the capacity, else how many definitions it moved.
    fn define(&mut self, number: i32, code: Code) -> Result<Option<usize>> {
        let number = number as u32;
        if let Some(slot) = self.numbered.as_mut_slice().get_mut(number as usize) {
            if slot.is_none() {
                if self.defined == self.capacity {
                    return Ok(None);
                }
                self.defined += 1;
            }
            *slot = Some(code);
            return Ok(Some(0));
        }
        let at = match self
            .others
            .as_slice()
            .binary_search_by_key(&number, |&(n, _)| n)
        {
            Ok(i) => {
                self.others.as_mut_slice()[i].1 = code;
                return Ok(Some(0));
            }
            Err(at) => at,
        };
        if self.defined == self.capacity {
            return Ok(None);
        }
        self.defined += 1;
        self.others.try_push((number, code)).map_err(|e| e.error)?;
        self.others.as_mut_slice()[at..].rotate_right(1);
        Ok(Some(self.others.len() - at))
    }
}

/// Instruction definitions by opcode. Observed: as many distinct opcodes may
/// be defined as `maxp` allows, a definition of an opcode already defined
/// replaces it, and a definition past the limit or of a number outside a
/// byte is an error (`idef-opcode`, `idef-opcode-none-allowed`,
/// `idef-opcode-wide`, `idef-opcode-negative`, `idef-limit-two`,
/// `idef-limit-same`).
#[derive(Debug)]
pub(super) struct Instructions {
    by_opcode: BudgetVec<Option<Code>>,
    defined: usize,
    capacity: usize,
}

impl Instructions {
    pub(super) fn new(budget: &triergon_core::Budget, capacity: u16) -> Result<Self> {
        Ok(Self {
            by_opcode: BudgetVec::try_filled(budget, None, 256)?,
            defined: 0,
            capacity: usize::from(capacity),
        })
    }

    fn get(&self, op: u8) -> Option<Code> {
        self.by_opcode.as_slice()[usize::from(op)]
    }

    /// Defines `number`'s instruction; `false` for a number outside a byte
    /// or past the capacity.
    fn define(&mut self, number: i32, code: Code) -> bool {
        let Some(slot) = u8::try_from(number)
            .ok()
            .map(|op| &mut self.by_opcode.as_mut_slice()[usize::from(op)])
        else {
            return false;
        };
        if slot.is_none() {
            if self.defined == self.capacity {
                return false;
            }
            self.defined += 1;
        }
        *slot = Some(code);
        true
    }
}

/// The graphics state (OpenType, "Graphics State Summary").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GraphicsState {
    pub(super) projection: Vector,
    pub(super) freedom: Vector,
    /// The dual projection vector, set by SDPVTL until the projection vector
    /// is next set.
    pub(super) dual: Option<Vector>,
    pub(super) rp: [usize; 3],
    pub(super) zp: [usize; 3],
    pub(super) loop_count: i32,
    pub(super) round: Round,
    pub(super) minimum_distance: i32,
    pub(super) cut_in: i32,
    pub(super) single_width: i32,
    pub(super) single_width_cut_in: i32,
    pub(super) delta_base: i32,
    pub(super) delta_shift: i32,
    pub(super) auto_flip: bool,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self {
            projection: Vector::X,
            freedom: Vector::X,
            dual: None,
            rp: [0; 3],
            zp: [1; 3],
            loop_count: 1,
            round: Round::Grid,
            minimum_distance: 64,
            cut_in: 68,
            single_width: 0,
            single_width_cut_in: 0,
            delta_base: 9,
            delta_shift: 3,
            auto_flip: true,
        }
    }
}

impl GraphicsState {
    /// The state a glyph program starts in after the control value program
    /// left `prep`: observed, glyphs inherit the control program's minimum
    /// distance, cut-ins, single width, delta base and shift and auto flip,
    /// and start from the defaults for the rest (`prep-state-*`).
    pub(super) fn for_glyph(prep: &Self) -> Self {
        Self {
            minimum_distance: prep.minimum_distance,
            cut_in: prep.cut_in,
            single_width: prep.single_width,
            single_width_cut_in: prep.single_width_cut_in,
            delta_base: prep.delta_base,
            delta_shift: prep.delta_shift,
            auto_flip: prep.auto_flip,
            ..Self::default()
        }
    }
}

/// The size the programs run at.
#[derive(Clone, Copy, Debug)]
pub(super) struct Size {
    pub(super) ppem: u16,
    /// 16.16, font units to 26.6.
    pub(super) scale: i64,
    /// Whether GETINFO reports symmetric rendering.
    pub(super) symmetric: bool,
}

/// Why a program stopped early: an error in the program, which ends it with
/// what it did so far, or a limit of the caller's, which fails the load.
#[derive(Debug)]
pub(super) enum Stop {
    Program,
    Limit(Error),
}

impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Self::Limit(e)
    }
}

type Run<T = ()> = std::result::Result<T, Stop>;

/// A call in progress: where to return, and how many more times to run the
/// body of a LOOPCALL.
#[derive(Clone, Copy, Debug)]
struct Frame {
    program: Program,
    ip: usize,
    end: usize,
    body: Code,
    remaining: i32,
}

/// Everything a program runs against.
pub(super) struct Machine<'a> {
    pub(super) code: [&'a [u8]; 3],
    pub(super) stack: &'a mut BudgetVec<i32>,
    pub(super) stack_limit: usize,
    pub(super) cvt: &'a mut [i32],
    pub(super) storage: &'a mut [i32],
    pub(super) functions: &'a mut Functions,
    pub(super) instructions: &'a mut Instructions,
    pub(super) twilight: &'a mut Zone,
    pub(super) glyph: &'a mut Zone,
    pub(super) state: GraphicsState,
    pub(super) size: Size,
    pub(super) work: &'a mut WorkBudget,
    /// The program running at the top level.
    pub(super) program: Program,
    /// INSTCTRL's flags, set by the control value program.
    pub(super) instruct_control: u8,
    /// Whether IUP has run in x and in y: once both have, glyph points no
    /// longer move (observed, `backward-compatibility`).
    pub(super) interpolated: [bool; 2],
}

impl Machine<'_> {
    /// Runs `program` from the start, and returns whether it ran to its end:
    /// an error in it ends it early, keeping what it did (observed,
    /// `errors`).
    pub(super) fn run(&mut self, program: Program) -> Result<bool> {
        self.program = program;
        let end = self.code[program as usize].len();
        match self.execute(program, 0, end) {
            Ok(()) => Ok(true),
            Err(Stop::Program) => Ok(false),
            Err(Stop::Limit(e)) => Err(e),
        }
    }

    fn execute(&mut self, program: Program, mut ip: usize, mut end: usize) -> Run {
        let mut program = program;
        let mut calls: [Option<Frame>; MAX_CALL_DEPTH] = [None; MAX_CALL_DEPTH];
        let mut depth: usize = 0;
        loop {
            if ip >= end {
                if depth == 0 {
                    return Ok(());
                }
                // A function without ENDF ends with its program.
                return Err(Stop::Program);
            }
            self.charge(1)?;
            let code = self.code[program as usize];
            let op = code[ip];
            let mut next = ip + 1;
            // Observed: an instruction short of arguments takes what the
            // stack holds and runs with zeros (`stack-underflow`).
            let needed = arity(op);
            if self.stack.len() < needed {
                self.stack.clear();
                for _ in 0..needed {
                    self.push_value(0)?;
                }
            }
            match op {
                0x40 | 0x41 | 0xB0..=0xBF => next = self.push(code, ip)?,
                0x1B => next = self.skip(code, next, end, Skip::Eif)?,
                0x1C | 0x78 | 0x79 => {
                    let (offset, jump) = if op == 0x1C {
                        (self.pop(), true)
                    } else {
                        let e = self.pop();
                        (self.pop(), (e != 0) == (op == 0x78))
                    };
                    if jump {
                        let target = ip as i64 + i64::from(offset);
                        if target < 0 || target > end as i64 {
                            return Err(Stop::Program);
                        }
                        next = target as usize;
                    }
                }
                0x58 => {
                    if self.pop() == 0 {
                        next = self.skip(code, next, end, Skip::ElseOrEif)?;
                    }
                }
                0x59 => {}
                0x2C | 0x89 => next = self.define(op, program, code, next, end)?,
                0x2D => {
                    let Some(frame) = depth.checked_sub(1).and_then(|d| calls[d].as_mut()) else {
                        return Err(Stop::Program);
                    };
                    if frame.remaining > 1 {
                        frame.remaining -= 1;
                        next = frame.body.start as usize;
                    } else {
                        let frame = *frame;
                        depth -= 1;
                        program = frame.program;
                        next = frame.ip;
                        end = frame.end;
                    }
                }
                0x2A | 0x2B => {
                    let f = self.pop();
                    let count = if op == 0x2A { self.pop() } else { 1 };
                    let body = self.functions.get(f).ok_or(Stop::Program)?;
                    if count > 0 {
                        if depth == MAX_CALL_DEPTH {
                            return Err(Stop::Program);
                        }
                        calls[depth] = Some(Frame {
                            program,
                            ip: next,
                            end,
                            body,
                            remaining: count,
                        });
                        depth += 1;
                        program = body.program;
                        next = body.start as usize;
                        end = body.end as usize;
                    }
                }
                _ => {
                    if let Some(body) = self.defined_instruction(op) {
                        if depth == MAX_CALL_DEPTH {
                            return Err(Stop::Program);
                        }
                        calls[depth] = Some(Frame {
                            program,
                            ip: next,
                            end,
                            body,
                            remaining: 1,
                        });
                        depth += 1;
                        program = body.program;
                        next = body.start as usize;
                        end = body.end as usize;
                    } else {
                        self.instruction(op)?;
                    }
                }
            }
            ip = next;
        }
    }

    /// Charges `units` against the work budget: one for each instruction,
    /// and one for each value, point, flag or byte of code an instruction
    /// goes through.
    fn charge(&mut self, units: usize) -> Run {
        if self.work.try_charge(units as u64) {
            Ok(())
        } else {
            Err(Stop::Limit(Error::Policy("TrueType instructions")))
        }
    }

    /// The position after the end of the skipped run at `ip` that `target`
    /// names, charging a unit per byte skipped; an error if the code ends
    /// first.
    fn skip(&mut self, code: &[u8], ip: usize, end: usize, target: Skip) -> Run<usize> {
        let next = skip(code, ip, end, target);
        self.charge(next.unwrap_or(end).saturating_sub(ip))?;
        next.ok_or(Stop::Program)
    }

    /// The instruction definition for `op`, where the instruction set leaves
    /// it undefined.
    fn defined_instruction(&self, op: u8) -> Option<Code> {
        if !is_undefined(op) {
            return None;
        }
        self.instructions.get(op)
    }

    /// FDEF or IDEF at `ip`: records the body up to its ENDF and returns the
    /// position after it.
    fn define(
        &mut self,
        op: u8,
        program: Program,
        code: &[u8],
        ip: usize,
        end: usize,
    ) -> Run<usize> {
        if program == Program::Glyph || self.program == Program::Glyph {
            return Err(Stop::Program);
        }
        let id = self.pop();
        let close = self.skip(code, ip, end, Skip::Endf)?;
        let body = Code {
            program,
            start: ip as u32,
            end: close as u32,
        };
        if op == 0x2C {
            let moved = self.functions.define(id, body)?.ok_or(Stop::Program)?;
            self.charge(moved)?;
        } else if !self.instructions.define(id, body) {
            return Err(Stop::Program);
        }
        Ok(close)
    }

    fn push(&mut self, code: &[u8], ip: usize) -> Run<usize> {
        let op = code[ip];
        let (count, words, data) = match op {
            0x40 | 0x41 => (
                usize::from(*code.get(ip + 1).ok_or(Stop::Program)?),
                op == 0x41,
                ip + 2,
            ),
            0xB0..=0xB7 => (usize::from(op - 0xAF), false, ip + 1),
            _ => (usize::from(op - 0xB7), true, ip + 1),
        };
        self.charge(count)?;
        let size = if words { 2 } else { 1 };
        let bytes = code.get(data..data + count * size).ok_or(Stop::Program)?;
        for chunk in bytes.chunks_exact(size) {
            let v = if words {
                i32::from(i16::from_be_bytes([chunk[0], chunk[1]]))
            } else {
                i32::from(chunk[0])
            };
            self.push_value(v)?;
        }
        Ok(data + count * size)
    }

    fn push_value(&mut self, v: i32) -> Run {
        if self.stack.len() >= self.stack_limit {
            return Err(Stop::Program);
        }
        self.stack.try_push(v).map_err(|e| Stop::Limit(e.error))
    }

    fn pop(&mut self) -> i32 {
        self.stack.pop().unwrap_or(0)
    }

    /// A point of the zone `zone` points to; `None` for one outside it, which
    /// the instruction then leaves alone (observed, `stack-underflow`).
    fn pop_point(&mut self, zone: usize) -> Option<usize> {
        let p = self.pop();
        usize::try_from(p)
            .ok()
            .filter(|&p| p < self.zone(zone).len())
    }

    fn zone(&self, zone: usize) -> &Zone {
        if self.state.zp[zone] == 0 {
            self.twilight
        } else {
            self.glyph
        }
    }

    fn zone_mut(&mut self, zone: usize) -> &mut Zone {
        if self.state.zp[zone] == 0 {
            self.twilight
        } else {
            self.glyph
        }
    }

    fn current(&self, zone: usize, p: usize) -> Point {
        self.zone(zone).current.as_slice()[p]
    }

    fn original(&self, zone: usize, p: usize) -> Point {
        self.zone(zone).original.as_slice()[p]
    }

    /// The projection of the current position of `a` relative to `b`.
    fn distance(&self, za: usize, a: usize, zb: usize, b: usize) -> i32 {
        let (pa, pb) = (self.current(za, a), self.current(zb, b));
        self.state
            .projection
            .project(pa.x.wrapping_sub(pb.x), pa.y.wrapping_sub(pb.y))
    }

    /// The distance of `a` from `b` in the original outline, along the dual
    /// projection vector where one is set.
    ///
    /// Observed: between glyph points it is measured in font units, then
    /// scaled by the scale the points load at (`projections`,
    /// `original-distances`).
    fn original_distance(&self, za: usize, a: usize, zb: usize, b: usize) -> i32 {
        let v = self.state.dual.unwrap_or(self.state.projection);
        if self.state.zp[za] != 0 && self.state.zp[zb] != 0 && self.glyph.font_units {
            let (ua, ub) = (
                self.glyph.units.as_slice()[a],
                self.glyph.units.as_slice()[b],
            );
            let d = v.project(ua.x.wrapping_sub(ub.x), ua.y.wrapping_sub(ub.y));
            return math::scale(d, self.size.scale);
        }
        let (pa, pb) = (self.original(za, a), self.original(zb, b));
        v.project(pa.x.wrapping_sub(pb.x), pa.y.wrapping_sub(pb.y))
    }

    fn coordinate(&self, zone: usize, p: usize) -> i32 {
        let c = self.current(zone, p);
        self.state.projection.project(c.x, c.y)
    }

    /// Moves point `p` of the zone `zone` points to along the freedom vector
    /// until its projection has changed by `distance`, and touches it.
    fn move_point(&mut self, zone: usize, p: usize, distance: i32, touch: bool) {
        if let Some((_, dy)) = self.displacement(distance) {
            self.shift_point(zone, p, dy, touch);
        }
    }

    /// The move along the freedom vector that changes a projection by
    /// `distance`; `None` where the vectors are perpendicular.
    fn displacement(&self, distance: i32) -> Option<(i32, i32)> {
        let (f, pv) = (self.state.freedom, self.state.projection);
        if f == pv && (f == Vector::X || f == Vector::Y) {
            return Some((
                if f == Vector::X { distance } else { 0 },
                if f == Vector::Y { distance } else { 0 },
            ));
        }
        let dot = f.dot(pv);
        (dot != 0).then(|| {
            (
                math::mul_div(distance, f.x, dot),
                math::mul_div(distance, f.y, dot),
            )
        })
    }

    /// Moves point `p` by `dy` in y, touching it in the directions the
    /// freedom vector has. Observed in backward compatibility mode: no point
    /// moves in x, in the glyph zone or the twilight zone, and no glyph point
    /// moves once IUP has run in both directions (`backward-compatibility`,
    /// `twilight-moves`, `twilight-moves-more`); the instructions that place
    /// twilight points set their x.
    fn shift_point(&mut self, zone: usize, p: usize, dy: i32, touch: bool) {
        let glyph = self.state.zp[zone] != 0;
        let frozen = glyph && self.interpolated == [true; 2];
        let z = self.zone_mut(zone);
        let point = &mut z.current.as_mut_slice()[p];
        if !frozen {
            point.y = point.y.wrapping_add(dy);
        }
        if touch {
            self.touch(zone, p);
        }
    }

    /// Touches point `p` in the directions the freedom vector has.
    fn touch(&mut self, zone: usize, p: usize) {
        let (fx, fy) = (self.state.freedom.x != 0, self.state.freedom.y != 0);
        let flags = &mut self.zone_mut(zone).flags.as_mut_slice()[p];
        if fx {
            *flags |= TOUCHED_X;
        }
        if fy {
            *flags |= TOUCHED_Y;
        }
    }

    /// Moves `p` so its projection relative to the reference point is
    /// `target`.
    fn move_to_distance(&mut self, zp: usize, p: usize, zr: usize, r: usize, target: i32) {
        let current = self.distance(zp, p, zr, r);
        self.move_point(zp, p, target.wrapping_sub(current), true);
    }

    fn set_vector(&mut self, projection: bool, v: Vector) {
        if projection {
            self.state.projection = v;
            self.state.dual = None;
        } else {
            self.state.freedom = v;
        }
    }

    /// The two points of a line, as SPVTL, SFVTL and SDPVTL pop them: the
    /// first in the zone zp2 points to, the second in zp1's (OpenType, "The
    /// TrueType Instruction Set").
    fn pop_line(&mut self) -> Option<(usize, usize)> {
        let first = self.pop_point(2);
        let second = self.pop_point(1);
        Some((first?, second?))
    }

    /// The unit vector along a line, from the original or the current
    /// positions. Observed: it runs from the first point popped to the
    /// second, and `perpendicular` turns it a quarter counter-clockwise; a
    /// line of zero length gives the x axis either way (`vectors`).
    fn line_vector(
        &self,
        (first, second): (usize, usize),
        perpendicular: bool,
        original: bool,
    ) -> Vector {
        let (a, b) = if original {
            (self.original(2, first), self.original(1, second))
        } else {
            (self.current(2, first), self.current(1, second))
        };
        match Vector::along(b.x.wrapping_sub(a.x), b.y.wrapping_sub(a.y)) {
            Some(v) if perpendicular => v.perpendicular(),
            Some(v) => v,
            None => Vector::X,
        }
    }

    /// The vector SPVFS and SFVFS set: the stack's two 2.14 values,
    /// normalized; `None` for a zero vector, which leaves the vector as it
    /// was (observed, `vectors-from-stack`).
    fn stack_vector(&mut self) -> Option<Vector> {
        let y = self.pop();
        let x = self.pop();
        Vector::along(i32::from(x as i16), i32::from(y as i16))
    }

    fn pop_zone(&mut self) -> Option<usize> {
        let zone = self.pop();
        (0..=1).contains(&zone).then_some(zone as usize)
    }

    fn arithmetic(&mut self, op: u8) -> Run {
        let b = self.pop();
        let a = self.pop();
        let v = match op {
            0x50 => i32::from(a < b),
            0x51 => i32::from(a <= b),
            0x52 => i32::from(a > b),
            0x53 => i32::from(a >= b),
            0x54 => i32::from(a == b),
            0x55 => i32::from(a != b),
            0x5A => i32::from(a != 0 && b != 0),
            0x5B => i32::from(a != 0 || b != 0),
            0x60 => a.wrapping_add(b),
            0x61 => a.wrapping_sub(b),
            0x62 => math::div(a, b).ok_or(Stop::Program)?,
            0x63 => math::mul(a, b),
            0x8B => a.max(b),
            _ => a.min(b),
        };
        self.push_value(v)
    }

    /// A DELTA instruction's amount for `arg` at this size, if it applies:
    /// `range` is 0, 16 or 32 for DELTA*1, *2 and *3.
    fn delta(&self, arg: i32, range: i32) -> Option<i32> {
        let ppem = self.state.delta_base + range + ((arg >> 4) & 15);
        if ppem != i32::from(self.size.ppem) {
            return None;
        }
        let steps = (arg & 15) - 8;
        let steps = if steps >= 0 { steps + 1 } else { steps };
        Some((steps * 64) >> self.state.delta_shift)
    }

    fn instruction(&mut self, op: u8) -> Run {
        match op {
            0x00..=0x05 => {
                let v = if op & 1 == 0 { Vector::Y } else { Vector::X };
                // SVTCA sets both vectors, SPVTCA the projection vector and
                // SFVTCA the freedom vector.
                if op < 0x04 {
                    self.set_vector(true, v);
                }
                if !(0x02..0x04).contains(&op) {
                    self.set_vector(false, v);
                }
            }
            0x06..=0x09 => {
                if let Some(line) = self.pop_line() {
                    let v = self.line_vector(line, op & 1 == 1, false);
                    self.set_vector(op < 0x08, v);
                }
            }
            0x86 | 0x87 => {
                if let Some(line) = self.pop_line() {
                    self.state.dual = Some(self.line_vector(line, op & 1 == 1, true));
                    self.state.projection = self.line_vector(line, op & 1 == 1, false);
                }
            }
            0x0A | 0x0B => {
                if let Some(v) = self.stack_vector() {
                    self.set_vector(op == 0x0A, v);
                }
            }
            0x0C | 0x0D => {
                let v = if op == 0x0C {
                    self.state.projection
                } else {
                    self.state.freedom
                };
                self.push_value(v.x)?;
                self.push_value(v.y)?;
            }
            0x0E => self.state.freedom = self.state.projection,
            0x0F => self.intersect(),
            0x10..=0x12 => self.state.rp[usize::from(op - 0x10)] = self.pop().max(0) as usize,
            0x13..=0x16 => {
                // Observed: a zone other than 0 or 1 is ignored
                // (`stack-underflow`).
                if let Some(zone) = self.pop_zone() {
                    match op {
                        0x16 => self.state.zp = [zone; 3],
                        _ => self.state.zp[usize::from(op - 0x13)] = zone,
                    }
                }
            }
            0x17 => {
                // Observed: a negative loop count ends the program
                // (`stack-underflow`).
                let n = self.pop();
                if n < 0 {
                    return Err(Stop::Program);
                }
                self.state.loop_count = n;
            }
            0x18 => self.state.round = Round::Grid,
            0x19 => self.state.round = Round::HalfGrid,
            0x1A => self.state.minimum_distance = self.pop(),
            0x1D => self.state.cut_in = self.pop(),
            0x1E => self.state.single_width_cut_in = self.pop(),
            0x1F => {
                let units = self.pop();
                self.state.single_width = math::scale(units, self.size.scale);
            }
            0x20 => {
                let v = self.pop();
                self.push_value(v)?;
                self.push_value(v)?;
            }
            0x21 | 0x4F | 0x7E | 0x7F | 0x85 | 0x8D => {
                self.pop();
            }
            0x22 => self.stack.clear(),
            0x23 => {
                let b = self.pop();
                let a = self.pop();
                self.push_value(b)?;
                self.push_value(a)?;
            }
            0x24 => {
                let depth = self.stack.len() as i32;
                self.push_value(depth)?;
            }
            0x25 | 0x26 => {
                // Observed: an index outside the stack has CINDEX push zero
                // and MINDEX do nothing (`stack-underflow`).
                let k = self.pop();
                let len = self.stack.len();
                let index = usize::try_from(k)
                    .ok()
                    .filter(|&k| k >= 1 && k <= len)
                    .map(|k| len - k);
                match (index, op) {
                    (Some(index), 0x26) => {
                        self.charge(len - index)?;
                        self.stack.as_mut_slice()[index..].rotate_left(1);
                    }
                    (Some(index), _) => {
                        let v = self.stack.as_slice()[index];
                        self.push_value(v)?;
                    }
                    (None, 0x26) => {}
                    (None, _) => self.push_value(0)?,
                }
            }
            0x8A => {
                let a = self.pop();
                let b = self.pop();
                let c = self.pop();
                self.push_value(b)?;
                self.push_value(a)?;
                self.push_value(c)?;
            }
            0x27 => self.align_points(),
            0x29 => {
                if let Some(p) = self.pop_point(0) {
                    let mut clear = 0;
                    if self.state.freedom.x != 0 {
                        clear |= TOUCHED_X;
                    }
                    if self.state.freedom.y != 0 {
                        clear |= TOUCHED_Y;
                    }
                    self.zone_mut(0).flags.as_mut_slice()[p] &= !clear;
                }
            }
            0x2E | 0x2F => {
                if let Some(p) = self.pop_point(0) {
                    let distance = if op == 0x2F {
                        let c = self.coordinate(0, p);
                        self.state.round.apply(c).wrapping_sub(c)
                    } else {
                        0
                    };
                    self.move_point(0, p, distance, true);
                    self.state.rp[0] = p;
                    self.state.rp[1] = p;
                }
            }
            0x30 | 0x31 => self.interpolate_untouched(op == 0x31)?,
            0x32..=0x37 => self.shift(op)?,
            0x38 => self.shift_by_pixels()?,
            0x39 => self.interpolate()?,
            0x3A | 0x3B => {
                let d = self.pop();
                if let Some(p) = self.pop_point(1) {
                    let r = self.state.rp[0];
                    if r < self.zone(0).len() {
                        if self.state.zp[1] == 0 {
                            // Observed: MSIRP places a twilight point, original
                            // and current, at rp0's original position moved by
                            // the distance along the freedom vector, x
                            // included, or at rp0's original position where
                            // the vectors are perpendicular; the move below
                            // then measures from rp0's current position
                            // (`twilight-relative`, `twilight-moves`,
                            // `twilight-msirp-perpendicular`).
                            let o = self.original(0, r);
                            let (dx, dy) = self.displacement(d).unwrap_or_default();
                            let at = Point::new(o.x.wrapping_add(dx), o.y.wrapping_add(dy));
                            let z = self.zone_mut(1);
                            z.original.as_mut_slice()[p] = at;
                            z.current.as_mut_slice()[p] = at;
                        }
                        self.move_to_distance(1, p, 0, r, d);
                        self.state.rp[1] = r;
                        self.state.rp[2] = p;
                        if op == 0x3B {
                            self.state.rp[0] = p;
                        }
                    }
                }
            }
            0x3C => self.align_to_reference()?,
            0x3D => self.state.round = Round::DoubleGrid,
            0x3E | 0x3F => self.move_indirect_absolute(op == 0x3F),
            0x42 => {
                let v = self.pop();
                let i = self.pop();
                if let Some(slot) = usize::try_from(i)
                    .ok()
                    .and_then(|i| self.storage.get_mut(i))
                {
                    *slot = v;
                }
            }
            0x43 => {
                let i = self.pop();
                let v = usize::try_from(i)
                    .ok()
                    .and_then(|i| self.storage.get(i).copied())
                    .unwrap_or(0);
                self.push_value(v)?;
            }
            0x44 | 0x70 => {
                let v = self.pop();
                let i = self.pop();
                let v = if op == 0x70 {
                    math::scale(v, self.size.scale)
                } else {
                    v
                };
                if let Some(slot) = usize::try_from(i).ok().and_then(|i| self.cvt.get_mut(i)) {
                    *slot = v;
                }
            }
            0x45 => {
                let i = self.pop();
                let v = self.control_value(i);
                self.push_value(v)?;
            }
            0x46 | 0x47 => {
                let v = match self.pop_point(2) {
                    Some(p) if op == 0x47 => {
                        let o = self.original(2, p);
                        self.state
                            .dual
                            .unwrap_or(self.state.projection)
                            .project(o.x, o.y)
                    }
                    Some(p) => self.coordinate(2, p),
                    None => 0,
                };
                self.push_value(v)?;
            }
            0x48 => {
                let c = self.pop();
                if let Some(p) = self.pop_point(2) {
                    let current = self.coordinate(2, p);
                    self.move_point(2, p, c.wrapping_sub(current), true);
                    // Observed: in the twilight zone SCFS moves the original
                    // position too (`twilight-moves`).
                    if self.state.zp[2] == 0 {
                        let z = self.zone_mut(2);
                        z.original.as_mut_slice()[p] = z.current.as_slice()[p];
                    }
                }
            }
            0x49 | 0x4A => {
                let p2 = self.pop_point(1);
                let p1 = self.pop_point(0);
                let d = match (p1, p2) {
                    (Some(p1), Some(p2)) if op == 0x4A => self.original_distance(0, p1, 1, p2),
                    (Some(p1), Some(p2)) => self.distance(0, p1, 1, p2),
                    _ => 0,
                };
                self.push_value(d)?;
            }
            0x4B => self.push_value(i32::from(self.size.ppem))?,
            0x4C => self.push_value(i32::from(self.size.ppem) << 6)?,
            0x4D => self.state.auto_flip = true,
            0x4E => self.state.auto_flip = false,
            0x50..=0x55 | 0x5A | 0x5B | 0x60..=0x63 | 0x8B | 0x8C => self.arithmetic(op)?,
            0x56 | 0x57 => {
                let v = self.pop();
                let odd = (self.state.round.apply(v) >> 6) & 1 == 1;
                self.push_value(i32::from(odd == (op == 0x56)))?;
            }
            0x5C => {
                let v = self.pop();
                self.push_value(i32::from(v == 0))?;
            }
            0x5D | 0x71 | 0x72 => self.delta_points(match op {
                0x5D => 0,
                0x71 => 16,
                _ => 32,
            })?,
            0x73..=0x75 => {
                let range = i32::from(op - 0x73) * 16;
                let n = self.pop();
                for _ in 0..n.max(0) {
                    if self.stack.len() < 2 {
                        break;
                    }
                    let c = self.pop();
                    let arg = self.pop();
                    let amount = self.delta(arg, range);
                    let slot = usize::try_from(c).ok().and_then(|c| self.cvt.get_mut(c));
                    if let (Some(amount), Some(slot)) = (amount, slot) {
                        *slot = slot.wrapping_add(amount);
                    }
                    self.charge(1)?;
                }
            }
            // Observed: the delta base keeps the low 16 bits of SDB's value,
            // and an SDS outside 0 to 6 is an error (`delta-base-extremes`,
            // `delta-base-wide`, `delta-shift`).
            0x5E => self.state.delta_base = i32::from(self.pop() as u16),
            0x5F => {
                let shift = self.pop();
                if !(0..=6).contains(&shift) {
                    return Err(Stop::Program);
                }
                self.state.delta_shift = shift;
            }
            0x64 => {
                let v = self.pop();
                self.push_value(v.wrapping_abs())?;
            }
            0x65 => {
                let v = self.pop();
                self.push_value(v.wrapping_neg())?;
            }
            0x66 => {
                let v = self.pop();
                self.push_value(v & !63)?;
            }
            0x67 => {
                let v = self.pop();
                self.push_value(v.wrapping_add(63) & !63)?;
            }
            0x68..=0x6B => {
                let v = self.pop();
                self.push_value(self.state.round.apply(v))?;
            }
            0x6C..=0x6F => {}
            0x76 | 0x77 => {
                let n = self.pop();
                self.state.round = Round::custom(n, op == 0x77);
            }
            0x7A => self.state.round = Round::Off,
            0x7C => self.state.round = Round::UpToGrid,
            0x7D => self.state.round = Round::DownToGrid,
            0x80 => {
                let count = self.looped(0)?.unwrap_or(0);
                for _ in 0..count {
                    if let Some(p) = self.pop_point(0) {
                        self.zone_mut(0).flags.as_mut_slice()[p] ^= ON_CURVE;
                    }
                }
            }
            0x81 | 0x82 => {
                let high = self.pop();
                let low = self.pop();
                let len = self.zone(0).len();
                let (Ok(low), Ok(high)) = (usize::try_from(low), usize::try_from(high)) else {
                    return Ok(());
                };
                if low <= high && high < len {
                    self.charge(high - low + 1)?;
                    for f in &mut self.zone_mut(0).flags.as_mut_slice()[low..=high] {
                        if op == 0x81 {
                            *f |= ON_CURVE;
                        } else {
                            *f &= !ON_CURVE;
                        }
                    }
                }
            }
            0x88 => {
                let selector = self.pop();
                self.push_value(get_info(selector, self.size.symmetric))?;
            }
            0x8E => {
                let selector = self.pop();
                let value = self.pop();
                if self.program == Program::Control && (1..=2).contains(&selector) {
                    let bit = 1 << (selector - 1);
                    if value & bit != 0 {
                        self.instruct_control |= bit as u8;
                    } else {
                        self.instruct_control &= !(bit as u8);
                    }
                }
            }
            0xC0..=0xDF => self.move_direct_relative(op),
            0xE0..=0xFF => self.move_indirect_relative(op),
            _ => return Err(Stop::Program),
        }
        Ok(())
    }

    fn control_value(&self, i: i32) -> i32 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.cvt.get(i).copied())
            .unwrap_or(0)
    }

    /// The loop variable, reset to one for the next instruction, if the
    /// stack holds that many values and `extra` more; observed, a looped
    /// instruction short of them is skipped (`stack-underflow`).
    fn looped(&mut self, extra: usize) -> Run<Option<usize>> {
        let count = std::mem::replace(&mut self.state.loop_count, 1).max(0) as usize;
        if self.stack.len() < count + extra {
            return Ok(None);
        }
        self.charge(count)?;
        Ok(Some(count))
    }

    fn intersect(&mut self) {
        let b1 = self.pop_point(0);
        let b0 = self.pop_point(0);
        let a1 = self.pop_point(1);
        let a0 = self.pop_point(1);
        let p = self.pop_point(2);
        let (Some(b1), Some(b0), Some(a1), Some(a0), Some(p)) = (b1, b0, a1, a0, p) else {
            return;
        };
        let target = intersection(
            [self.current(1, a0), self.current(1, a1)],
            [self.current(0, b0), self.current(0, b1)],
        );
        // Observed: ISECT moves glyph points in x too, and after IUP in both
        // directions (`intersections`).
        let z = self.zone_mut(2);
        z.current.as_mut_slice()[p] = target;
        z.flags.as_mut_slice()[p] |= TOUCHED_X | TOUCHED_Y;
    }

    fn align_points(&mut self) {
        let p2 = self.pop_point(0);
        let p1 = self.pop_point(1);
        if let (Some(p1), Some(p2)) = (p1, p2) {
            let d = self.distance(0, p2, 1, p1) / 2;
            self.move_point(1, p1, d, true);
            self.move_point(0, p2, -d, true);
        }
    }

    fn align_to_reference(&mut self) -> Run {
        let r = self.state.rp[0];
        let Some(count) = self.looped(0)? else {
            return Ok(());
        };
        let valid = r < self.zone(0).len();
        for _ in 0..count {
            if let Some(p) = self.pop_point(1).filter(|_| valid) {
                let d = self.distance(1, p, 0, r);
                self.move_point(1, p, d.wrapping_neg(), true);
            }
        }
        Ok(())
    }

    fn move_indirect_absolute(&mut self, round: bool) {
        let n = self.pop();
        let Some(p) = self.pop_point(0) else {
            return;
        };
        let mut distance = self.control_value(n);
        if self.state.zp[0] == 0 {
            // Observed: MIAP places a twilight point at the control value
            // along the projection vector (`twilight`).
            let v = self.state.projection;
            let at = Point::new(
                math::mul_div(distance, v.x, 0x4000),
                math::mul_div(distance, v.y, 0x4000),
            );
            let z = self.zone_mut(0);
            z.original.as_mut_slice()[p] = at;
            z.current.as_mut_slice()[p] = at;
        }
        let current = self.coordinate(0, p);
        if round {
            if (distance.wrapping_sub(current)).wrapping_abs() > self.state.cut_in {
                distance = current;
            }
            distance = self.state.round.apply(distance);
        }
        self.move_point(0, p, distance.wrapping_sub(current), true);
        self.state.rp[0] = p;
        self.state.rp[1] = p;
    }

    fn move_direct_relative(&mut self, op: u8) {
        let Some(p) = self.pop_point(1) else {
            return;
        };
        let r = self.state.rp[0];
        if r >= self.zone(0).len() {
            return;
        }
        let mut d = self.original_distance(1, p, 0, r);
        // Observed: the cut-in compares the signed distance with the single
        // width as set, and a distance within it becomes the single width,
        // negated for a negative distance: a negative distance snaps only to
        // a negative single width, and then turns positive
        // (`single-width-signed`).
        let (sw, swci) = (self.state.single_width, self.state.single_width_cut_in);
        if (d.wrapping_sub(sw)).wrapping_abs() < swci {
            d = if d >= 0 { sw } else { sw.wrapping_neg() };
        }
        let d = self.finish_distance(op, d, d);
        self.move_to_distance(1, p, 0, r, d);
        self.set_relative_references(op, r, p);
    }

    fn move_indirect_relative(&mut self, op: u8) {
        let n = self.pop();
        let Some(p) = self.pop_point(1) else {
            return;
        };
        let r = self.state.rp[0];
        if r >= self.zone(0).len() {
            return;
        }
        let mut cvt = self.control_value(n);
        // Observed: as in MDRP, the signed control value, before the auto
        // flip (`single-width-signed`).
        let (sw, swci) = (self.state.single_width, self.state.single_width_cut_in);
        if (cvt.wrapping_sub(sw)).wrapping_abs() < swci {
            cvt = if cvt >= 0 { sw } else { sw.wrapping_neg() };
        }
        if self.state.zp[1] == 0 {
            // Observed: a twilight point starts at rp0's original position
            // plus the control value along the freedom vector, x included,
            // then moves in y only (`twilight-relative`, `twilight-moves`).
            let o = self.original(0, r);
            let f = self.state.freedom;
            let at = Point::new(
                o.x.wrapping_add(math::mul_div(cvt, f.x, 0x4000)),
                o.y.wrapping_add(math::mul_div(cvt, f.y, 0x4000)),
            );
            let z = self.zone_mut(1);
            z.original.as_mut_slice()[p] = at;
            z.current.as_mut_slice()[p] = at;
        }
        // Observed: unlike MDRP, MIRP measures the original distance between
        // the scaled original positions (`original-distances-mirp`).
        let original = {
            let (pa, pb) = (self.original(1, p), self.original(0, r));
            let v = self.state.dual.unwrap_or(self.state.projection);
            v.project(pa.x.wrapping_sub(pb.x), pa.y.wrapping_sub(pb.y))
        };
        if self.state.auto_flip && (original ^ cvt) < 0 {
            cvt = cvt.wrapping_neg();
        }
        if op & 4 != 0 && (cvt.wrapping_sub(original)).wrapping_abs() > self.state.cut_in {
            cvt = original;
        }
        let d = self.finish_distance(op, cvt, original);
        self.move_to_distance(1, p, 0, r, d);
        self.set_relative_references(op, r, p);
    }

    /// Rounds a relative move's distance when the flags ask for it and keeps
    /// it at least the minimum distance, in the direction of `original`.
    fn finish_distance(&self, op: u8, d: i32, original: i32) -> i32 {
        let mut d = if op & 4 != 0 {
            self.state.round.apply(d)
        } else {
            d
        };
        if op & 8 != 0 {
            let minimum = self.state.minimum_distance;
            if original >= 0 {
                if d < minimum {
                    d = minimum;
                }
            } else if d > minimum.wrapping_neg() {
                d = minimum.wrapping_neg();
            }
        }
        d
    }

    fn set_relative_references(&mut self, op: u8, r: usize, p: usize) {
        self.state.rp[1] = r;
        self.state.rp[2] = p;
        if op & 16 != 0 {
            self.state.rp[0] = p;
        }
    }

    /// The reference point SHP, SHC and SHZ use, and how far it has moved
    /// along the projection vector.
    fn reference_shift(&self, op: u8) -> Option<(usize, usize, i32)> {
        let (zone, r) = if op & 1 == 1 {
            (0, self.state.rp[1])
        } else {
            (1, self.state.rp[2])
        };
        (r < self.zone(zone).len()).then(|| {
            let (c, o) = (self.current(zone, r), self.original(zone, r));
            let d = self
                .state
                .projection
                .project(c.x.wrapping_sub(o.x), c.y.wrapping_sub(o.y));
            (zone, r, d)
        })
    }

    fn shift(&mut self, op: u8) -> Run {
        match op {
            0x32 | 0x33 => {
                let Some(count) = self.looped(0)? else {
                    return Ok(());
                };
                let reference = self.reference_shift(op);
                for _ in 0..count {
                    let p = self.pop_point(2);
                    if let (Some(p), Some((_, _, d))) = (p, reference) {
                        self.move_point(2, p, d, true);
                    }
                }
            }
            0x34 | 0x35 => {
                let c = self.pop();
                let Some((zone, r, d)) = self.reference_shift(op) else {
                    return Ok(());
                };
                let z = self.zone(2);
                let range = usize::try_from(c).ok().and_then(|c| {
                    let ends = z.ends.as_slice();
                    let end = *ends.get(c)? as usize;
                    let start = if c == 0 { 0 } else { ends[c - 1] as usize };
                    Some(start..end)
                });
                // Observed: SHC on a contour the glyph does not have does
                // nothing (`shifts-invalid-contour`).
                let Some(range) = range else {
                    return Ok(());
                };
                self.charge(range.len())?;
                // Observed: SHC touches the points it moves, SHZ does not
                // (`shifts`).
                let same = self.state.zp[zone] == self.state.zp[2];
                for p in range {
                    if !(same && p == r) {
                        self.move_point(2, p, d, true);
                    }
                }
            }
            _ => {
                let Some(e) = self.pop_zone() else {
                    return Ok(());
                };
                let Some((zone, r, d)) = self.reference_shift(op) else {
                    return Ok(());
                };
                let saved = self.state.zp[2];
                self.state.zp[2] = e;
                let same = self.state.zp[zone] == e;
                let len = self.zone(2).len();
                self.charge(len)?;
                for p in 0..len {
                    if !(same && p == r) {
                        self.move_point(2, p, d, false);
                    }
                }
                self.state.zp[2] = saved;
            }
        }
        Ok(())
    }

    fn shift_by_pixels(&mut self) -> Run {
        let Some(count) = self.looped(1)? else {
            return Ok(());
        };
        let amount = self.pop();
        let dy = math::mul_div(amount, self.state.freedom.y, 0x4000);
        for _ in 0..count {
            if let Some(p) = self.pop_point(2) {
                // Observed: in a simple glyph's zone SHPIX moves only points
                // already touched in y (`backward-compatibility`), in a
                // composite's any (`composites`).
                if self.untouched_simple(2, p) {
                    continue;
                }
                self.shift_point(2, p, dy, true);
            }
        }
        Ok(())
    }

    fn delta_points(&mut self, range: i32) -> Run {
        let n = self.pop();
        // Observed: a DELTA instruction stops at the first pair the stack no
        // longer holds (`stack-underflow`).
        for _ in 0..n.max(0) {
            if self.stack.len() < 2 {
                break;
            }
            let p = self.pop_point(0);
            let arg = self.pop();
            self.charge(1)?;
            let (Some(p), Some(amount)) = (p, self.delta(arg, range)) else {
                continue;
            };
            if self.untouched_simple(0, p) {
                continue;
            }
            self.move_point(0, p, amount, true);
        }
        Ok(())
    }

    /// Whether point `p` of the zone `zone` points to is a point of a simple
    /// glyph not yet touched in y, which SHPIX and DELTAP leave alone
    /// (observed, `backward-compatibility`, `moves-delta`).
    fn untouched_simple(&self, zone: usize, p: usize) -> bool {
        self.state.zp[zone] != 0
            && self.glyph.font_units
            && self.glyph.flags.as_slice()[p] & TOUCHED_Y == 0
    }

    /// IP. Observed: between glyph points the original distances are
    /// measured in font units and left unscaled, so a point whose reference
    /// points share an original position moves to its font-unit distance
    /// from rp1 (`interpolation`, `interpolation-ip`).
    fn interpolate(&mut self) -> Run {
        let Some(count) = self.looped(0)? else {
            return Ok(());
        };
        let (r1, r2) = (self.state.rp[1], self.state.rp[2]);
        let valid = r1 < self.zone(0).len() && r2 < self.zone(1).len();
        let units = self.state.zp.iter().all(|&z| z != 0);
        let original = |m: &Self, za: usize, a: usize, zb: usize, b: usize| {
            let v = m.state.dual.unwrap_or(m.state.projection);
            let (pa, pb) = if units {
                (m.glyph.units.as_slice()[a], m.glyph.units.as_slice()[b])
            } else {
                (m.original(za, a), m.original(zb, b))
            };
            v.project(pa.x.wrapping_sub(pb.x), pa.y.wrapping_sub(pb.y))
        };
        let (original_range, current_range) = if valid {
            (original(self, 1, r2, 0, r1), self.distance(1, r2, 0, r1))
        } else {
            (0, 0)
        };
        for _ in 0..count {
            let p = self.pop_point(2);
            let Some(p) = p.filter(|_| valid) else {
                continue;
            };
            let from = original(self, 2, p, 0, r1);
            let current = self.distance(2, p, 0, r1);
            let target = if original_range != 0 {
                math::mul_div(from, current_range, original_range)
            } else {
                from
            };
            self.move_point(2, p, target.wrapping_sub(current), true);
        }
        Ok(())
    }

    /// IUP: interpolates the untouched points of each glyph contour between
    /// the touched ones, in x or y. Observed in backward compatibility mode:
    /// once IUP has run in both directions, glyph points no longer move
    /// (`backward-compatibility`).
    fn interpolate_untouched(&mut self, x: bool) -> Run {
        let frozen = self.interpolated == [true; 2];
        self.interpolated[usize::from(!x)] = true;
        if frozen || self.state.zp[2] == 0 {
            return Ok(());
        }
        self.charge(self.glyph.len())?;
        let touched = if x { TOUCHED_X } else { TOUCHED_Y };
        let z = &mut *self.glyph;
        let mut start = 0;
        for i in 0..z.ends.len() {
            let end = (z.ends.as_slice()[i] as usize).min(z.len());
            let range = start..end;
            start = end;
            let is_touched = |z: &Zone, p: usize| z.flags.as_slice()[p] & touched != 0;
            let Some(first) = range.clone().find(|&p| is_touched(z, p)) else {
                continue;
            };
            let mut previous = first;
            let mut p = first + 1;
            loop {
                if p == range.end {
                    p = range.start;
                }
                if p == first {
                    break;
                }
                if is_touched(z, p) {
                    interpolate_run(z, x, previous, p, range.clone());
                    previous = p;
                }
                p += 1;
            }
            interpolate_run(z, x, previous, first, range);
        }
        Ok(())
    }
}

/// Moves the untouched points after `a` and before `b`, on the contour
/// `range`, between the touched points `a` and `b`, along x or y. Observed:
/// a point between them in font units is placed by a 16.16 ratio of the
/// touched points' current to font-unit distance, rounded, and a point
/// outside is shifted from its current position (`interpolation`).
fn interpolate_run(z: &mut Zone, x: bool, a: usize, b: usize, range: std::ops::Range<usize>) {
    let at = |points: &BudgetVec<Point>, p: usize| {
        let point = points.as_slice()[p];
        if x { point.x } else { point.y }
    };
    let (ua, ub) = (at(&z.units, a), at(&z.units, b));
    let (low, high) = if ua <= ub { (a, b) } else { (b, a) };
    let (u_low, u_high) = (at(&z.units, low), at(&z.units, high));
    let (o_low, o_high) = (at(&z.original, low), at(&z.original, high));
    let (c_low, c_high) = (at(&z.current, low), at(&z.current, high));
    let ratio = (u_high > u_low).then(|| {
        math::div_round(
            (i64::from(c_high) - i64::from(c_low)) << 16,
            i64::from(u_high) - i64::from(u_low),
        )
    });
    let mut p = a + 1;
    loop {
        if p == range.end {
            p = range.start;
        }
        if p == b {
            break;
        }
        let (u, c) = (at(&z.units, p), at(&z.current, p));
        let v = match ratio {
            _ if u <= u_low => c.wrapping_add(c_low.wrapping_sub(o_low)),
            _ if u >= u_high => c.wrapping_add(c_high.wrapping_sub(o_high)),
            Some(ratio) => {
                let offset = math::div_round((i64::from(u) - i64::from(u_low)) * ratio, 1 << 16);
                c_low.wrapping_add(offset as i32)
            }
            None => c,
        };
        let point = &mut z.current.as_mut_slice()[p];
        if x {
            point.x = v;
        } else {
            point.y = v;
        }
        p += 1;
    }
}

/// Where lines `a` and `b` cross, as ISECT places a point. Observed: the
/// products are taken in 26.6 and the crossing found from line `a`, each
/// quotient rounded; lines within about three degrees of parallel give the
/// average of the four points, truncated (`intersections`).
fn intersection(a: [Point; 2], b: [Point; 2]) -> Point {
    let product = |u: i32, v: i32| math::mul_div(u, v, 64);
    let (dax, day) = (a[1].x.wrapping_sub(a[0].x), a[1].y.wrapping_sub(a[0].y));
    let (dbx, dby) = (b[1].x.wrapping_sub(b[0].x), b[1].y.wrapping_sub(b[0].y));
    let (dx, dy) = (b[0].x.wrapping_sub(a[0].x), b[0].y.wrapping_sub(a[0].y));
    let discriminant = product(dax, dby).wrapping_sub(product(day, dbx));
    let dot = product(dax, dbx).wrapping_add(product(day, dby));
    if 19 * i64::from(discriminant).abs() > i64::from(dot).abs() {
        let t = product(dx, dby).wrapping_sub(product(dy, dbx));
        Point::new(
            a[0].x.wrapping_add(math::mul_div(t, dax, discriminant)),
            a[0].y.wrapping_add(math::mul_div(t, day, discriminant)),
        )
    } else {
        let average = |v: [i32; 4]| (v.iter().map(|&v| i64::from(v)).sum::<i64>() / 4) as i32;
        Point::new(
            average([a[0].x, a[1].x, b[0].x, b[1].x]),
            average([a[0].y, a[1].y, b[0].y, b[1].y]),
        )
    }
}

/// GETINFO's result for `selector`. Observed: the reference reports
/// version 40 and a ClearType rasterizer with sub-pixel positioning and gray
/// rendering, symmetric rendering where `symmetric`, and nothing else
/// (`getinfo`).
fn get_info(selector: i32, symmetric: bool) -> i32 {
    let symmetric = if symmetric { 0x4_0000 } else { 0 };
    [
        (0x1, 40),
        (0x40, 0x2000),
        (0x400, 0x2_0000),
        (0x800, symmetric),
        (0x1000, 0x8_0000),
    ]
    .into_iter()
    .filter(|&(bit, _)| selector & bit != 0)
    .fold(0, |result, (_, value)| result | value)
}

/// How many values an instruction pops, apart from those its loop count or
/// a count on the stack ask for.
const fn arity(op: u8) -> usize {
    match op {
        0x0F => 5,
        0x8A => 3,
        0x06..=0x0B
        | 0x23
        | 0x27
        | 0x2A
        | 0x3A
        | 0x3B
        | 0x3E
        | 0x3F
        | 0x42
        | 0x44
        | 0x48..=0x4A
        | 0x50..=0x55
        | 0x5A
        | 0x5B
        | 0x60..=0x63
        | 0x70
        | 0x78
        | 0x79
        | 0x81
        | 0x82
        | 0x86
        | 0x87
        | 0x8B
        | 0x8C
        | 0x8E
        | 0xE0..=0xFF => 2,
        0x10..=0x17
        | 0x1A
        | 0x1C..=0x21
        | 0x25
        | 0x26
        | 0x29
        | 0x2B
        | 0x2C
        | 0x2E
        | 0x2F
        | 0x34..=0x37
        | 0x43
        | 0x45..=0x47
        | 0x4F
        | 0x56..=0x58
        | 0x5C..=0x5F
        | 0x64..=0x6F
        | 0x71..=0x77
        | 0x7E
        | 0x7F
        | 0x85
        | 0x88
        | 0x89
        | 0x8D
        | 0xC0..=0xDF => 1,
        _ => 0,
    }
}

/// Opcodes the instruction set leaves undefined.
const fn is_undefined(op: u8) -> bool {
    matches!(op, 0x28 | 0x7B | 0x83 | 0x84 | 0x8F | 0x90 | 0x91..=0xAF)
}

/// What [`skip`] looks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Skip {
    ElseOrEif,
    Eif,
    Endf,
}

/// The position after the instruction ending a skipped run that starts at
/// `ip`: the ELSE or EIF of the current IF (nested ones skipped), or the ENDF
/// of a definition; `None` if the program ends first.
fn skip(code: &[u8], mut ip: usize, end: usize, target: Skip) -> Option<usize> {
    let mut nesting = 0_u32;
    while ip < end {
        let op = code[ip];
        ip = match op {
            0x40 | 0x41 => {
                let n = usize::from(*code.get(ip + 1)?);
                ip + 2 + n * if op == 0x41 { 2 } else { 1 }
            }
            0xB0..=0xB7 => ip + 1 + usize::from(op - 0xAF),
            0xB8..=0xBF => ip + 1 + 2 * usize::from(op - 0xB7),
            _ => ip + 1,
        };
        match (op, target) {
            (0x58, Skip::ElseOrEif | Skip::Eif) => nesting += 1,
            (0x1B, Skip::ElseOrEif) if nesting == 0 => return Some(ip),
            (0x59, Skip::ElseOrEif | Skip::Eif) => {
                if nesting == 0 {
                    return Some(ip);
                }
                nesting -= 1;
            }
            (0x2D, Skip::Endf) => return Some(ip),
            (0x2C | 0x89, Skip::Endf) => return None,
            _ => {}
        }
    }
    None
}
