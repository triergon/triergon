//! Character sets (EN 300 706 section 15): G0 primary sets with national option
//! subsets, the G2 supplementary sets and diacritical composition.
//!
//! Characters are Unicode scalar values. Glyphs Unicode does not encode (the
//! Arabic presentation forms and the Turkish currency sign) use the private-use
//! code points common to teletext software (`U+E606`.., `U+E800`).

/// A national option subset of the Latin G0 set (EN 300 706 table 36).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NationalSubset {
    English,
    German,
    SwedishFinnishHungarian,
    Italian,
    French,
    PortugueseSpanish,
    CzechSlovak,
    Polish,
    Turkish,
    SerbianCroatianSlovenian,
    Rumanian,
    Estonian,
    LettishLithuanian,
    /// No national option: the thirteen positions keep their ISO 646 characters.
    Plain,
}

/// A G0 primary character set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum G0 {
    Latin(NationalSubset),
    /// Serbian/Croatian.
    Cyrillic1,
    /// Russian/Bulgarian.
    Cyrillic2,
    /// Ukrainian.
    Cyrillic3,
    Greek,
    Arabic,
    Hebrew,
}

/// A G2 supplementary character set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum G2 {
    Latin,
    Cyrillic,
    Greek,
    Arabic,
}

/// A G0 and G2 pair, as one default character set designation selects them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharacterSet {
    pub g0: G0,
    pub g2: G2,
}

impl CharacterSet {
    /// The Latin G0 set without national option characters and the Latin G2 set.
    pub const PLAIN_LATIN: Self = Self {
        g0: G0::Latin(NationalSubset::Plain),
        g2: G2::Latin,
    };

    /// The set with the ISO 646 characters in place of a Latin national
    /// option subset.
    #[must_use]
    pub const fn without_national_option(self) -> Self {
        match self.g0 {
            G0::Latin(_) => Self {
                g0: G0::Latin(NationalSubset::Plain),
                ..self
            },
            _ => self,
        }
    }

    /// The pair of a seven-bit default G0 and G2 designation (table 32: the
    /// group in bits 3-6, the national option bits C12-C14 in bits 0-2), or
    /// `None` when the code designates nothing.
    #[must_use]
    pub fn from_designation(code: u8) -> Option<Self> {
        use NationalSubset as N;
        let latin = |subset| Self {
            g0: G0::Latin(subset),
            g2: G2::Latin,
        };
        let with_g2 = |g0, g2| Self { g0, g2 };
        Some(match code {
            0 | 16 => latin(N::English),
            1 | 9 | 17 => latin(N::German),
            2 | 10 | 18 => latin(N::SwedishFinnishHungarian),
            3 | 11 | 19 => latin(N::Italian),
            4 | 12 | 20 => latin(N::French),
            5 | 21 => latin(N::PortugueseSpanish),
            6 | 14 => latin(N::CzechSlovak),
            8 => latin(N::Polish),
            22 | 54 => latin(N::Turkish),
            29 => latin(N::SerbianCroatianSlovenian),
            31 => latin(N::Rumanian),
            32 => with_g2(G0::Cyrillic1, G2::Cyrillic),
            33 => with_g2(G0::Latin(N::German), G2::Cyrillic),
            34 => with_g2(G0::Latin(N::Estonian), G2::Cyrillic),
            35 => with_g2(G0::Latin(N::LettishLithuanian), G2::Cyrillic),
            36 => with_g2(G0::Cyrillic2, G2::Cyrillic),
            37 => with_g2(G0::Cyrillic3, G2::Cyrillic),
            38 => with_g2(G0::Latin(N::CzechSlovak), G2::Cyrillic),
            55 => with_g2(G0::Greek, G2::Greek),
            64 => with_g2(G0::Latin(N::English), G2::Arabic),
            68 => with_g2(G0::Latin(N::French), G2::Arabic),
            71 | 87 => with_g2(G0::Arabic, G2::Arabic),
            85 => with_g2(G0::Hebrew, G2::Arabic),
            _ => return None,
        })
    }

    /// The G0 character of a code 0x20 to 0x7F.
    #[must_use]
    pub fn g0_char(self, code: u8) -> char {
        let index = usize::from(code.clamp(0x20, 0x7F) - 0x20);
        match self.g0 {
            G0::Latin(subset) => NATIONAL_POSITIONS
                .iter()
                .position(|&p| p == code)
                .map_or(LATIN_G0[index], |slot| subset.chars()[slot]),
            G0::Cyrillic1 => CYRILLIC_1_G0[index],
            G0::Cyrillic2 => CYRILLIC_2_G0[index],
            G0::Cyrillic3 => CYRILLIC_3_G0[index],
            G0::Greek => GREEK_G0[index],
            G0::Arabic => ARABIC_G0[index],
            G0::Hebrew => HEBREW_G0[index],
        }
    }

    /// The G2 character of a code 0x20 to 0x7F.
    #[must_use]
    pub fn g2_char(self, code: u8) -> char {
        let index = usize::from(code.clamp(0x20, 0x7F) - 0x20);
        match self.g2 {
            G2::Latin => LATIN_G2[index],
            G2::Cyrillic => CYRILLIC_G2[index],
            G2::Greek => GREEK_G2[index],
            G2::Arabic => ARABIC_G2[index],
        }
    }
}

impl NationalSubset {
    fn chars(self) -> &'static [char; 13] {
        &NATIONAL_SUBSETS[self as usize]
    }
}

/// The precomposed character of `base` with diacritical mark `mark` (1 to 15,
/// G2 column 4), if Unicode has one in the Latin-1 and Latin Extended-A blocks.
#[must_use]
pub fn compose(base: char, mark: u8) -> Option<char> {
    let (bases, composed) = COMPOSITIONS.get(usize::from(mark).checked_sub(1)?)?;
    bases
        .chars()
        .position(|b| b == base)
        .and_then(|at| composed.chars().nth(at))
}

/// G0 positions a national option subset replaces.
const NATIONAL_POSITIONS: [u8; 13] = [
    0x23, 0x24, 0x40, 0x5B, 0x5C, 0x5D, 0x5E, 0x5F, 0x60, 0x7B, 0x7C, 0x7D, 0x7E,
];

/// The thirteen characters of each subset, in [`NationalSubset`] order.
const NATIONAL_SUBSETS: [[char; 13]; 14] = [
    chars("£$@←½→↑#—¼‖¾÷"),
    chars("#$§ÄÖÜ^_°äöüß"),
    chars("#¤ÉÄÖÅÜ_éäöåü"),
    chars("£$é°ç→↑#ùàòèì"),
    chars("éïàëêùî#èâôûç"),
    chars("ç$¡áéíóú¿üñèà"),
    chars("#ůčťžýířéáěúš"),
    chars("#ńąƵŚŁćóężśłź"),
    chars("\u{e800}ğİŞÖÇÜĞışöçü"),
    chars("#ËČĆŽÐŠëčćžðš"),
    chars("#¤ŢÂŞǍÍıţâşǎî"),
    chars("#õŠÄÖŽÜÕšäöžü"),
    chars("#$ŠėȩŽčūšąųžį"),
    chars("#¤@[\\]^_`{¦}~"),
];

const LATIN_G0: [char; 96] = chars(concat!(
    " !\"£$%&'()*+,-./0123456789:;<=>?",
    "@ABCDEFGHIJKLMNOPQRSTUVWXYZ←½→↑#",
    "—abcdefghijklmnopqrstuvwxyz¼‖¾÷■",
));

const CYRILLIC_1_G0: [char; 96] = chars(concat!(
    " !\"#$%&'()*+,-./0123456789:;<=>?",
    "ЧАБЦДЕФГХИЈКЛМНОПЌРСТУВЃЉЊЗЋЖЂШЏ",
    "чабцдефгхијклмнопќрстувѓљњзћжђш■",
));

const CYRILLIC_2_G0: [char; 96] = chars(concat!(
    " !\"#$%ы'()*+,-./0123456789:;<=>?",
    "ЮАБЦДЕФГХИЙКЛМНОПЯРСТУЖВЬЪЗШЭЩЧЫ",
    "юабцдефгхийклмнопярстужвьъзшэщч■",
));

const CYRILLIC_3_G0: [char; 96] = chars(concat!(
    " !\"#$%ï'()*+,-./0123456789:;<=>?",
    "ЮАБЦДЕФГХИЙКЛМНОПЯРСТУЖВЬІЗШЄЩЧЇ",
    "юабцдефгхийклмнопярстужвьізшєщч■",
));

const GREEK_G0: [char; 96] = chars(concat!(
    " !\"#$%&'()*+,-./0123456789:;«=»?",
    "ΐΑΒΓΔΕΖΗΘΙΚΛΜΝΞΟΠΡ\u{374}ΣΤΥΦΧΨΩΪΫάέήί",
    "ΰαβγδεζηθικλμνξοπρςστυφχψωϊϋόύώ■",
));

const ARABIC_G0: [char; 96] = chars(concat!(
    " !\"£$%\u{e606}\u{e607})(*+،-./0123456789:؛>=<؟",
    "\u{e620}\u{e621}\u{e622}\u{e623}\u{e624}\u{e625}\u{e626}\u{e627}",
    "\u{e628}\u{e629}\u{e62a}\u{e62b}\u{e62c}\u{e62d}\u{e62e}\u{e62f}",
    "\u{e630}\u{e631}\u{e632}\u{e633}\u{e634}\u{e635}\u{e636}\u{e637}",
    "\u{e638}\u{e639}\u{e63a}\u{e63b}\u{e63c}\u{e63d}\u{e63e}#",
    "\u{e640}\u{e641}\u{e642}\u{e643}\u{e644}\u{e645}\u{e646}\u{e647}",
    "\u{e648}\u{e649}\u{e64a}\u{e64b}\u{e64c}\u{e64d}\u{e64e}\u{e64f}",
    "\u{e650}\u{e651}\u{e652}\u{e653}\u{e654}\u{e655}\u{e656}\u{e657}",
    "\u{e658}\u{e659}\u{e65a}\u{e65b}\u{e65c}\u{e65d}\u{e65e}■",
));

const HEBREW_G0: [char; 96] = chars(concat!(
    " !\"#$%&'()*+,-./0123456789:;<=>?",
    "@ABCDEFGHIJKLMNOPQRSTUVWXYZ←½→↑#",
    "אבגדהוזחטיךכלםמןנסעףפץצקרשת₪‖¾÷■",
));

const LATIN_G2: [char; 96] = chars(concat!(
    "\u{a0}¡¢£$¥#§¤‘“«←↑→↓°±²³×µ¶·÷’”»¼½¾¿",
    " ˋˊˆ˜ˉ˘˙¨.˚ˏˍ˝˛ˇ—¹®©™♪₠‰ɑ   ⅛⅜⅝⅞",
    "\u{2126}ÆÐªĦ ĲĿŁØŒºÞŦŊŉĸæđðħıĳŀłøœßþŧŋ■",
));

const CYRILLIC_G2: [char; 96] = chars(concat!(
    "\u{a0}¡¢£ ¥#§ ‘“«←↑→↓°±²³×µ¶·÷’”»¼½¾¿",
    " ˋˊˆ˜ˉ˘˙¨.˚ˏˍ˝˛ˇ—¹®©™♪₠‰ɑŁłß⅛⅜⅝⅞",
    "DEFGIJKLNQRSUVWZdefgijklnqrsuvwz",
));

const GREEK_G2: [char; 96] = chars(concat!(
    "\u{a0}ab£ehi§:‘“k←↑→↓°±²³xmnp÷’”t¼½¾x",
    " ˋˊˆ˜ˉ˘˙¨.˚ˏˍ˝˛ˇ?¹®©™♪₠‰ɑΊΎΏ⅛⅜⅝⅞",
    "CDFGJLQRSUVWYZΆΉcdfgjlqrsuvwyzΈ■",
));

const ARABIC_G2: [char; 96] = chars(concat!(
    "\u{e660}\u{e661}\u{e662}\u{e663}\u{e664}\u{e665}\u{e666}\u{e667}",
    "\u{e668}\u{e669}\u{e66a}\u{e66b}\u{e66c}\u{e66d}\u{e66e}\u{e66f}",
    "\u{e670}\u{e671}\u{e672}\u{e673}\u{e674}\u{e675}\u{e676}\u{e677}",
    "\u{e678}\u{e679}\u{e67a}\u{e67b}\u{e67c}\u{e67d}\u{e67e}\u{e67f}",
    "àABCDEFGHIJKLMNOPQRSTUVWXYZëêùî\u{e75f}",
    "éabcdefghijklmnopqrstuvwxyzâôûç ",
));

/// Base letters and their precomposed forms for each diacritical mark 1 to 15.
const COMPOSITIONS: [(&str, &str); 15] = [
    ("AEIOUaeiou", "ÀÈÌÒÙàèìòù"),
    ("ACEILNORSUYZaceilnorsuyz", "ÁĆÉÍĹŃÓŔŚÚÝŹáćéíĺńóŕśúýź"),
    ("ACEGHIJOSUWYaceghijosuwy", "ÂĈÊĜĤÎĴÔŜÛŴŶâĉêĝĥîĵôŝûŵŷ"),
    ("AINOUainou", "ÃĨÑÕŨãĩñõũ"),
    ("AEIOUaeiou", "ĀĒĪŌŪāēīōū"),
    ("AEGIOUaegiou", "ĂĔĞĬŎŬăĕğĭŏŭ"),
    ("CEGIZcegz", "ĊĖĠİŻċėġż"),
    ("AEIOUYaeiouy", "ÄËÏÖÜŸäëïöüÿ"),
    ("", ""),
    ("AUau", "ÅŮåů"),
    ("CGKLNRSTcgklnrst", "ÇĢĶĻŅŖŞŢçģķļņŗşţ"),
    ("", ""),
    ("OUou", "ŐŰőű"),
    ("AEIUaeiu", "ĄĘĮŲąęįų"),
    ("CDELNRSTZcdelnrstz", "ČĎĚĽŇŘŠŤŽčďěľňřšťž"),
];

/// The characters of a UTF-8 table literal, checked for length at compile time.
const fn chars<const N: usize>(text: &str) -> [char; N] {
    let bytes = text.as_bytes();
    let mut out = ['\0'; N];
    let (mut at, mut n) = (0, 0);
    while at < bytes.len() {
        let lead = bytes[at] as u32;
        let (mut value, length) = match lead {
            0x00..=0x7F => (lead, 1),
            0xC0..=0xDF => (lead & 0x1F, 2),
            0xE0..=0xEF => (lead & 0x0F, 3),
            _ => (lead & 0x07, 4),
        };
        let mut k = 1;
        while k < length {
            value = value << 6 | (bytes[at + k] as u32 & 0x3F);
            k += 1;
        }
        out[n] = match char::from_u32(value) {
            Some(c) => c,
            None => panic!("table character"),
        };
        n += 1;
        at += length;
    }
    assert!(n == N, "table length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn national_subsets_replace_their_positions() {
        let german = CharacterSet::from_designation(1).expect("German");
        assert_eq!(german.g0_char(0x5B), 'Ä');
        assert_eq!(german.g0_char(0x41), 'A');
        let english = CharacterSet::from_designation(16).expect("English");
        assert_eq!(english.g0_char(0x23), '£');
        assert_eq!(english.g0_char(0x7F), '■');
        assert_eq!(CharacterSet::from_designation(7), None);
    }

    #[test]
    fn composes_latin_letters() {
        assert_eq!(compose('e', 2), Some('é'));
        assert_eq!(compose('z', 15), Some('ž'));
        assert_eq!(compose('q', 2), None);
        assert_eq!(compose('a', 9), None);
        assert_eq!(compose('a', 0), None);
    }
}
