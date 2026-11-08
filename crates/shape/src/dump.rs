//! The character properties the harness compares with HarfBuzz's and FriBidi's,
//! in the reference's own numbering.

use crate::unicode::{self, fribidi};

/// What HarfBuzz's default Unicode functions and FriBidi's lookups give for one
/// code point, numbered as their C APIs number them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceProperties {
    /// `hb_unicode_general_category_t`.
    pub general_category: u8,
    pub combining_class: u8,
    /// `hb_script_t`: the ISO 15924 tag as a big-endian number.
    pub script: u32,
    pub mirroring: u32,
    /// `FriBidiCharType`.
    pub bidi_type: u32,
    /// `fribidi_get_bracket`: the opening bracket, with the top bit for an opening one.
    pub bracket: u32,
    pub mirror: Option<u32>,
    /// `FriBidiJoiningType`.
    pub joining_type: u32,
    pub decomposition: Option<(u32, u32)>,
}

/// The properties of `cp`.
#[doc(hidden)]
pub fn reference_properties(cp: u32) -> ReferenceProperties {
    use fribidi::{BidiClass, JoiningType};
    let bidi_type = match fribidi::bidi_class(cp) {
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
    };
    let joining_type = match fribidi::joining_type(cp) {
        JoiningType::NonJoining => 0x0,
        JoiningType::RightJoining => 0x5,
        JoiningType::DualJoining => 0x7,
        JoiningType::JoinCausing => 0x3,
        JoiningType::Transparent => 0xC,
        JoiningType::LeftJoining => 0x6,
        JoiningType::Ignored => 0x10,
    };
    let bracket = fribidi::bracket(cp).map_or(0, |b| {
        if b.opens() {
            b.pair() | 0x8000_0000
        } else {
            b.pair()
        }
    });
    ReferenceProperties {
        general_category: unicode::general_category(cp) as u8,
        combining_class: unicode::combining_class(cp),
        script: u32::from_be_bytes(unicode::script(cp).to_bytes()),
        mirroring: unicode::mirroring(cp),
        bidi_type,
        bracket,
        mirror: fribidi::mirror(cp),
        joining_type,
        decomposition: unicode::decompose(cp),
    }
}
