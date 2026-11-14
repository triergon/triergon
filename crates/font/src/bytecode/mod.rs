//! The TrueType bytecode interpreter: a face's font program, control value
//! program and glyph programs, run as the reference's default load runs them
//! in backward compatibility mode.
//!
//! The font program runs once for a face, at the first size it loads hinted
//! at, and defines functions and instructions. At each size request the control
//! value program then scales and adjusts the control value table, fills the
//! storage area and the twilight zone and sets the graphics state glyphs start
//! from.
//! Each glyph program then runs on the glyph's scaled points and its four
//! phantom points. Behaviour the specifications leave open is reproduced as observed
//! by the probes under harness/fonts/probes, each rule naming its probe.

mod exec;
mod math;
mod zone;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::outline::{Outline, Point, PointKind};
use exec::{Functions, GraphicsState, Instructions, Machine, Program, Size};
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

/// The limits the programs run within.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Profile {
    /// `maxp`'s twilight points, storage locations, instruction definitions
    /// and stack values.
    twilight_points: u16,
    storage: u16,
    instruction_defs: u16,
    stack: u16,
    /// How many distinct function numbers may be defined.
    functions: usize,
}

impl Profile {
    /// Observed: the stack holds 128 values more than `maxp` asks for
    /// (`limits-stack`).
    fn stack_limit(self) -> usize {
        usize::from(self.stack) + 128
    }

    /// Observed: the twilight zone holds four points more than `maxp` asks
    /// for (`limits-twilight`).
    fn twilight_len(self) -> usize {
        usize::from(self.twilight_points) + 4
    }

    /// The limits a `maxp` table sets. Observed: up to `maxp`'s count of
    /// function definitions or 64, whichever is more, and none at all with a
    /// `maxp` other than version 1.0, which leaves every other limit at zero
    /// (`limits-functions-64`, `limits-functions-65`,
    /// `limits-functions-maxp-0`, `limits-functions-maxp-1`,
    /// `limits-maxp-0.5`, `limits-maxp-0.5-functions-64`).
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
            instruction_defs: field(22),
            stack: field(24),
            functions: usize::from(field(20)).max(64),
        }
    }
}

/// What a face's programs define, which every size keeps: the functions and
/// instruction definitions of the font program, run once for the face at the
/// first size it loads hinted at, and of the control value program at each
/// size since (observed, `sizes-definitions`, `sizes-control-definitions`).
#[derive(Debug)]
pub(crate) struct Definitions {
    functions: Functions,
    instructions: Instructions,
    /// Whether the font program stopped on an error, which fails every hinted
    /// load of the face at every size (observed, `errors-font-program`,
    /// `sizes-font-program-error`).
    failed: bool,
}

impl Definitions {
    /// Runs the font program at `ppem` and the 16.16 `scale`. Observed: it
    /// reads control values and storage as zero, and what it writes there,
    /// in the twilight zone or to the graphics state is not what the control
    /// value program or a glyph program find (`sizes-storage`,
    /// `sizes-font-program-reads`, `sizes-graphics-state`).
    fn new(
        tables: Tables<'_>,
        ppem: u16,
        scale: i64,
        budget: &Budget,
        work: &mut WorkBudget,
    ) -> Result<Self> {
        let profile = Profile::read(tables.maxp);
        // A unit of work for each value the run fills.
        let filled =
            usize::from(profile.storage) + usize::from(profile.twilight_points) + profile.functions;
        if !work.try_charge(filled as u64) {
            return Err(Error::Policy("TrueType instructions"));
        }
        let mut definitions = Self {
            functions: Functions::new(budget, profile.functions)?,
            instructions: Instructions::new(budget, profile.instruction_defs)?,
            failed: false,
        };
        let mut stack = BudgetVec::new(budget);
        stack.try_reserve_exact(profile.stack_limit())?;
        let mut storage = BudgetVec::try_filled(budget, 0, usize::from(profile.storage))?;
        let mut twilight = Zone::new(budget);
        twilight.reset(profile.twilight_len())?;
        let mut glyph = Zone::new(budget);
        let mut machine = Machine {
            code: [tables.programs.font, tables.programs.control, &[]],
            stack: &mut stack,
            stack_limit: profile.stack_limit(),
            cvt: &mut [],
            storage: storage.as_mut_slice(),
            functions: &mut definitions.functions,
            instructions: &mut definitions.instructions,
            twilight: &mut twilight,
            glyph: &mut glyph,
            state: GraphicsState::default(),
            size: Size {
                ppem,
                scale,
                symmetric: symmetric_rendering(tables.gasp, ppem),
            },
            work,
            program: Program::Font,
            instruct_control: 0,
            interpolated: [false; 2],
        };
        definitions.failed = !machine.run(Program::Font)?;
        Ok(definitions)
    }
}

/// A face's interpreter state at one size, after its control value program.
#[derive(Debug)]
pub(crate) struct Hinter {
    ppem: u16,
    scale: i64,
    /// Whether GETINFO reports symmetric rendering at the size.
    symmetric: bool,
    definitions: Box<Definitions>,
    /// The control values and storage the control value program left; each
    /// glyph starts from them (observed, `glyph-state`).
    cvt: BudgetVec<i32>,
    storage: BudgetVec<i32>,
    state: GraphicsState,
    instruct_control: u8,
    /// The twilight zone, which keeps what each glyph program leaves in it
    /// (observed, `twilight`).
    twilight: Zone,
    stack_limit: usize,
    /// Whether the control value program stopped on an error, which fails
    /// every hinted load at the size (observed, `errors-control-program`,
    /// `sizes-control-program-error`).
    failed: bool,
    /// Scratch for one glyph: its stack, control values, storage and zone.
    stack: BudgetVec<i32>,
    glyph_cvt: BudgetVec<i32>,
    glyph_storage: BudgetVec<i32>,
    glyph: Zone,
}

/// What each size starts from before its control value program: the control
/// values scaled to it, zeroed storage, a twilight zone at the origin and an
/// empty stack (observed, `sizes-storage`, `sizes-twilight`).
struct Fresh {
    cvt: BudgetVec<i32>,
    storage: BudgetVec<i32>,
    twilight: Zone,
    stack: BudgetVec<i32>,
}

impl Fresh {
    fn new(
        cvt: &[u8],
        profile: Profile,
        scale: i64,
        budget: &Budget,
        work: &mut WorkBudget,
    ) -> Result<Self> {
        // A unit of work for each value filled.
        let filled =
            cvt.len() / 2 + usize::from(profile.storage) + usize::from(profile.twilight_points);
        if !work.try_charge(filled as u64) {
            return Err(Error::Policy("TrueType instructions"));
        }
        let mut values = BudgetVec::new(budget);
        values.try_reserve_exact(cvt.len() / 2)?;
        for unit in cvt.chunks_exact(2) {
            let unit = i32::from(i16::from_be_bytes([unit[0], unit[1]]));
            values
                .try_push(math::scale(unit, scale))
                .map_err(|e| e.error)?;
        }
        let mut twilight = Zone::new(budget);
        twilight.reset(profile.twilight_len())?;
        let mut stack = BudgetVec::new(budget);
        stack.try_reserve_exact(profile.stack_limit())?;
        Ok(Self {
            cvt: values,
            storage: BudgetVec::try_filled(budget, 0, usize::from(profile.storage))?,
            twilight,
            stack,
        })
    }
}

/// INSTCTRL flag 1: glyph programs do not run.
const INHIBIT_GRID_FITTING: u8 = 1;
/// INSTCTRL flag 2: glyphs start from the default graphics state.
const IGNORE_CONTROL_STATE: u8 = 2;

impl Hinter {
    /// Runs the control value program at `ppem` and the 16.16 `scale`, with
    /// the control values `cvt ` (font units) scaled by it, after the
    /// face's `definitions`, which it may add to. A limit the program runs
    /// into is returned with the definitions, which the face keeps.
    pub(crate) fn new(
        tables: Tables<'_>,
        ppem: u16,
        scale: i64,
        definitions: Box<Definitions>,
        budget: &Budget,
        work: &mut WorkBudget,
    ) -> std::result::Result<Self, (Error, Box<Definitions>)> {
        let profile = Profile::read(tables.maxp);
        let fresh = match Fresh::new(tables.cvt, profile, scale, budget, work) {
            Ok(fresh) => fresh,
            Err(e) => return Err((e, definitions)),
        };
        let symmetric = symmetric_rendering(tables.gasp, ppem);
        let mut hinter = Self {
            ppem,
            scale,
            symmetric,
            definitions,
            cvt: fresh.cvt,
            storage: fresh.storage,
            state: GraphicsState::default(),
            instruct_control: 0,
            twilight: fresh.twilight,
            stack_limit: profile.stack_limit(),
            failed: false,
            stack: fresh.stack,
            glyph_cvt: BudgetVec::new(budget),
            glyph_storage: BudgetVec::new(budget),
            glyph: Zone::new(budget),
        };
        let mut machine = Machine {
            code: [tables.programs.font, tables.programs.control, &[]],
            stack: &mut hinter.stack,
            stack_limit: hinter.stack_limit,
            cvt: hinter.cvt.as_mut_slice(),
            storage: hinter.storage.as_mut_slice(),
            functions: &mut hinter.definitions.functions,
            instructions: &mut hinter.definitions.instructions,
            twilight: &mut hinter.twilight,
            glyph: &mut hinter.glyph,
            state: GraphicsState::default(),
            size: Size {
                ppem,
                scale,
                symmetric,
            },
            work,
            program: Program::Control,
            instruct_control: 0,
            interpolated: [false; 2],
        };
        let ran = machine.run(Program::Control);
        let (state, instruct_control) = (machine.state, machine.instruct_control);
        match ran {
            Ok(ran) => {
                hinter.failed = !ran;
                hinter.state = state;
                hinter.instruct_control = instruct_control;
                hinter.stack.clear();
                Ok(hinter)
            }
            Err(e) => Err((e, hinter.definitions)),
        }
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
    /// `phantoms`, and moves the outline's points as the program leaves
    /// them.
    pub(crate) fn hint(
        &mut self,
        programs: Programs<'_>,
        instructions: &[u8],
        glyph: GlyphPoints<'_>,
        work: &mut WorkBudget,
    ) -> Result<()> {
        if self.failed || self.definitions.failed {
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
        // A unit of work for each point, control value and storage location
        // the glyph starts from.
        let points = &outline.points()[base..];
        let copied = points.len() + self.cvt.len() + self.storage.len();
        if !work.try_charge(copied as u64) {
            return Err(Error::Policy("TrueType instructions"));
        }
        let zone = &mut self.glyph;
        zone.clear();
        // A composite's points start where its components left them.
        zone.font_units = units.is_some();
        for (i, p) in points.iter().enumerate() {
            let u = units.map_or(p.point, |u| u[i]);
            zone.push(p.point, p.point, u, p.kind == PointKind::On)?;
        }
        for i in 0..4 {
            let original = phantoms.original[i];
            let u = if units.is_some() {
                phantoms.units[i]
            } else {
                original
            };
            zone.push(phantoms.current[i], original, u, true)?;
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
            functions: &mut self.definitions.functions,
            instructions: &mut self.definitions.instructions,
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
    pub(crate) phantoms: Phantoms,
}

/// A glyph's four phantom points as its program starts from them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Phantoms {
    /// Their current positions, partly on the pixel grid.
    pub(crate) current: [Point; 4],
    /// Their original positions: scaled, off the grid (observed,
    /// `phantom-originals`).
    pub(crate) original: [Point; 4],
    /// Their positions in font units.
    pub(crate) units: [Point; 4],
}

/// A face's interpreter: the definitions its font program made at the first
/// hinted load, and the state of the size it last loaded hinted at, or why it
/// could not be made. A failure for want of memory the budget may later have
/// (`Backpressure`) is not kept.
#[derive(Debug)]
pub(crate) struct Bytecode {
    budget: Budget,
    setup: Mutex<Option<Setup>>,
    /// Whether the face has been requested at a size since the size's state
    /// was made.
    requested: AtomicBool,
}

/// What a face's interpreter keeps.
#[derive(Debug)]
enum Setup {
    /// The font program could not run within the face's limits, which fails
    /// every hinted load of the face.
    Unusable(Error),
    /// The interpreter at a size.
    Ready(Box<State>),
    /// A size whose control value program could not run within the face's
    /// limits; one that ran out of memory the budget may later have is made
    /// again.
    Failed {
        ppem: u16,
        scale: i64,
        error: Error,
        definitions: Box<Definitions>,
    },
}

impl Setup {
    fn is_for(&self, ppem: u16, scale: i64) -> bool {
        match self {
            Self::Unusable(_) => true,
            Self::Ready(state) => state.hinter.is_for(ppem, scale),
            Self::Failed {
                ppem: p,
                scale: s,
                error,
                ..
            } => (*p, *s) == (ppem, scale) && !matches!(error, Error::Backpressure { .. }),
        }
    }

    /// The face's definitions, leaving the size.
    fn into_definitions(self) -> Option<Box<Definitions>> {
        match self {
            Self::Unusable(_) => None,
            Self::Failed { definitions, .. } => Some(definitions),
            Self::Ready(state) => Some(state.hinter.definitions),
        }
    }
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
            setup: Mutex::new(None),
            requested: AtomicBool::new(false),
        }
    }

    /// Records a request of the face at a size: the next hinted load makes
    /// its size's state afresh, even at the size the state is for (probe
    /// `sizes-same-size-request`).
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Relaxed);
    }

    /// Runs `load` with the interpreter at `ppem` and the 16.16 `scale`, made
    /// from `tables` (the face's) within `work` units unless it already is,
    /// or made and failed, since the last request: the font program runs at
    /// the first size, the control value program at each size and request.
    pub(crate) fn with_size<'t, R>(
        &self,
        tables: impl FnOnce() -> Tables<'t>,
        ppem: u16,
        scale: i64,
        work: u64,
        load: impl FnOnce(&mut State) -> Result<R>,
    ) -> Result<R> {
        let mut guard = self.setup.lock().unwrap_or_else(PoisonError::into_inner);
        let requested = self.requested.swap(false, Ordering::Relaxed);
        let setup = match guard.take() {
            Some(setup @ Setup::Unusable(_)) => guard.insert(setup),
            Some(setup) if !requested && setup.is_for(ppem, scale) => guard.insert(setup),
            kept => {
                let tables = tables();
                let mut budget = WorkBudget::new(work);
                // The old size's state goes before the new one is made.
                let definitions = match kept.map(Setup::into_definitions) {
                    Some(Some(definitions)) => definitions,
                    _ => match Definitions::new(tables, ppem, scale, &self.budget, &mut budget) {
                        Ok(definitions) => Box::new(definitions),
                        Err(e @ Error::Backpressure { .. }) => return Err(e),
                        Err(error) => {
                            *guard = Some(Setup::Unusable(error));
                            return Err(error);
                        }
                    },
                };
                let setup = match Hinter::new(
                    tables,
                    ppem,
                    scale,
                    definitions,
                    &self.budget,
                    &mut budget,
                ) {
                    Ok(hinter) => Setup::Ready(Box::new(State {
                        hinter,
                        units: BudgetVec::new(&self.budget),
                        ends: BudgetVec::new(&self.budget),
                    })),
                    Err((error, definitions)) => Setup::Failed {
                        ppem,
                        scale,
                        error,
                        definitions,
                    },
                };
                guard.insert(setup)
            }
        };
        match setup {
            Setup::Ready(state) => load(state),
            Setup::Unusable(error) | Setup::Failed { error, .. } => Err(*error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bytecode, Definitions, GlyphPoints, Hinter, Phantoms, Programs, Tables};
    use crate::outline::{Outline, Point, PointKind};
    use std::cell::Cell;
    use triergon_core::{Budget, Error, WorkBudget};

    /// A version 1.0 `maxp`: 4 twilight points, 8 storage locations, 8
    /// functions, 64 stack values.
    const MAXP: [u8; 32] = [
        0, 1, 0, 0, 0, 1, 0, 4, 0, 1, 0, 0, 0, 0, 0, 2, 0, 4, 0, 8, 0, 8, 0, 0, 0, 64, 0, 0, 0, 0,
        0, 0,
    ];

    /// `MAXP` with as many twilight points as a `maxp` can ask for.
    fn wide_twilight() -> [u8; 32] {
        let mut maxp = MAXP;
        maxp[16..18].copy_from_slice(&[0xFF, 0xFF]);
        maxp
    }

    /// The tables of a face with `maxp`, `fpgm` and `prep`, and one control
    /// value.
    fn tables<'a>(maxp: &'a [u8], fpgm: &'a [u8], prep: &'a [u8]) -> Tables<'a> {
        Tables {
            programs: Programs {
                font: fpgm,
                control: prep,
            },
            cvt: &[0, 100],
            maxp,
            gasp: &[],
        }
    }

    /// Runs `program` at 16 pixels per em (font units halved) on a square
    /// glyph whose points are in font units, and returns its points.
    fn hint(fpgm: &[u8], prep: &[u8], program: &[u8], work: u64) -> Result<Vec<Point>, Error> {
        hint_in(tables(&MAXP, fpgm, prep), 1 << 16, program, work)
    }

    /// As [`hint`] for the face `tables`, its setup within `setup` units.
    fn hint_in(
        tables: Tables<'_>,
        setup: u64,
        program: &[u8],
        work: u64,
    ) -> Result<Vec<Point>, Error> {
        let budget = Budget::new(1 << 24)?;
        let programs = tables.programs;
        let mut setup = WorkBudget::new(setup);
        let definitions = Definitions::new(tables, 16, 0x8000, &budget, &mut setup)?;
        let mut hinter = Hinter::new(
            tables,
            16,
            0x8000,
            Box::new(definitions),
            &budget,
            &mut setup,
        )
        .map_err(|(e, _)| e)?;
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
            phantoms: Phantoms {
                current: phantoms,
                original: phantoms,
                units: phantoms,
            },
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
    fn instructions_are_charged_for_each_point_and_value_they_go_through() {
        // PUSHB 0; SHZ[0]; PUSHW -6; JMPR: shifts the 65,539 twilight points
        // over and over.
        let shifting = [0xB0, 0, 0x36, 0xB8, 0xFF, 0xFA, 0x1C];
        let maxp = wide_twilight();
        let started = std::time::Instant::now();
        assert_eq!(
            hint_in(tables(&maxp, &[], &[]), 1 << 24, &shifting, 1 << 24),
            Err(Error::Policy("TrueType instructions"))
        );
        // The same in the control value program fails the setup.
        assert_eq!(
            hint_in(tables(&maxp, &[], &shifting), 1 << 24, &[], 1 << 24),
            Err(Error::Policy("TrueType instructions"))
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // NPUSHB of 255 values costs 256 units.
        let mut npush = vec![0x40, 255];
        npush.extend([0; 255]);
        assert!(hint(&[], &[], &npush, 300).is_ok());
        assert_eq!(
            hint(&[], &[], &npush, 200),
            Err(Error::Policy("TrueType instructions"))
        );
        // A skipped IF body costs a unit a byte: PUSHB 0; IF; 250 x
        // (PUSHB 0); EIF.
        let mut skipped = vec![0xB0, 0, 0x58];
        skipped.extend([0xB0, 0].repeat(125));
        skipped.push(0x59);
        assert_eq!(
            hint(&[], &[], &skipped, 200),
            Err(Error::Policy("TrueType instructions"))
        );
    }

    #[test]
    fn calls_nest_32_deep() {
        // fpgm: function 0 calls itself while storage 0 counts down, then
        // rounds point 1: PUSHB 0; FDEF; PUSHB 0; RS; IF; PUSHB 0 0; RS;
        // PUSHB 1; SUB; WS; PUSHB 0; CALL; ELSE; SVTCA[0]; PUSHB 1; MDAP[1];
        // EIF; ENDF.
        let fpgm = [
            0xB0, 0, 0x2C, 0xB0, 0, 0x43, 0x58, 0xB1, 0, 0, 0x43, 0xB0, 1, 0x61, 0x42, 0xB0, 0,
            0x2B, 0x1B, 0x00, 0xB0, 1, 0x2F, 0x59, 0x2D,
        ];
        // PUSHB 0 n-1; WS; PUSHB 0; CALL: n calls in progress at the deepest.
        let calls = |n: u8| hint(&fpgm, &[], &[0xB1, 0, n - 1, 0x42, 0xB0, 0, 0x2B], 10_000);
        assert_eq!(ys(&calls(32).unwrap()), [1, 320, 351, 1]);
        assert_eq!(ys(&calls(33).unwrap()), [1, 351, 351, 1]);
    }

    #[test]
    fn delta_bases_keep_16_bits() {
        // 2^31 - 1 from words: PUSHW 32767 16384; MUL; PUSHW 16384; MUL;
        // PUSHW 32767; ADD; PUSHW 32767; ADD; PUSHB 1; ADD.
        let max = [
            0xB9, 0x7F, 0xFF, 0x40, 0x00, 0x63, 0xB8, 0x40, 0x00, 0x63, 0xB8, 0x7F, 0xFF, 0x60,
            0xB8, 0x7F, 0xFF, 0x60, 0xB0, 1, 0x60,
        ];
        // Then SDB; SVTCA[0]; PUSHB 1; MDAP[0]; PUSHB 0x6F 1 1; DELTAP1:
        // no ppem matches, and nothing overflows.
        let mut program = max.to_vec();
        program.extend([0x5E, 0x00, 0xB0, 1, 0x2E, 0xB2, 0x6F, 1, 1, 0x5D]);
        assert_eq!(
            ys(&hint(&[], &[], &program, 1000).unwrap()),
            [1, 351, 351, 1]
        );
        // 65536 + 10 acts as 10 (ppem 16 = 10 + 6): PUSHW 16384 256; MUL;
        // PUSHB 10; ADD; SDB; ...
        let mut program = vec![0xB9, 0x40, 0x00, 0x01, 0x00, 0x63, 0xB0, 10, 0x60];
        program.extend([0x5E, 0x00, 0xB0, 1, 0x2E, 0xB2, 0x6F, 1, 1, 0x5D]);
        assert_eq!(
            ys(&hint(&[], &[], &program, 1000).unwrap()),
            [1, 415, 351, 1]
        );
    }

    #[test]
    fn a_failed_setup_is_kept_for_its_size() {
        // prep: PUSHW -3; JMPR, without end.
        let looping = [0xB8, 0xFF, 0xFD, 0x1C];
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        let made = Cell::new(0);
        let at = |ppem| {
            bytecode.with_size(
                || {
                    made.set(made.get() + 1);
                    tables(&MAXP, &[], &looping)
                },
                ppem,
                i64::from(ppem) << 11,
                1000,
                |_| Ok(()),
            )
        };
        assert_eq!(at(16), Err(Error::Policy("TrueType instructions")));
        assert_eq!(at(16), Err(Error::Policy("TrueType instructions")));
        assert_eq!(made.get(), 1);
        assert_eq!(at(17), Err(Error::Policy("TrueType instructions")));
        assert_eq!(made.get(), 2);
    }

    #[test]
    fn a_request_remakes_the_size_it_is_at() {
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        let made = Cell::new(0);
        let at = |ppem| {
            bytecode.with_size(
                || {
                    made.set(made.get() + 1);
                    tables(&MAXP, &[], &[])
                },
                ppem,
                i64::from(ppem) << 11,
                1000,
                |_| Ok(()),
            )
        };
        at(16).unwrap();
        at(16).unwrap();
        assert_eq!(made.get(), 1);
        bytecode.request();
        at(16).unwrap();
        at(16).unwrap();
        assert_eq!(made.get(), 2);
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

    /// Loads at `ppem` through `bytecode`, made from `tables`, and returns
    /// what `CALL 1` leaves on the stack of a glyph program, or the error.
    fn function_1_at(bytecode: &Bytecode, tables: Tables<'_>, ppem: u16) -> Result<i32, Error> {
        bytecode.with_size(
            || tables,
            ppem,
            i64::from(ppem) << 11,
            10_000,
            |state| {
                let points = [(0, 0), (0, 0)].map(|(x, y)| Point::new(x, y));
                let mut outline = Outline::new(&Budget::new(1 << 20)?);
                outline.begin(usize::MAX);
                for p in points {
                    outline.push_point(p, PointKind::On)?;
                }
                outline.end_contour()?;
                let phantoms = [Point::default(); 4];
                let glyph = GlyphPoints {
                    outline: &mut outline,
                    base: 0,
                    ends: &[2],
                    units: Some(&points),
                    phantoms: Phantoms {
                        current: phantoms,
                        original: phantoms,
                        units: phantoms,
                    },
                };
                // SVTCA[0]; PUSHB 0 1; CALL; SCFS: function 1's value into
                // point 0's y.
                let program = [0x00, 0xB1, 0, 1, 0x2B, 0x48];
                state
                    .hinter
                    .hint(tables.programs, &program, glyph, &mut WorkBudget::new(1000))?;
                Ok(outline.points()[0].point.y)
            },
        )
    }

    #[test]
    fn the_font_program_runs_once_for_the_face() {
        // fpgm: MPPEM; PUSHB 12; LT; IF; PUSHB 1; FDEF; PUSHB 111; ENDF; ELSE;
        // PUSHB 1; FDEF; PUSHB 222; ENDF; EIF: function 1 by the first size.
        let fpgm = [
            0x4B, 0xB0, 12, 0x50, 0x58, 0xB0, 1, 0x2C, 0xB0, 111, 0x2D, 0x1B, 0xB0, 1, 0x2C, 0xB0,
            222, 0x2D, 0x59,
        ];
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        let face = tables(&MAXP, &fpgm, &[]);
        assert_eq!(function_1_at(&bytecode, face, 16), Ok(222));
        assert_eq!(function_1_at(&bytecode, face, 10), Ok(222));
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        assert_eq!(function_1_at(&bytecode, face, 10), Ok(111));
        assert_eq!(function_1_at(&bytecode, face, 16), Ok(111));
    }

    #[test]
    fn control_value_program_definitions_stay_with_the_face() {
        // prep: MPPEM; PUSHB 12; LT; IF; PUSHB 1; FDEF; PUSHB 111; ENDF; EIF:
        // function 1 only below 12 ppem.
        let prep = [
            0x4B, 0xB0, 12, 0x50, 0x58, 0xB0, 1, 0x2C, 0xB0, 111, 0x2D, 0x59,
        ];
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        let face = tables(&MAXP, &[], &prep);
        // An undefined function ends the program, leaving the point at 0.
        assert_eq!(function_1_at(&bytecode, face, 16), Ok(0));
        assert_eq!(function_1_at(&bytecode, face, 10), Ok(111));
        assert_eq!(function_1_at(&bytecode, face, 16), Ok(111));
    }

    #[test]
    fn a_failed_font_program_fails_every_size() {
        // fpgm: MPPEM; PUSHB 12; LT; IF; ELSE; an undefined opcode; EIF.
        let fpgm = [0x4B, 0xB0, 12, 0x50, 0x58, 0x1B, 0x28, 0x59];
        let face = tables(&MAXP, &fpgm, &[]);
        let failed = Err(Error::Invalid("TrueType font or control value program"));
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        assert_eq!(function_1_at(&bytecode, face, 16), failed);
        assert_eq!(function_1_at(&bytecode, face, 10), failed);
        let bytecode = Bytecode::new(Budget::new(1 << 20).unwrap());
        assert_eq!(function_1_at(&bytecode, face, 10), Ok(0));
        assert_eq!(function_1_at(&bytecode, face, 16), Ok(0));
    }
}
