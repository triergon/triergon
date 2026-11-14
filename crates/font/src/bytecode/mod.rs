//! The TrueType bytecode interpreter: a face's font program, control value
//! program and glyph programs, run as the reference's default load runs them
//! in backward compatibility mode.
//!
//! The font program runs once at a size, then the control value program
//! scales and adjusts the control value table, fills the storage area and the
//! twilight zone and sets the graphics state glyphs start from. Each glyph
//! program then runs on the glyph's scaled points and its four phantom
//! points. Behaviour the specifications leave open is reproduced as observed
//! by the probes under harness/fonts/probes, each rule naming its probe.

mod exec;
mod math;
mod zone;

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::outline::{Outline, Point, PointKind};
use exec::{Code, Functions, GraphicsState, Machine, Program, Size};
use triergon_core::{Budget, BudgetVec, Error, Result, WorkBudget};
use zone::{ON_CURVE, Zone};

/// The programs of a face: `fpgm` and `prep`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Programs<'a> {
    pub(crate) font: &'a [u8],
    pub(crate) control: &'a [u8],
}

/// The tables of a face the interpreter reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tables<'a> {
    pub(crate) programs: Programs<'a>,
    pub(crate) cvt: &'a [u8],
    pub(crate) maxp: &'a [u8],
    pub(crate) gasp: &'a [u8],
}

/// Whether GETINFO reports symmetric rendering at `ppem`. Observed: as the
/// `gasp` range for the size asks for symmetric smoothing (bit 3), and
/// without `gasp` above 20 pixels per em (`getinfo`, `getinfo-gasp`).
fn symmetric_rendering(gasp: &[u8], ppem: u16) -> bool {
    let field = |at: usize| {
        gasp.get(at..at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let Some(count) = field(2) else {
        return ppem > 20;
    };
    (0..usize::from(count))
        .filter_map(|i| Some((field(4 + 4 * i)?, field(6 + 4 * i)?)))
        .find(|&(largest, _)| ppem <= largest)
        .is_some_and(|(_, behaviour)| behaviour & 8 != 0)
}

/// The `maxp` limits the programs run within.
#[derive(Clone, Copy, Debug, Default)]
struct Profile {
    twilight_points: u16,
    storage: u16,
    functions: u16,
    stack: u16,
}

impl Profile {
    /// The limits of a version 1.0 `maxp` table; none for any other.
    fn read(maxp: &[u8]) -> Self {
        let field = |at: usize| {
            maxp.get(at..at + 2)
                .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]))
        };
        if maxp.get(..4) != Some(&[0, 1, 0, 0]) {
            return Self::default();
        }
        Self {
            twilight_points: field(16),
            storage: field(18),
            functions: field(20),
            stack: field(24),
        }
    }
}

/// A face's interpreter state at one size, after its font and control value
/// programs.
#[derive(Debug)]
pub(crate) struct Hinter {
    ppem: u16,
    scale: i64,
    /// Whether GETINFO reports symmetric rendering at the size.
    symmetric: bool,
    /// The control values and storage the control value program left; each
    /// glyph starts from them (observed, `glyph-state`).
    cvt: BudgetVec<i32>,
    storage: BudgetVec<i32>,
    state: GraphicsState,
    instruct_control: u8,
    functions: Functions,
    instructions: BudgetVec<(u8, Code)>,
    /// The twilight zone, which keeps what each glyph program leaves in it
    /// (observed, `twilight`).
    twilight: Zone,
    stack_limit: usize,
    /// Whether the font or control value program stopped on an error, which
    /// fails every hinted load at the size (observed,
    /// `errors-font-program`, `errors-control-program`).
    failed: bool,
    /// Scratch for one glyph: its stack, control values, storage and zone.
    stack: BudgetVec<i32>,
    glyph_cvt: BudgetVec<i32>,
    glyph_storage: BudgetVec<i32>,
    glyph: Zone,
}

/// INSTCTRL flag 1: glyph programs do not run.
const INHIBIT_GRID_FITTING: u8 = 1;
/// INSTCTRL flag 2: glyphs start from the default graphics state.
const IGNORE_CONTROL_STATE: u8 = 2;

impl Hinter {
    /// Runs the font and control value programs at `ppem` and the 16.16
    /// `scale`, the control values `cvt` (`cvt `, font units) scaled by it.
    pub(crate) fn new(
        tables: Tables<'_>,
        ppem: u16,
        scale: i64,
        budget: &Budget,
        work: &mut WorkBudget,
    ) -> Result<Self> {
        let Tables {
            programs,
            cvt,
            maxp,
            gasp,
        } = tables;
        let profile = Profile::read(maxp);
        let symmetric = symmetric_rendering(gasp, ppem);
        let mut values = BudgetVec::new(budget);
        values.try_reserve_exact(cvt.len() / 2)?;
        for unit in cvt.chunks_exact(2) {
            let unit = i32::from(i16::from_be_bytes([unit[0], unit[1]]));
            values
                .try_push(math::scale(unit, scale))
                .map_err(|e| e.error)?;
        }
        // Observed: the twilight zone holds four points more than `maxp`
        // asks for, and the stack 128 values more (`limits-twilight`,
        // `limits-stack`).
        let mut twilight = Zone::new(budget);
        twilight.reset(usize::from(profile.twilight_points) + 4)?;
        let mut hinter = Self {
            ppem,
            scale,
            symmetric,
            cvt: values,
            storage: BudgetVec::try_filled(budget, 0, usize::from(profile.storage))?,
            state: GraphicsState::default(),
            instruct_control: 0,
            functions: Functions::new(budget, profile.functions)?,
            instructions: BudgetVec::new(budget),
            twilight,
            stack_limit: usize::from(profile.stack) + 128,
            failed: false,
            stack: BudgetVec::new(budget),
            glyph_cvt: BudgetVec::new(budget),
            glyph_storage: BudgetVec::new(budget),
            glyph: Zone::new(budget),
        };
        hinter.stack.try_reserve_exact(hinter.stack_limit)?;
        for program in [Program::Font, Program::Control] {
            let mut machine = Machine {
                code: [programs.font, programs.control, &[]],
                stack: &mut hinter.stack,
                stack_limit: hinter.stack_limit,
                cvt: hinter.cvt.as_mut_slice(),
                storage: hinter.storage.as_mut_slice(),
                functions: &mut hinter.functions,
                instructions: &mut hinter.instructions,
                twilight: &mut hinter.twilight,
                glyph: &mut hinter.glyph,
                state: if program == Program::Control {
                    GraphicsState::default()
                } else {
                    hinter.state
                },
                size: Size {
                    ppem,
                    scale,
                    symmetric,
                },
                work,
                program,
                instruct_control: hinter.instruct_control,
                interpolated: [false; 2],
            };
            hinter.failed |= !machine.run(program)?;
            if program == Program::Control {
                hinter.state = machine.state;
                hinter.instruct_control = machine.instruct_control;
            }
            hinter.stack.clear();
        }
        Ok(hinter)
    }

    /// The 16.16 scale the hinter was made for.
    pub(crate) const fn scale(&self) -> i64 {
        self.scale
    }

    /// Whether glyph programs run: the control value program can turn them
    /// off (INSTCTRL flag 1).
    pub(crate) const fn grid_fitting(&self) -> bool {
        self.instruct_control & INHIBIT_GRID_FITTING == 0
    }

    /// Whether the hinter was made for this size.
    pub(crate) fn is_for(&self, ppem: u16, scale: i64) -> bool {
        self.ppem == ppem && self.scale == scale
    }

    /// Runs a glyph program on the points of `outline` from `base` on, whose
    /// contours end at `ends` (relative to `base`) and that started at
    /// `units` in font units, with the glyph's four phantom points
    /// `phantoms` (scaled, and in font units), and moves the outline's
    /// points as the program leaves them.
    pub(crate) fn hint(
        &mut self,
        programs: Programs<'_>,
        instructions: &[u8],
        glyph: GlyphPoints<'_>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("TrueType font or control value program"));
        }
        // Without a program to run the points stay as they are.
        if !self.grid_fitting() || instructions.is_empty() {
            return Ok(());
        }
        let GlyphPoints {
            outline,
            base,
            ends,
            units,
            phantoms,
        } = glyph;
        let zone = &mut self.glyph;
        zone.clear();
        let points = &outline.points()[base..];
        // A composite's points start where its components left them.
        zone.font_units = units.is_some();
        for (i, p) in points.iter().enumerate() {
            let u = units.map_or(p.point, |u| u[i]);
            zone.push(p.point, u, p.kind == PointKind::On)?;
        }
        for (p, u) in phantoms.0.into_iter().zip(phantoms.1) {
            zone.push(p, if units.is_some() { u } else { p }, true)?;
        }
        zone.ends.try_extend_from_slice(ends)?;
        self.glyph_cvt.clear();
        self.glyph_cvt.try_extend_from_slice(self.cvt.as_slice())?;
        self.glyph_storage.clear();
        self.glyph_storage
            .try_extend_from_slice(self.storage.as_slice())?;
        let state = if self.instruct_control & IGNORE_CONTROL_STATE != 0 {
            GraphicsState::default()
        } else {
            GraphicsState::for_glyph(&self.state)
        };
        let mut machine = Machine {
            code: [programs.font, programs.control, instructions],
            stack: &mut self.stack,
            stack_limit: self.stack_limit,
            cvt: self.glyph_cvt.as_mut_slice(),
            storage: self.glyph_storage.as_mut_slice(),
            functions: &mut self.functions,
            instructions: &mut self.instructions,
            twilight: &mut self.twilight,
            glyph: &mut self.glyph,
            state,
            size: Size {
                ppem: self.ppem,
                scale: self.scale,
                symmetric: self.symmetric,
            },
            work,
            program: Program::Glyph,
            instruct_control: self.instruct_control,
            interpolated: [false; 2],
        };
        let result = machine.run(Program::Glyph);
        self.stack.clear();
        result?;
        // The phantom points move nothing: the advances stay unhinted.
        let zone = &self.glyph;
        let flags = zone.flags.as_slice();
        for (i, p) in outline.points_mut()[base..].iter_mut().enumerate() {
            p.point = zone.current.as_slice()[i];
            p.kind = if flags[i] & ON_CURVE != 0 {
                PointKind::On
            } else {
                PointKind::Quadratic
            };
        }
        Ok(())
    }
}

/// A glyph's points as [`Hinter::hint`] takes them.
pub(crate) struct GlyphPoints<'o> {
    pub(crate) outline: &'o mut Outline,
    pub(crate) base: usize,
    pub(crate) ends: &'o [u32],
    /// The points' font-unit positions; none for a composite glyph.
    pub(crate) units: Option<&'o [Point]>,
    /// The phantom points, scaled and in font units.
    pub(crate) phantoms: ([Point; 4], [Point; 4]),
}

/// A face's interpreter, made on the first hinted load and kept for the size
/// it was last made at.
#[derive(Debug)]
pub(crate) struct Bytecode {
    budget: Budget,
    state: Mutex<Option<Box<State>>>,
}

/// What a face's interpreter keeps between loads.
#[derive(Debug)]
pub(crate) struct State {
    pub(crate) hinter: Hinter,
    /// Scratch for one load: the font-unit positions of its points and one
    /// glyph's contour ends.
    pub(crate) units: BudgetVec<Point>,
    pub(crate) ends: BudgetVec<u32>,
}

impl Bytecode {
    pub(crate) fn new(budget: Budget) -> Self {
        Self {
            budget,
            state: Mutex::new(None),
        }
    }

    /// The interpreter at `ppem` and the 16.16 `scale`, made from `tables`
    /// (the face's) unless it already is, held for one load.
    pub(crate) fn at_size<'t>(
        &self,
        tables: impl FnOnce() -> Tables<'t>,
        ppem: u16,
        scale: i64,
        work: u64,
    ) -> Result<MutexGuard<'_, Option<Box<State>>>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.as_ref().is_some_and(|s| s.hinter.is_for(ppem, scale)) {
            // The old size's state goes before the new one is made.
            *state = None;
            let mut setup = WorkBudget::new(work);
            let hinter = Hinter::new(tables(), ppem, scale, &self.budget, &mut setup)?;
            *state = Some(Box::new(State {
                hinter,
                units: BudgetVec::new(&self.budget),
                ends: BudgetVec::new(&self.budget),
            }));
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::{GlyphPoints, Hinter, Programs, Tables};
    use crate::outline::{Outline, Point, PointKind};
    use triergon_core::{Budget, Error, WorkBudget};

    /// A version 1.0 `maxp`: 4 twilight points, 8 storage locations, 8
    /// functions, 64 stack values.
    const MAXP: [u8; 32] = [
        0, 1, 0, 0, 0, 1, 0, 4, 0, 1, 0, 0, 0, 0, 0, 2, 0, 4, 0, 8, 0, 8, 0, 0, 0, 64, 0, 0, 0, 0,
        0, 0,
    ];

    /// Runs `program` at 16 pixels per em (font units halved) on a square
    /// glyph whose points are in font units, and returns its points.
    fn hint(fpgm: &[u8], prep: &[u8], program: &[u8], work: u64) -> Result<Vec<Point>, Error> {
        let budget = Budget::new(1 << 20)?;
        let programs = Programs {
            font: fpgm,
            control: prep,
        };
        let mut setup = WorkBudget::new(1 << 16);
        let cvt = [0, 100];
        let tables = Tables {
            programs,
            cvt: &cvt,
            maxp: &MAXP,
            gasp: &[],
        };
        let mut hinter = Hinter::new(tables, 16, 0x8000, &budget, &mut setup)?;
        let units = [(100, 3), (100, 703), (501, 703), (501, 3)].map(|(x, y)| Point::new(x, y));
        let mut outline = Outline::new(&budget);
        outline.begin(usize::MAX);
        for u in units {
            outline.push_point(Point::new(u.x / 2, u.y / 2), PointKind::On)?;
        }
        outline.end_contour()?;
        let phantoms = [Point::default(); 4];
        let points = GlyphPoints {
            outline: &mut outline,
            base: 0,
            ends: &[4],
            units: Some(&units),
            phantoms: (phantoms, phantoms),
        };
        hinter.hint(programs, program, points, &mut WorkBudget::new(work))?;
        Ok(outline.points().iter().map(|p| p.point).collect())
    }

    fn ys(points: &[Point]) -> Vec<i32> {
        points.iter().map(|p| p.y).collect()
    }

    #[test]
    fn programs_move_points_in_y_only() {
        // SVTCA[0]; PUSHB 1; MDAP[1]; SVTCA[1]; PUSHB 2; MDAP[1]
        let points = hint(&[], &[], &[0x00, 0xB0, 1, 0x2F, 0x01, 0xB0, 2, 0x2F], 100).unwrap();
        assert_eq!(ys(&points), [1, 320, 351, 1]);
        assert_eq!(points[2].x, 250);
    }

    #[test]
    fn functions_from_the_font_program_run_in_glyphs() {
        // fpgm: PUSHB 0; FDEF; SVTCA[0]; PUSHB 1; MDAP[1]; ENDF
        let fpgm = [0xB0, 0, 0x2C, 0x00, 0xB0, 1, 0x2F, 0x2D];
        // PUSHB 0; CALL; IUP[0]
        let points = hint(&fpgm, &[], &[0xB0, 0, 0x2B, 0x30], 100).unwrap();
        assert_eq!(ys(&points), [-30, 320, 320, -30]);
    }

    #[test]
    fn short_stacks_and_bad_points_do_not_stop_a_program() {
        // ADD on an empty stack; MDAP on point 9; then SVTCA[0]; PUSHB 1; MDAP[1]
        let points = hint(&[], &[], &[0x60, 0xB0, 9, 0x2F, 0x00, 0xB0, 1, 0x2F], 100).unwrap();
        assert_eq!(ys(&points), [1, 320, 351, 1]);
    }

    #[test]
    fn hostile_programs_end_within_the_work_budget() {
        // An endless loop: PUSHW 0; JMPR jumps back to the push.
        let looping = [0xB8, 0xFF, 0xFD, 0x1C];
        assert_eq!(
            hint(&[], &[], &looping, 1000),
            Err(Error::Policy("TrueType instructions"))
        );
        // A function calling itself without end runs out of call depth and
        // stops the program, keeping the points as they are.
        let fpgm = [0xB0, 0, 0x2C, 0xB0, 0, 0x2B, 0x2D];
        let points = hint(&fpgm, &[], &[0xB0, 0, 0x2B], 1000).unwrap();
        assert_eq!(ys(&points), [1, 351, 351, 1]);
    }

    #[test]
    fn an_error_in_the_control_value_program_fails_hinted_loads() {
        // prep: an undefined opcode.
        assert_eq!(
            hint(&[], &[0x28], &[], 100),
            Err(Error::Invalid("TrueType font or control value program"))
        );
    }

    #[test]
    fn the_control_value_program_can_turn_glyph_programs_off() {
        // prep: PUSHB 1 1; INSTCTRL
        let points = hint(&[], &[0xB1, 1, 1, 0x8E], &[0x00, 0xB0, 1, 0x2F], 100).unwrap();
        assert_eq!(ys(&points), [1, 351, 351, 1]);
    }
}
