//! Styles and the values styles, override tags and glyphs share.

use std::ops::{Index, IndexMut};

use triergon_core::{Budget, Result};

use crate::number::{ffmax, header_int, strtod};
use crate::strings::Strings;

/// A colour as libass holds it: red in the top byte, then green and blue, and
/// alpha in the low byte, where 0 is opaque. Scripts write `&HAABBGGRR`; the
/// byte order is swapped on reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgba(pub u32);

impl Rgba {
    /// The red component.
    pub const fn r(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// The green component.
    pub const fn g(self) -> u8 {
        (self.0 >> 16) as u8
    }

    /// The blue component.
    pub const fn b(self) -> u8 {
        (self.0 >> 8) as u8
    }

    /// Transparency: 0 is opaque, 255 invisible.
    pub const fn a(self) -> u8 {
        self.0 as u8
    }

    /// The colour with its alpha replaced.
    #[must_use]
    pub const fn with_alpha(self, alpha: u8) -> Self {
        Self(self.0 & 0xffff_ff00 | alpha as u32)
    }
}

/// One of the four colours of a style or glyph, in the order `\1c` to `\4c`
/// and `\1a` to `\4a` address them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColourSlot {
    /// The fill (`\1c`).
    Primary,
    /// The karaoke fill before its syllable is sung (`\2c`).
    Secondary,
    /// The border (`\3c`).
    Outline,
    /// The shadow, or the box of an opaque-box border (`\4c`).
    Back,
}

impl ColourSlot {
    /// Every slot, in tag order.
    pub const ALL: [Self; 4] = [Self::Primary, Self::Secondary, Self::Outline, Self::Back];
}

/// The four colours of a style or glyph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Colours {
    /// The fill.
    pub primary: Rgba,
    /// The karaoke fill before its syllable is sung.
    pub secondary: Rgba,
    /// The border.
    pub outline: Rgba,
    /// The shadow, or the box of an opaque-box border.
    pub back: Rgba,
}

impl Index<ColourSlot> for Colours {
    type Output = Rgba;

    fn index(&self, slot: ColourSlot) -> &Rgba {
        match slot {
            ColourSlot::Primary => &self.primary,
            ColourSlot::Secondary => &self.secondary,
            ColourSlot::Outline => &self.outline,
            ColourSlot::Back => &self.back,
        }
    }
}

impl IndexMut<ColourSlot> for Colours {
    fn index_mut(&mut self, slot: ColourSlot) -> &mut Rgba {
        match slot {
            ColourSlot::Primary => &mut self.primary,
            ColourSlot::Secondary => &mut self.secondary,
            ColourSlot::Outline => &mut self.outline,
            ColourSlot::Back => &mut self.back,
        }
    }
}

/// The horizontal part of an [`Alignment`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HAlign {
    /// Neither left, centre nor right, as a v4+ style alignment of 0 or a style
    /// override can leave it.
    None,
    /// Left.
    Left,
    /// Centred.
    Center,
    /// Right.
    Right,
}

/// The vertical part of an [`Alignment`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VAlign {
    /// The subtitle position, at the bottom.
    Bottom,
    /// Top.
    Top,
    /// Centred.
    Middle,
    /// Both of libass's vertical bits, which only a style override sets and
    /// libass places as neither.
    TopAndMiddle,
}

/// Where an event is anchored: libass keeps it as two bits of horizontal and
/// two of vertical alignment, which is all it reads of the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Alignment {
    /// Left, centre or right.
    pub horizontal: HAlign,
    /// Bottom, middle or top.
    pub vertical: VAlign,
}

impl Alignment {
    /// The alignment in libass's bits, which is also VSFilter's legacy `\a`
    /// numbering: 1 to 3 at the bottom, 5 to 7 at the top and 9 to 11 in the
    /// middle. Bits above the lowest four are ignored, as libass ignores them.
    pub const fn from_bits(bits: i32) -> Self {
        let horizontal = match bits & 0b11 {
            0 => HAlign::None,
            1 => HAlign::Left,
            2 => HAlign::Center,
            _ => HAlign::Right,
        };
        let vertical = match bits & 0b1100 {
            0 => VAlign::Bottom,
            4 => VAlign::Top,
            8 => VAlign::Middle,
            _ => VAlign::TopAndMiddle,
        };
        Self {
            horizontal,
            vertical,
        }
    }

    /// The alignment from numeric-keypad numbering, as `\an` and v4+ styles
    /// write it (libass's `numpad2align`): 1 to 9, negative values by their
    /// magnitude, larger values wrapping horizontally and staying at the top.
    pub const fn from_numpad(value: i32) -> Self {
        let value = if value < -i32::MAX {
            2
        } else {
            value.wrapping_abs()
        };
        // `(value - 1) % 3` truncates toward zero, so 0 gives no horizontal bits.
        let horizontal = (value - 1) % 3 + 1;
        let vertical = if value <= 3 {
            0
        } else if value <= 6 {
            8
        } else {
            4
        };
        Self::from_bits(horizontal | vertical)
    }

    /// The alignment in libass's bits.
    pub const fn bits(self) -> i32 {
        let horizontal = match self.horizontal {
            HAlign::None => 0,
            HAlign::Left => 1,
            HAlign::Center => 2,
            HAlign::Right => 3,
        };
        let vertical = match self.vertical {
            VAlign::Bottom => 0,
            VAlign::Top => 4,
            VAlign::Middle => 8,
            VAlign::TopAndMiddle => 12,
        };
        horizontal | vertical
    }
}

/// Text justification independent of alignment, which only a style override
/// sets (`Justify`, libass's `ASS_JUSTIFY_*`). Values libass does not name act
/// as [`Justify::Auto`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Justify {
    /// Lines are justified as the event is aligned.
    #[default]
    Auto,
    /// Lines start at the left of the event's box.
    Left,
    /// Lines are centred in the event's box.
    Center,
    /// Lines end at the right of the event's box.
    Right,
}

impl Justify {
    /// The justification of libass's value.
    pub const fn from_value(value: i32) -> Self {
        match value {
            1 => Self::Left,
            2 => Self::Center,
            3 => Self::Right,
            _ => Self::Auto,
        }
    }
}

/// A `Bold` field or `\b` value as libass keeps it: 0 regular, 1 bold, or a
/// weight such as 100 or 900. Glyph runs split where it changes, so 1 and 700
/// are different values even though both ask for weight 700.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bold(pub i32);

impl Bold {
    /// The font weight libass requests (`ass_update_font`): 700 for 1 and -1,
    /// 400 for 0, else the value read as unsigned.
    pub const fn weight(self) -> u32 {
        match self.0 {
            1 | -1 => 700,
            0 => 400,
            value => value as u32,
        }
    }
}

/// An `Italic` field or `\i` value as libass keeps it: 0 upright, 1 italic, or
/// any other value a style override sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Italic(pub i32);

impl Italic {
    /// The slant libass requests (`ass_update_font`): 100 for 1, 0 for 0, else
    /// the value read as unsigned.
    pub const fn slant(self) -> u32 {
        match self.0 {
            1 => 100,
            value => value as u32,
        }
    }
}

/// A `BorderStyle` value as libass keeps it. 3 draws an opaque box for each
/// glyph and 4 a background box behind the event; every other value draws an
/// outline. Glyph runs split where the value changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BorderStyle(pub i32);

impl BorderStyle {
    /// An outline and a drop shadow, libass's default.
    pub const OUTLINE: Self = Self(1);
    /// An opaque box around each glyph in the outline colour.
    pub const OPAQUE_BOX: Self = Self(3);
    /// One box in the back colour behind the whole event.
    pub const BACKGROUND_BOX: Self = Self(4);
}

/// Left, right and vertical margins in script pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Margins {
    /// From the left edge.
    pub left: i32,
    /// From the right edge.
    pub right: i32,
    /// From the bottom or top edge, by the vertical alignment.
    pub vertical: i32,
}

/// One style of a script.
#[derive(Clone, Debug)]
pub struct Style {
    strings: Strings<2>,
    /// The font size in script pixels.
    pub font_size: f64,
    /// The four colours.
    pub colours: Colours,
    /// The weight.
    pub bold: Bold,
    /// The slant.
    pub italic: Italic,
    /// Underlined.
    pub underline: bool,
    /// Struck out.
    pub strike_out: bool,
    /// Horizontal scale; 1.0 is 100%.
    pub scale_x: f64,
    /// Vertical scale; 1.0 is 100%.
    pub scale_y: f64,
    /// Extra space between letters in script pixels.
    pub spacing: f64,
    /// The `\frz` rotation in degrees.
    pub angle: f64,
    /// The kind of border.
    pub border_style: BorderStyle,
    /// The border width in script pixels.
    pub outline: f64,
    /// The shadow depth in script pixels.
    pub shadow: f64,
    /// Where events are anchored.
    pub alignment: Alignment,
    /// The margins events use where theirs are 0.
    pub margins: Margins,
    /// The font charset, as Windows numbers it; libass's value -1 asks for the
    /// base direction to be found from the text.
    pub encoding: i32,
    /// The default `\blur`; only a style override sets it.
    pub blur: f64,
    /// Justification within the event; only a style override sets it.
    pub justify: Justify,
}

/// The fields a style line or a style override can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Name,
    FontName,
    Colour(ColourSlot),
    AlphaLevel,
    FontSize,
    Bold,
    Italic,
    Underline,
    StrikeOut,
    Spacing,
    Angle,
    BorderStyle,
    Alignment,
    MarginL,
    MarginR,
    MarginV,
    Encoding,
    ScaleX,
    ScaleY,
    Outline,
    Shadow,
    Blur,
    Justify,
}

impl Field {
    /// The field `name` designates, ignoring ASCII case.
    pub(crate) fn named(name: &[u8]) -> Option<Self> {
        const NAMES: [(&[u8], Field); 26] = [
            (b"name", Field::Name),
            (b"fontname", Field::FontName),
            (b"primarycolour", Field::Colour(ColourSlot::Primary)),
            (b"secondarycolour", Field::Colour(ColourSlot::Secondary)),
            (b"outlinecolour", Field::Colour(ColourSlot::Outline)),
            (b"backcolour", Field::Colour(ColourSlot::Back)),
            (b"alphalevel", Field::AlphaLevel),
            (b"fontsize", Field::FontSize),
            (b"bold", Field::Bold),
            (b"italic", Field::Italic),
            (b"underline", Field::Underline),
            (b"strikeout", Field::StrikeOut),
            (b"spacing", Field::Spacing),
            (b"angle", Field::Angle),
            (b"borderstyle", Field::BorderStyle),
            (b"alignment", Field::Alignment),
            (b"marginl", Field::MarginL),
            (b"marginr", Field::MarginR),
            (b"marginv", Field::MarginV),
            (b"encoding", Field::Encoding),
            (b"scalex", Field::ScaleX),
            (b"scaley", Field::ScaleY),
            (b"outline", Field::Outline),
            (b"shadow", Field::Shadow),
            (b"blur", Field::Blur),
            (b"justify", Field::Justify),
        ];
        NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, field)| field)
    }
}

/// libass's `parse_color_header`: a header integer with its bytes swapped.
pub(crate) fn header_colour(s: &[u8]) -> Rgba {
    Rgba((header_int(s) as u32).swap_bytes())
}

impl Style {
    /// The style libass gives every track before its own styles, from
    /// VSFilter's defaults.
    pub(crate) fn libass_default(budget: &Budget) -> Result<Self> {
        let mut style = Self::empty(budget, b"Default", b"Arial")?;
        style.font_size = 18.0;
        style.colours = Colours {
            primary: Rgba(0xffff_ff00),
            secondary: Rgba(0x00ff_ff00),
            outline: Rgba(0),
            back: Rgba(0x0000_0080),
        };
        style.bold = Bold(200);
        style.scale_x = 1.0;
        style.scale_y = 1.0;
        style.border_style = BorderStyle::OUTLINE;
        style.outline = 2.0;
        style.shadow = 3.0;
        style.alignment = Alignment::from_bits(2);
        style.margins = Margins {
            left: 20,
            right: 20,
            vertical: 20,
        };
        Ok(style)
    }

    /// A zeroed style, as libass allocates one.
    pub(crate) fn empty(budget: &Budget, name: &[u8], font_name: &[u8]) -> Result<Self> {
        Ok(Self {
            strings: Strings::new(budget, [Some(name), Some(font_name)])?,
            font_size: 0.0,
            colours: Colours::default(),
            bold: Bold(0),
            italic: Italic(0),
            underline: false,
            strike_out: false,
            scale_x: 0.0,
            scale_y: 0.0,
            spacing: 0.0,
            angle: 0.0,
            border_style: BorderStyle(0),
            outline: 0.0,
            shadow: 0.0,
            alignment: Alignment::from_bits(0),
            margins: Margins::default(),
            encoding: 0,
            blur: 0.0,
            justify: Justify::Auto,
        })
    }

    /// The style's name, which events and `\r` look it up by.
    pub fn name(&self) -> &[u8] {
        self.strings.get(0).unwrap_or_default()
    }

    /// The font family.
    pub fn font_name(&self) -> &[u8] {
        self.strings.get(1).unwrap_or_default()
    }

    pub(crate) fn set_names(&mut self, budget: &Budget, name: &[u8], font: &[u8]) -> Result<()> {
        self.strings = Strings::new(budget, [Some(name), Some(font)])?;
        Ok(())
    }

    pub(crate) fn set_font_name(&mut self, budget: &Budget, value: &[u8]) -> Result<()> {
        self.strings = self.strings.with(budget, 1, value)?;
        Ok(())
    }

    /// libass's `set_style_alpha`: clamped alphas for the primary, secondary
    /// and outline colours and for the back colour.
    pub(crate) fn set_alpha(&mut self, front: i32, back: i32) {
        let front = front.clamp(0, 255) as u8;
        let back = back.clamp(0, 255) as u8;
        let colours = &mut self.colours;
        for slot in [
            ColourSlot::Primary,
            ColourSlot::Secondary,
            ColourSlot::Outline,
        ] {
            colours[slot] = colours[slot].with_alpha(front);
        }
        colours.back = colours.back.with_alpha(back);
    }

    /// Assigns a numeric field from its text, as a style line or override does
    /// (string fields and the alpha level are the caller's). Values are kept
    /// as written; [`Style::finish_line`] applies a style line's conversions.
    pub(crate) fn assign(&mut self, field: Field, token: &[u8]) {
        let float = || strtod(token).0;
        let int = || header_int(token);
        match field {
            Field::Colour(slot) => self.colours[slot] = header_colour(token),
            Field::FontSize => self.font_size = float(),
            Field::Bold => self.bold = Bold(int()),
            Field::Italic => self.italic = Italic(int()),
            Field::Underline => self.underline = int() != 0,
            Field::StrikeOut => self.strike_out = int() != 0,
            Field::Spacing => self.spacing = float(),
            Field::Angle => self.angle = float(),
            Field::BorderStyle => self.border_style = BorderStyle(int()),
            Field::Alignment => self.alignment = Alignment::from_bits(int()),
            Field::MarginL => self.margins.left = int(),
            Field::MarginR => self.margins.right = int(),
            Field::MarginV => self.margins.vertical = int(),
            Field::Encoding => self.encoding = int(),
            Field::ScaleX => self.scale_x = float(),
            Field::ScaleY => self.scale_y = float(),
            Field::Outline => self.outline = float(),
            Field::Shadow => self.shadow = float(),
            Field::Blur => self.blur = float(),
            Field::Justify => self.justify = Justify::from_value(int()),
            Field::Name | Field::FontName | Field::AlphaLevel => {}
        }
    }

    /// The normalisation libass applies after reading a style line.
    pub(crate) fn finish_line(&mut self) {
        self.scale_x = ffmax(self.scale_x, 0.0) / 100.0;
        self.scale_y = ffmax(self.scale_y, 0.0) / 100.0;
        self.spacing = ffmax(self.spacing, 0.0);
        self.outline = ffmax(self.outline, 0.0);
        self.shadow = ffmax(self.shadow, 0.0);
        self.bold = Bold(i32::from(self.bold.0 != 0));
        self.italic = Italic(i32::from(self.italic.0 != 0));
    }
}

#[cfg(test)]
mod tests {
    use super::{Alignment, Bold, HAlign, Italic, Justify, VAlign};

    #[test]
    fn numpad_alignment_converts_as_libass() {
        let bits = |value| Alignment::from_numpad(value).bits();
        assert_eq!(bits(1), 1);
        assert_eq!(bits(5), 10);
        assert_eq!(bits(9), 7);
        assert_eq!(bits(-7), 5);
        assert_eq!(bits(0), 0);
        assert_eq!(bits(i32::MIN), 2);
        assert_eq!(bits(12), 7);
        let centre = Alignment::from_bits(10);
        assert_eq!(
            (centre.horizontal, centre.vertical),
            (HAlign::Center, VAlign::Middle)
        );
        assert_eq!(Alignment::from_bits(16 + 12).bits(), 12);
    }

    #[test]
    fn weights_and_slants_follow_ass_update_font() {
        assert_eq!(Bold(1).weight(), 700);
        assert_eq!(Bold(-1).weight(), 700);
        assert_eq!(Bold(0).weight(), 400);
        assert_eq!(Bold(300).weight(), 300);
        assert_eq!(Bold(-2).weight(), u32::MAX - 1);
        assert_eq!(Italic(1).slant(), 100);
        assert_eq!(Italic(0).slant(), 0);
        assert_eq!(Justify::from_value(7), Justify::Auto);
    }
}
