//! The `unicode` operation's values: a code point's properties read through
//! triergon-shape's typed API, numbered as HarfBuzz's and FriBidi's C APIs
//! number them, in the order `harness/reference_shape.h` records them.

use triergon_shape::Script;
use triergon_shape::bidi::{self, BidiClass, Bracket, JoiningType};
use triergon_shape::unicode::{self, Decomposition, GeneralCategory};

/// What HarfBuzz's default Unicode functions and FriBidi's lookups give for one
/// code point.
struct Properties {
    general_category: GeneralCategory,
    combining_class: u8,
    script: Script,
    mirroring: Option<char>,
    bidi_class: BidiClass,
    bracket: Option<Bracket>,
    mirror: Option<char>,
    joining_type: JoiningType,
    decomposition: Option<Decomposition>,
}

impl Properties {
    fn of(c: char) -> Self {
        Self {
            general_category: GeneralCategory::of(c),
            combining_class: unicode::combining_class(c),
            script: Script::of(c),
            mirroring: unicode::mirroring(c),
            bidi_class: BidiClass::of(c),
            bracket: Bracket::of(c),
            mirror: bidi::mirror(c),
            joining_type: JoiningType::of(c),
            decomposition: unicode::decompose(c),
        }
    }

    /// A surrogate, which is not a `char` but which the reference queries like
    /// any other code point: HarfBuzz's Cs and FriBidi's defaults (L, and
    /// non-joining).
    const SURROGATE: Self = Self {
        general_category: GeneralCategory::Surrogate,
        combining_class: 0,
        script: Script::UNKNOWN,
        mirroring: None,
        bidi_class: BidiClass::L,
        bracket: None,
        mirror: None,
        joining_type: JoiningType::NonJoining,
        decomposition: None,
    };
}

/// The reference's values for `cp`: the code point, `hb_unicode_general_category`,
/// `hb_unicode_combining_class`, `hb_unicode_script`, `hb_unicode_mirroring`,
/// `fribidi_get_bidi_type`, `fribidi_get_bracket`, `fribidi_get_mirror_char` (or
/// -1), `fribidi_get_joining_type` and `hb_unicode_decompose`'s two characters
/// (or -1).
pub fn point(cp: u32) -> [i64; 11] {
    let p = char::from_u32(cp).map_or(Properties::SURROGATE, Properties::of);
    let optional = |c: Option<char>| c.map_or(-1, |c| i64::from(u32::from(c)));
    let (a, b) = match p.decomposition {
        None => (-1, -1),
        Some(Decomposition::Single(a)) => (i64::from(u32::from(a)), 0),
        Some(Decomposition::Pair(a, b)) => (i64::from(u32::from(a)), i64::from(u32::from(b))),
    };
    [
        cp.into(),
        general_category(p.general_category).into(),
        p.combining_class.into(),
        u32::from_be_bytes(p.script.to_bytes()).into(),
        p.mirroring.map_or(cp, u32::from).into(),
        bidi_type(p.bidi_class).into(),
        bracket(p.bracket).into(),
        optional(p.mirror),
        joining_type(p.joining_type).into(),
        a,
        b,
    ]
}

/// `hb_unicode_general_category_t`.
const fn general_category(category: GeneralCategory) -> u8 {
    match category {
        GeneralCategory::Control => 0,
        GeneralCategory::Format => 1,
        GeneralCategory::Unassigned => 2,
        GeneralCategory::PrivateUse => 3,
        GeneralCategory::Surrogate => 4,
        GeneralCategory::LowercaseLetter => 5,
        GeneralCategory::ModifierLetter => 6,
        GeneralCategory::OtherLetter => 7,
        GeneralCategory::TitlecaseLetter => 8,
        GeneralCategory::UppercaseLetter => 9,
        GeneralCategory::SpacingMark => 10,
        GeneralCategory::EnclosingMark => 11,
        GeneralCategory::NonSpacingMark => 12,
        GeneralCategory::DecimalNumber => 13,
        GeneralCategory::LetterNumber => 14,
        GeneralCategory::OtherNumber => 15,
        GeneralCategory::ConnectPunctuation => 16,
        GeneralCategory::DashPunctuation => 17,
        GeneralCategory::ClosePunctuation => 18,
        GeneralCategory::FinalPunctuation => 19,
        GeneralCategory::InitialPunctuation => 20,
        GeneralCategory::OtherPunctuation => 21,
        GeneralCategory::OpenPunctuation => 22,
        GeneralCategory::CurrencySymbol => 23,
        GeneralCategory::ModifierSymbol => 24,
        GeneralCategory::MathSymbol => 25,
        GeneralCategory::OtherSymbol => 26,
        GeneralCategory::LineSeparator => 27,
        GeneralCategory::ParagraphSeparator => 28,
        GeneralCategory::SpaceSeparator => 29,
    }
}

/// `FriBidiCharType`.
const fn bidi_type(class: BidiClass) -> u32 {
    match class {
        BidiClass::L => 0x110,
        BidiClass::R => 0x111,
        BidiClass::Al => 0x113,
        BidiClass::En => 0x220,
        BidiClass::Es => 0x10420,
        BidiClass::Et => 0x20420,
        BidiClass::An => 0x222,
        BidiClass::Cs => 0x40420,
        BidiClass::Nsm => 0x80020,
        BidiClass::Bn => 0x100820,
        BidiClass::B => 0x202840,
        BidiClass::S => 0x402840,
        BidiClass::Ws => 0x800840,
        BidiClass::On => 0x40,
        BidiClass::Lre => 0x1010,
        BidiClass::Lro => 0x5010,
        BidiClass::Rle => 0x1011,
        BidiClass::Rlo => 0x5011,
        BidiClass::Pdf => 0x1020,
        BidiClass::Lri => 0x8040,
        BidiClass::Rli => 0x8041,
        BidiClass::Fsi => 0x0200_8040,
        BidiClass::Pdi => 0x8060,
    }
}

/// `fribidi_get_bracket`: the pair's opening bracket, with the top bit set for
/// an opening one; 0 for no bracket.
fn bracket(bracket: Option<Bracket>) -> u32 {
    bracket.map_or(0, |b| {
        let open = if b.opens() { 0x8000_0000 } else { 0 };
        u32::from(b.pair()) | open
    })
}

/// `FriBidiJoiningType`.
const fn joining_type(joining: JoiningType) -> u32 {
    match joining {
        JoiningType::NonJoining => 0x0,
        JoiningType::RightJoining => 0x5,
        JoiningType::DualJoining => 0x7,
        JoiningType::JoinCausing => 0x3,
        JoiningType::Transparent => 0xC,
        JoiningType::LeftJoining => 0x6,
        JoiningType::Ignored => 0x10,
    }
}
