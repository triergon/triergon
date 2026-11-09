//! Page presentation (EN 300 706 sections 12 and 13): the spacing attributes of
//! Level 1 and the X/26 enhancements of Levels 1.5 and 2.5, with the local
//! objects of Level 2.5, applied to a page.

use super::charset::{CharacterSet, compose};
use super::coding::odd_parity;
use super::packet::Control;
use super::page::{Page, TRIPLETS, Triplet};
use super::presentation::Remapping;

/// Character rows of a page: the header row and rows 1 to 24.
pub(crate) const ROWS: usize = 25;
/// Character columns of a row.
pub(crate) const COLUMNS: usize = 40;

/// What a character position shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Glyph {
    Char(char),
    /// A G1 block mosaic, code 0x20-0x3F or 0x60-0x7F.
    Mosaic {
        code: u8,
        separated: bool,
    },
    /// A G3 smoothed mosaic or line drawing character, code 0x20-0x7F.
    Smooth(u8),
    /// A diacritical mark on a letter Unicode has no precomposed form for.
    Uncomposable,
}

impl Glyph {
    pub(crate) const SPACE: Self = Self::Char(' ');
}

/// A colour of the colour lookup tables 0 to 3, 0 to 31; the Level 1 colours
/// are 0 (black) to 7 (white) in the order of the alphanumeric colour codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colour(pub u8);

impl Colour {
    pub(crate) const BLACK: Self = Self(0);
    pub(crate) const WHITE: Self = Self(7);
}

/// One dimension of a character's size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Part {
    #[default]
    Single,
    /// The upper (or left) half of a doubled character.
    First,
    /// The lower (or right) half of a doubled character.
    Second,
}

/// The size of a character and the part of it a position shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Size {
    pub width: Part,
    pub height: Part,
}

impl Size {
    const NORMAL: Self = Self {
        width: Part::Single,
        height: Part::Single,
    };

    fn is_double_height(self) -> bool {
        self.height != Part::Single
    }
}

/// One character position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    pub glyph: Glyph,
    pub foreground: Colour,
    pub background: Colour,
    pub size: Size,
    pub flash: bool,
    pub conceal: bool,
    /// Inside a box (start box ... end box).
    pub boxed: bool,
    /// Shown with foreground and background exchanged (X/26 invert, an odd
    /// number of times).
    pub inverted: bool,
}

impl Cell {
    /// A space, white on black.
    pub(crate) const BLANK: Self = Self {
        glyph: Glyph::SPACE,
        foreground: Colour::WHITE,
        background: Colour::BLACK,
        size: Size::NORMAL,
        flash: false,
        conceal: false,
        boxed: false,
        inverted: false,
    };

    /// The foreground and background colours the position shows.
    #[must_use]
    pub(crate) fn shown_colours(&self) -> (Colour, Colour) {
        if self.inverted {
            (self.background, self.foreground)
        } else {
            (self.foreground, self.background)
        }
    }

    /// The position below this one on a row following a double-height row:
    /// the lower half of a double-height character, else a space in its
    /// colours.
    fn lower_half(self) -> Self {
        if self.size.is_double_height() {
            Self {
                size: Size {
                    height: Part::Second,
                    ..self.size
                },
                ..self
            }
        } else {
            Self {
                glyph: Glyph::SPACE,
                size: Size::NORMAL,
                ..self
            }
        }
    }
}

/// The character sets a page is presented with: the default set and the
/// second G0 set the escape code switches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Charsets {
    pub primary: CharacterSet,
    pub secondary: CharacterSet,
}

/// How a page is presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Style {
    pub charsets: Charsets,
    /// The tables the Level 1 colour codes select colours from.
    pub remapping: Remapping,
    /// Whether the X/26 enhancements apply; a Level 1 presentation ignores
    /// them.
    pub enhanced: bool,
}

/// A presented page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Display {
    pub rows: [[Cell; COLUMNS]; ROWS],
    pub control: Control,
    /// Whether the X/26 enhancements were presented: the style asked for
    /// them and the page did not fall back to Level 1.
    pub enhanced: bool,
    /// The positions whose boxing the enhancements set.
    enhanced_boxes: [[bool; COLUMNS]; ROWS],
}

/// The Level 1 codes of each row; the rows below double-height rows show
/// none of their own, but their codes still end enhancement attributes.
type Codes = [[u8; COLUMNS]; ROWS];

impl Display {
    /// Presents `page` in `style`. The transmission leaves columns 0 to 7 of
    /// the header row to the receiver, which supplies their codes in
    /// `header_start`, usually showing the page number.
    ///
    /// Enhancements that need what the page cannot supply, a DRCS character
    /// (mode 0x0D, characters 0 to 47) or an object of an object page, or
    /// that hold a row triplet of mode 0x0B, present the page at Level 1, as
    /// libzvbi does. Only the triplets presented count: those of the page up
    /// to its first object definition, and those of the objects it invokes.
    #[must_use]
    pub(crate) fn present(page: &Page, style: Style, header_start: &[u8; 8]) -> Self {
        let (mut display, mut codes) = Self::level_1(page, style, header_start);
        let mut placements = Placements::default();
        if style.enhanced {
            // The receiver's columns of the header row hold no codes that end
            // enhancement attributes.
            codes[0][..8].fill(0x20);
            let mut canvas = Canvas {
                rows: &mut display.rows,
                boxes: &mut display.enhanced_boxes,
                codes: &codes,
                placements: &mut placements,
                set: style.charsets.primary,
            };
            if canvas.enhance(page).is_some() {
                display.enhanced = true;
            } else {
                (display, _) = Self::level_1(page, style, header_start);
                placements = Placements::default();
            }
        }
        // Double-width and double-height characters, of Level 1 or sized by
        // the enhancements, cover their lower halves (rows 0 to 22), then
        // their right halves. The lower half of a mosaic the enhancements did
        // not size is not inverted.
        for row in 0..ROWS {
            if (1..ROWS - 1).contains(&row) {
                for column in 0..COLUMNS {
                    let above = display.rows[row - 1][column];
                    let lower = match placements.lowers[row - 1][column] {
                        Some(lower) => lower,
                        None if above.size.height == Part::First => {
                            let mosaic =
                                matches!(above.glyph, Glyph::Mosaic { .. } | Glyph::Smooth(_));
                            Cell {
                                inverted: above.inverted
                                    && (placements.sized[row - 1][column] || !mosaic),
                                ..above.lower_half()
                            }
                        }
                        None => continue,
                    };
                    display.rows[row][column] = lower;
                    display.enhanced_boxes[row][column] = display.enhanced_boxes[row - 1][column];
                }
            }
            display.cover_right_halves(row);
        }
        display
    }

    /// The page at Level 1 before double sizes cover their halves, and the
    /// codes of its rows.
    fn level_1(page: &Page, style: Style, header_start: &[u8; 8]) -> (Self, Codes) {
        let mut display = Self {
            rows: [[Cell::BLANK; COLUMNS]; ROWS],
            control: page.header.control,
            enhanced: false,
            enhanced_boxes: [[false; COLUMNS]; ROWS],
        };
        let mut codes: Codes = [[0x20; COLUMNS]; ROWS];
        codes[0][..8].copy_from_slice(header_start);
        for (code, &byte) in codes[0][8..].iter_mut().zip(&page.header.text) {
            *code = odd_parity(byte).unwrap_or(0x20);
        }
        // The rows below double-height rows (rows 1 to 22).
        let mut lower = [false; ROWS];
        for row in 0..ROWS {
            if let Some(transmitted) = page.row(row as u8) {
                codes[row] = *transmitted;
            }
            if lower[row] {
                display.rows[row] = display.rows[row - 1].map(Cell::lower_half);
                continue;
            }
            let doubled;
            (display.rows[row], doubled) =
                present_row(&codes[row], row, style.charsets, style.remapping);
            display.cover_right_halves(row);
            if doubled && (1..ROWS - 2).contains(&row) {
                lower[row + 1] = true;
            }
        }
        (display, codes)
    }

    /// Shows the left half of each double-width character of row `row` in the
    /// position to its right.
    fn cover_right_halves(&mut self, row: usize) {
        let (cells, boxes) = (&mut self.rows[row], &mut self.enhanced_boxes[row]);
        for column in 1..COLUMNS {
            let left = cells[column - 1];
            if left.size.width == Part::First {
                cells[column] = Cell {
                    size: Size {
                        width: Part::Second,
                        ..left.size
                    },
                    ..left
                };
                boxes[column] = boxes[column - 1];
            }
        }
    }

    /// Whether a position is shown: on subtitle and newsflash pages only boxed
    /// positions are; C7 hides the header row and C10 the whole page. On
    /// those pages neither hides the positions an enhancement box reaches, but
    /// C7 the header row of a subtitle page that is not a newsflash page.
    #[must_use]
    pub(crate) fn is_visible(&self, row: usize, column: usize) -> bool {
        let control = self.control;
        let header = row == 0 && control.suppress_header;
        let hidden_row = control.inhibit_display || header;
        if !(control.subtitle || control.newsflash) {
            return !hidden_row;
        }
        self.rows[row][column].boxed
            && if self.enhanced_boxes[row][column] {
                !header || control.newsflash
            } else {
                !hidden_row
            }
    }
}

/// The page the enhancements draw on.
struct Canvas<'a> {
    rows: &'a mut [[Cell; COLUMNS]; ROWS],
    boxes: &'a mut [[bool; COLUMNS]; ROWS],
    codes: &'a Codes,
    placements: &'a mut Placements,
    /// The default set, which the page and each object start with.
    set: CharacterSet,
}

impl Canvas<'_> {
    /// The X/26 enhancements over the Level 1 page; `None` for a page to be
    /// presented at Level 1. Characters with diacritical marks and from the
    /// G1, G2 and G3 sets are placed at the active position; colours and
    /// display attributes take effect from it.
    ///
    /// The active position starts at row 0, column 0. A column triplet that
    /// takes effect moves it forward to its column, never back. A row address
    /// moves it to the row and the column its data gives: column 0 for full
    /// row colour and address display row 0 (address 63, row 0), the data for
    /// set active position (below 40).
    ///
    /// Attributes reach a position when the active position passes it: the
    /// positions before a column triplet's column, the rest of the row when
    /// the position moves down a row or the enhancements end, the positions
    /// through the active one when it moves up or along the row. Moving down
    /// clears all attributes but invert; moving up or along keeps them, so
    /// positions passed again take them again.
    ///
    /// The page's triplets end at the termination marker or the first object
    /// definition (modes 0x15 to 0x17). An invocation (modes 0x11 to 0x13)
    /// of a local object, with an address below 48, draws the triplets from
    /// the one its address and data point to, up to the next definition or
    /// termination marker, at the active position moved by the last origin
    /// modifier (mode 0x10, data below 72) since the last invocation. Objects
    /// draw as the page does, relative to their origin and with these
    /// differences: address display row 0 does nothing; an adaptive object's
    /// attributes pass the Level 1 codes and reach only through the active
    /// position; a passive object's reach only the characters it places,
    /// starting white on black and unboxed, and moving down keeps them. Each
    /// object starts with its own attributes, full row colour and default
    /// character set. Invocations in an object draw nothing, but one of a
    /// later type (adaptive in active, passive in active or adaptive) with an
    /// object page's address presents the page at Level 1.
    fn enhance(&mut self, page: &Page) -> Option<()> {
        let mut decoded = page.triplets();
        let triplets: [Option<Triplet>; TRIPLETS] = std::array::from_fn(|_| decoded.next());
        self.draw(
            &triplets,
            &triplets,
            Pen::new(Layer::Page, (0, 0), self.set),
        )
    }

    /// Draws `triplets` with `pen`; `all` holds the triplets objects are
    /// defined in. `None` when the page is to be presented at Level 1.
    fn draw(
        &mut self,
        all: &[Option<Triplet>],
        triplets: &[Option<Triplet>],
        mut pen: Pen,
    ) -> Option<()> {
        let mut modifier = None;
        for triplet in triplets.iter().map_while(|triplet| *triplet) {
            match triplet {
                Triplet::Row { mode: 0x0B, .. } => return None,
                Triplet::Column {
                    mode: 0x0D, data, ..
                } if data & 0x3F < 48 => return None,
                Triplet::Row {
                    mode: 0x15..=0x17 | 0x1F,
                    ..
                } => break,
                Triplet::Row {
                    row,
                    mode: 0x10,
                    data,
                } => {
                    if data < 72 {
                        modifier = Some((usize::from(row % 24), usize::from(data)));
                    }
                }
                Triplet::Row {
                    row,
                    mode: mode @ 0x11..=0x13,
                    data,
                } => {
                    let layer = Layer::invoked(mode);
                    // The address, row 24 being address 40.
                    let address = 40 + row % 24;
                    let local = address < 48;
                    if pen.layer != Layer::Page {
                        if layer > pen.layer && !local {
                            return None;
                        }
                        continue;
                    }
                    if !local {
                        return None;
                    }
                    let (rows, columns) = modifier.take().unwrap_or_default();
                    let origin = (pen.row + rows, pen.column + columns);
                    if let Some(object) = local_object(address, data).and_then(|i| all.get(i..)) {
                        self.draw(all, object, Pen::new(layer, origin, self.set))?;
                    }
                }
                Triplet::Row { row, mode, data } => {
                    let (row, column) = match mode {
                        0x01 => (row, 0),
                        0x04 if data < 40 => (row, data),
                        0x07 if row == 23 && pen.layer == Layer::Page => (0, 0),
                        _ => continue,
                    };
                    let (row, column) = (
                        pen.origin.0 + usize::from(row),
                        pen.origin.1 + usize::from(column),
                    );
                    if row > pen.row && !pen.row_colour.down {
                        pen.row_colour = RowColour::BLACK;
                    }
                    if mode != 0x04
                        && let Some(colour) = RowColour::from_data(data)
                    {
                        pen.row_colour = colour;
                    }
                    if row > pen.row {
                        let end = pen.row_end();
                        self.paint(&mut pen, end);
                        if pen.layer != Layer::Passive {
                            pen.attributes = Attributes {
                                inverted: pen.attributes.inverted,
                                ..Attributes::default()
                            };
                        }
                    } else {
                        let end = pen.column + 1;
                        self.paint(&mut pen, end);
                    }
                    pen.move_to(row, column);
                }
                Triplet::Column {
                    column: address,
                    mode,
                    data,
                } => {
                    if !takes_position(mode, data) {
                        continue;
                    }
                    pen.column = pen.column.max(pen.origin.1 + usize::from(address));
                    if pen.column >= COLUMNS || pen.row >= ROWS {
                        continue;
                    }
                    let column = pen.column;
                    self.paint(&mut pen, column);
                    if mode == 0x08 {
                        // Modified G0 and G2 character set designation.
                        pen.set = CharacterSet::from_designation(data).unwrap_or(pen.set);
                    } else if !pen.attributes.set(mode, data)
                        && let Some(glyph) = placed_glyph(mode, data, pen.set)
                    {
                        let row = pen.row;
                        self.rows[row][column].glyph = glyph;
                        self.placements.waiting[row][column] = Some(pen.layer);
                        pen.placed = Some(column);
                    }
                }
            }
        }
        let end = pen.row_end();
        self.paint(&mut pen, end);
        Some(())
    }

    /// Gives the active row the pen's attributes up to column `end`; a
    /// passive object's only the character it placed there. A character
    /// placed on a position already double height shows its lower half,
    /// which the attributes reach as well, but invert not the first time.
    /// G1 mosaics, and other placed characters the first time the attributes
    /// of their page or object reach them, take the size (but in row 24), and
    /// then show their lower half as the upper one.
    fn paint(&mut self, pen: &mut Pen, end: usize) {
        let (row, start) = (pen.row, pen.painted);
        pen.painted = pen.painted.max(end);
        if row >= ROWS {
            return;
        }
        let columns = if pen.layer == Layer::Passive {
            match pen.placed.take_if(|column| (start..end).contains(column)) {
                Some(column) => column..column + 1,
                None => return,
            }
        } else {
            start..end.min(COLUMNS)
        };
        let codes = self.codes;
        let code = |column: usize| {
            let code = codes[row][column];
            (code, column > 0 && codes[row][column - 1] == code)
        };
        for column in columns {
            let effect = pen.attributes.effect(pen.row_colour.colour);
            if pen.layer.ends_at_codes() {
                let next = (column + 1 < COLUMNS).then(|| code(column + 1));
                pen.attributes.reach(code(column).0, next);
            }
            let cell = &mut self.rows[row][column];
            let lower = &mut self.placements.lowers[row][column];
            let first = self.placements.waiting[row][column]
                .take_if(|layer| *layer == pen.layer)
                .is_some();
            if first && cell.size.height == Part::First {
                *lower = Some(cell.lower_half());
            }
            if let Some(lower) = lower {
                effect.apply_to_lower_half(lower, first);
            }
            effect.apply(cell);
            self.boxes[row][column] |= effect.boxed.is_some();
            let sizable = row < ROWS - 1 && (first || matches!(cell.glyph, Glyph::Mosaic { .. }));
            if sizable && let Some(size) = effect.size {
                cell.size = size;
                *lower = None;
                self.placements.sized[row][column] = true;
            }
        }
    }
}

/// The position of the first triplet of the local object an invocation's
/// address (40 to 47) and data point to: the designation of its packet in
/// the address's bit 0 and the data's bits 4 to 6, its triplet in the
/// data's bits 0 to 3.
fn local_object(address: u8, data: u8) -> Option<usize> {
    let designation = usize::from((address & 1) << 3 | data >> 4);
    let triplet = usize::from(data & 0xF);
    (triplet < 13).then_some(designation * 13 + triplet)
}

/// The characters the enhancements placed and the positions they sized.
#[derive(Debug)]
struct Placements {
    /// Placed characters the attributes of the page or object that placed
    /// them have not reached yet.
    waiting: [[Option<Layer>; COLUMNS]; ROWS],
    /// The lower halves of characters placed in double-height positions.
    lowers: [[Option<Cell>; COLUMNS]; ROWS],
    sized: [[bool; COLUMNS]; ROWS],
}

impl Default for Placements {
    fn default() -> Self {
        Self {
            waiting: [[None; COLUMNS]; ROWS],
            lowers: [[None; COLUMNS]; ROWS],
            sized: [[false; COLUMNS]; ROWS],
        }
    }
}

/// Whether a column triplet takes effect (and the active position): colours
/// need data below 32, characters (but G1 mosaics) codes from 0x20; flash,
/// the G0 and G2 designation and the display attributes always do, the DRCS
/// characters and reserved modes never.
fn takes_position(mode: u8, data: u8) -> bool {
    match mode {
        0x00 | 0x03 => data < 32,
        0x01 | 0x07 | 0x08 | 0x0C => true,
        0x02 | 0x09 | 0x0B | 0x0F..=0x1F => data >= 0x20,
        _ => false,
    }
}

/// What draws enhancements: the page itself or an object it invokes, in the
/// order of the object types (EN 300 706 section 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    Page,
    Active,
    Adaptive,
    Passive,
}

impl Layer {
    /// The type an invocation (mode 0x11 to 0x13) draws.
    fn invoked(mode: u8) -> Self {
        match mode {
            0x11 => Self::Active,
            0x12 => Self::Adaptive,
            _ => Self::Passive,
        }
    }

    /// Whether the Level 1 codes end its attributes.
    fn ends_at_codes(self) -> bool {
        matches!(self, Self::Page | Self::Active)
    }
}

/// The active position of the page or an object and the attributes in
/// effect from it; the positions before `painted` have them.
#[derive(Clone, Copy, Debug)]
struct Pen {
    layer: Layer,
    /// The row and column the object's addresses count from.
    origin: (usize, usize),
    row: usize,
    column: usize,
    painted: usize,
    attributes: Attributes,
    /// The position of the last character placed, until a passive object's
    /// attributes reach it.
    placed: Option<usize>,
    row_colour: RowColour,
    /// The set G0 and G2 characters are placed from.
    set: CharacterSet,
}

impl Pen {
    fn new(layer: Layer, origin: (usize, usize), set: CharacterSet) -> Self {
        let mut attributes = Attributes::default();
        if layer == Layer::Passive {
            attributes.foreground = Some(Ink::Colour(Colour::WHITE));
            attributes.background = Some(Ink::Colour(Colour::BLACK));
            attributes.boxed = Some(false);
        }
        Self {
            layer,
            origin,
            row: origin.0,
            column: origin.1,
            painted: origin.1,
            attributes,
            placed: None,
            row_colour: RowColour::BLACK,
            set,
        }
    }

    /// How far the attributes reach on the row the active position leaves:
    /// the page's and an active object's to its end, the others' through the
    /// active position.
    fn row_end(&self) -> usize {
        if self.layer.ends_at_codes() {
            COLUMNS
        } else {
            self.column + 1
        }
    }

    fn move_to(&mut self, row: usize, column: usize) {
        self.row = row;
        self.column = column;
        self.painted = column;
    }
}

/// The full row colour (mode 0x01, and 0x07 for row 0), which colour 8
/// stands for outside an enhancement box. A colour for this row lasts until
/// the active position moves down, one for this row and those below
/// until the next.
#[derive(Clone, Copy, Debug)]
struct RowColour {
    colour: Colour,
    down: bool,
}

impl RowColour {
    const BLACK: Self = Self {
        colour: Colour::BLACK,
        down: false,
    };

    /// The colour of a triplet's data: the colour in bits 0 to 4, this row
    /// (bits 5 and 6 clear) or this row and those below (both set).
    fn from_data(data: u8) -> Option<Self> {
        let colour = Colour(data & 0x1F);
        match data >> 5 {
            0 => Some(Self {
                colour,
                down: false,
            }),
            3 => Some(Self { colour, down: true }),
            _ => None,
        }
    }
}

/// A colour an X/26 triplet sets: colour 8, transparent inside an
/// enhancement box, stands for the full row colour elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Colour(Colour),
    RowColour,
}

impl Ink {
    fn colour(self, row_colour: Colour) -> Colour {
        match self {
            Self::Colour(colour) => colour,
            Self::RowColour => row_colour,
        }
    }
}

/// The colours and display attributes X/26 triplets set. Each lasts until
/// the Level 1 code that sets it: a foreground colour through the next colour
/// code, a background colour up to the next background code, boxing up to
/// the second code of the next box. The codes end them as the positions are
/// passed, so the code of the position an attribute is set at does not.
#[derive(Clone, Copy, Debug, Default)]
struct Attributes {
    foreground: Option<Ink>,
    background: Option<Ink>,
    boxed: Option<bool>,
    /// Exchanges foreground and background of each position it reaches,
    /// again on a second pass.
    inverted: bool,
    /// The size of the placed characters it reaches, from the double height
    /// and double width bits; without them their size stays.
    size: Option<Size>,
    /// Whether the G1 mosaics it reaches are separated.
    separated: Option<bool>,
}

impl Attributes {
    /// Takes the attribute of a column triplet that takes effect and reports
    /// whether it was one: a colour, or the boxing, invert, separated mosaic
    /// and size bits of the display attributes, whose other bits change
    /// nothing.
    fn set(&mut self, mode: u8, data: u8) -> bool {
        let colour = Some(if data == 8 && self.boxed != Some(true) {
            Ink::RowColour
        } else {
            Ink::Colour(Colour(data))
        });
        match mode {
            0x00 => self.foreground = colour,
            0x03 => self.background = colour,
            0x0C => {
                let part = |bit: u8| {
                    if data & bit != 0 {
                        Part::First
                    } else {
                        Part::Single
                    }
                };
                self.boxed = Some(data & 0x02 != 0);
                self.inverted = data & 0x10 != 0;
                self.separated = Some(data & 0x20 != 0);
                self.size = (data & 0x41 != 0).then(|| Size {
                    width: part(0x40),
                    height: part(0x01),
                });
            }
            _ => return false,
        }
        true
    }

    /// What the attributes do to a position on a row of colour `row_colour`.
    fn effect(&self, row_colour: Colour) -> Effect {
        Effect {
            foreground: self.foreground.map(|ink| ink.colour(row_colour)),
            background: self.background.map(|ink| ink.colour(row_colour)),
            boxed: self.boxed,
            inverted: self.inverted,
            size: self.size,
            separated: self.separated,
        }
    }

    /// Passes a position holding the Level 1 code `code`, followed by the
    /// code `next` (and whether it repeats `code`): a colour code ends the
    /// foreground colour after its position, a background code the
    /// background colour and the second code of a box the boxing before
    /// theirs.
    fn reach(&mut self, code: u8, next: Option<(u8, bool)>) {
        if matches!(code, 0x00..=0x07 | 0x10..=0x17) {
            self.foreground = None;
        }
        match next {
            Some((0x1C | 0x1D, _)) => self.background = None,
            Some((0x0A | 0x0B, true)) => self.boxed = None,
            _ => {}
        }
    }
}

/// What the attributes do to one position.
#[derive(Clone, Copy, Debug)]
struct Effect {
    foreground: Option<Colour>,
    background: Option<Colour>,
    boxed: Option<bool>,
    inverted: bool,
    size: Option<Size>,
    separated: Option<bool>,
}

impl Effect {
    fn apply(self, cell: &mut Cell) {
        if let Some(colour) = self.foreground {
            cell.foreground = colour;
        }
        if let Some(colour) = self.background {
            cell.background = colour;
        }
        if let Some(boxed) = self.boxed {
            cell.boxed = boxed;
        }
        cell.inverted ^= self.inverted;
        if let (Glyph::Mosaic { separated, .. }, Some(set)) = (&mut cell.glyph, self.separated) {
            *separated = set;
        }
    }

    /// Applies the effect to the lower half of a placed character; the
    /// first time without inverting it.
    fn apply_to_lower_half(self, cell: &mut Cell, first: bool) {
        Self {
            inverted: self.inverted && !first,
            ..self
        }
        .apply(cell);
    }
}

/// The character an X/26 triplet places with `set`, the default set or the
/// one a modified designation selected. G0 characters (and G1 codes 0x40 to
/// 0x5F) come from it without its national option, G2 characters from it;
/// characters with or without diacritical marks from the Latin set without
/// national option characters, code 0x2A standing for '@'.
fn placed_glyph(mode: u8, data: u8, set: CharacterSet) -> Option<Glyph> {
    if data < 0x20 {
        return None;
    }
    let base = CharacterSet::PLAIN_LATIN.g0_char(data);
    Some(match mode {
        0x01 if data & 0x20 != 0 => Glyph::Mosaic {
            code: data,
            separated: false,
        },
        0x01 | 0x09 => Glyph::Char(set.without_national_option().g0_char(data)),
        0x02 | 0x0B => Glyph::Smooth(data),
        0x0F => Glyph::Char(set.g2_char(data)),
        0x10 if data == 0x2A => Glyph::Char('@'),
        0x10 => Glyph::Char(base),
        0x11..=0x1F => compose(base, mode - 0x10).map_or(Glyph::Uncomposable, Glyph::Char),
        _ => return None,
    })
}

/// The attributes in effect while a row is presented. The row starts white on
/// black as remapped. The held mosaic is the last mosaic of the row in the
/// form it was shown (a contiguous blank before the first); changes of mode
/// or size do not clear it.
#[derive(Clone, Copy)]
struct RowState {
    foreground: Colour,
    background: Colour,
    mosaics: bool,
    separated: bool,
    hold: bool,
    held: Glyph,
    flash: bool,
    conceal: bool,
    boxed: bool,
    size: Size,
    escaped: bool,
    /// Whether the row may hold double-height characters.
    double_height: bool,
    remapping: Remapping,
}

impl RowState {
    const START: Self = Self {
        foreground: Colour::WHITE,
        background: Colour::BLACK,
        mosaics: false,
        separated: false,
        hold: false,
        held: Glyph::Mosaic {
            code: 0x20,
            separated: false,
        },
        flash: false,
        conceal: false,
        boxed: false,
        size: Size::NORMAL,
        escaped: false,
        double_height: true,
        remapping: Remapping::from_code(0),
    };

    /// Attributes that take effect at the position of their code; boxes start
    /// and end at the second of their two codes.
    fn set_at(&mut self, code: u8, previous: Option<u8>) {
        let doubled = previous == Some(code);
        match code {
            0x0A if doubled => self.boxed = false,
            0x0B if doubled => self.boxed = true,
            0x09 => self.flash = false,
            0x0C => self.resize(Size::NORMAL),
            0x18 => self.conceal = true,
            0x19 => self.separated = false,
            0x1A => self.separated = true,
            0x1C => self.background = self.remapping.background(0),
            0x1D => self.background = self.remapping.background(self.foreground.0),
            0x1E => self.hold = true,
            _ => {}
        }
    }

    /// Attributes that take effect at the position after their code.
    fn set_after(&mut self, code: u8) {
        match code {
            0x00..=0x07 => self.colour(code, false),
            0x08 => self.flash = true,
            0x0D if self.double_height => self.resize(Size {
                width: Part::Single,
                height: Part::First,
            }),
            0x0E => self.resize(Size {
                width: Part::First,
                height: Part::Single,
            }),
            0x0F if self.double_height => self.resize(Size {
                width: Part::First,
                height: Part::First,
            }),
            0x10..=0x17 => self.colour(code - 0x10, true),
            0x1B => self.escaped = !self.escaped,
            0x1F => self.hold = false,
            _ => {}
        }
    }

    fn colour(&mut self, code: u8, mosaics: bool) {
        self.foreground = self.remapping.foreground(code);
        self.conceal = false;
        self.mosaics = mosaics;
    }

    fn resize(&mut self, size: Size) {
        self.size = size;
    }

    fn cell(&self, glyph: Glyph) -> Cell {
        Cell {
            glyph,
            foreground: self.foreground,
            background: self.background,
            size: self.size,
            flash: self.flash,
            conceal: self.conceal,
            boxed: self.boxed,
            inverted: false,
        }
    }
}

/// Presents row `row` of codes and reports whether it holds a double-height or
/// double-size code. Rows 23 and 24 have no double height: those codes do
/// nothing there.
fn present_row(
    codes: &[u8; COLUMNS],
    row: usize,
    charsets: Charsets,
    remapping: Remapping,
) -> ([Cell; COLUMNS], bool) {
    let mut cells = [Cell::BLANK; COLUMNS];
    let mut state = RowState {
        foreground: remapping.foreground(7),
        background: remapping.background(0),
        double_height: row < ROWS - 2,
        remapping,
        ..RowState::START
    };
    let mut previous = None;
    let doubled = state.double_height && codes.iter().any(|&c| matches!(c, 0x0D | 0x0F));
    for (column, &code) in codes.iter().enumerate() {
        state.set_at(code, previous);
        let glyph = if code < 0x20 {
            if state.hold && state.mosaics {
                state.held
            } else {
                Glyph::SPACE
            }
        } else if state.mosaics && code & 0x20 != 0 {
            let mosaic = Glyph::Mosaic {
                code,
                separated: state.separated,
            };
            state.held = mosaic;
            mosaic
        } else {
            let set = if state.escaped {
                charsets.secondary
            } else {
                charsets.primary
            };
            Glyph::Char(set.g0_char(code))
        };
        cells[column] = state.cell(glyph);
        state.set_after(code);
        previous = Some(code);
    }
    (cells, doubled)
}
