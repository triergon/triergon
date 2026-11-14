//! The texts of the shaping features: authored samples per script, the scripts'
//! directions and libass's languages, and seeded random strings. Every text is
//! authored for this repository or generated from a fixed seed.

/// An authored text and the script libass would shape it with.
pub struct Sample {
    /// What the text exercises.
    pub label: &'static str,
    /// ISO 15924.
    pub script: &'static str,
    /// A language override (libass's track language), shaped in addition to the
    /// script's own language.
    pub language: Option<&'static str>,
    pub text: &'static str,
}

const fn sample(label: &'static str, script: &'static str, text: &'static str) -> Sample {
    Sample {
        label,
        script,
        language: None,
        text,
    }
}

const fn localized(
    label: &'static str,
    script: &'static str,
    language: &'static str,
    text: &'static str,
) -> Sample {
    Sample {
        label,
        script,
        language: Some(language),
        text,
    }
}

/// Authored texts. Combining sequences are spelled out with escapes.
pub const SAMPLES: &[Sample] = &[
    // Latin: ligatures, kerning, precomposed and decomposed diacritics, mark order.
    sample(
        "latin-pangram",
        "Latn",
        "The quick brown fox jumps over the lazy dog. 0123456789",
    ),
    sample(
        "latin-ligatures",
        "Latn",
        "fi ffi ffl ff fl office affluent difficult waffle fjord",
    ),
    sample(
        "latin-kerning",
        "Latn",
        "AV To Wa LT Yoyo AVAWAY Tr. P, F. \"Ty\" L'Y V.",
    ),
    sample(
        "latin-precomposed",
        "Latn",
        "café naïve résumé Ångström Øre façade Ŝ ĉ ŭ",
    ),
    sample(
        "latin-decomposed",
        "Latn",
        "cafe\u{301} nai\u{308}ve re\u{301}sume\u{301} A\u{30A}ngstro\u{308}m S\u{302}",
    ),
    sample(
        "latin-stacked-marks",
        "Latn",
        "a\u{301}\u{323} a\u{323}\u{301} q\u{307}\u{323} e\u{302}\u{301} o\u{31B}\u{303} \
         x\u{334}\u{335}\u{336} a\u{328}\u{301}\u{316}\u{300} u\u{308}\u{304}",
    ),
    // Marks HarfBuzz places by hinted contour points (DejaVu Sans' GPOS anchor
    // for U+0308) and, in a face without GPOS, by hinted extents.
    sample("latin-anchor-point", "Latn", "c\u{308} C\u{308}"),
    sample("latin-fallback-mark", "Latn", "x\u{301} o\u{301} X\u{301}"),
    sample(
        "latin-mark-order",
        "Latn",
        "a\u{316}\u{301} a\u{301}\u{316} a\u{345}\u{301} a\u{35C}b a\u{361}c i\u{307}\u{301} \
         e\u{327}\u{306} e\u{306}\u{327}",
    ),
    sample(
        "vietnamese",
        "Latn",
        "Tiếng Việt: Nguyễn, Đặng, phở, người, ước, Vie\u{302}\u{323}t, ngu\u{31B}\u{300}o\u{31B}i",
    ),
    sample(
        "latin-punctuation",
        "Latn",
        "\"Quotes\" «guillemets» — dashes – and… ‰ € £ © ® ™ ¶ §",
    ),
    sample(
        "latin-presentation",
        "Latn",
        "ǅungla ǈ ǋ Ŀ ŉ ſt ﬀ ﬁ ﬂ ﬃ ﬄ ﬅ ﬆ",
    ),
    sample("latin-ipa", "Latn", "ˈfəʊnɛtɪk tʃ dʒ ɬ ʔ ɡ ɦ ʰ ʲ ˤ"),
    localized("latin-turkish", "Latn", "tr", "İstanbul ıi İI fi ﬁ"),
    sample(
        "latin-ignorables",
        "Latn",
        "soft\u{AD}hyphen zw\u{200B}sp zw\u{200C}nj zw\u{200D}j word\u{2060}joiner bom\u{FEFF}x \
         lrm\u{200E}rlm\u{200F} mvs\u{180E} cgj\u{34F}x \u{E0001}\u{E0041}tag v\u{FE00}s",
    ),
    sample(
        "latin-controls-spaces",
        "Latn",
        "a\tb\nc\rd\u{1F}e\u{7F}f\u{85}g \u{A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\
         \u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}h",
    ),
    sample(
        "fractions",
        "Zyyy",
        "1\u{2044}2 3\u{2044}4 22\u{2044}7 123\u{2044}456 ½ ¾ ⅞",
    ),
    sample(
        "symbols",
        "Zyyy",
        "→ ← ↔ ∑ ∫ √ ∞ ≠ ≤ ≥ ★ ♠ ♣ ♥ ♦ ✓ ✗ ° ± × ÷",
    ),
    // Greek and Cyrillic.
    sample(
        "greek-monotonic",
        "Grek",
        "Καλημέρα κόσμε, ΑΒΓΔ άέήίόύώ ϊϋΐΰ ς",
    ),
    sample(
        "greek-polytonic",
        "Grek",
        "Ἐν ἀρχῇ ἦν ὁ λόγος, καὶ ὁ λόγος ἦν πρὸς τὸν θεόν",
    ),
    sample(
        "greek-decomposed",
        "Grek",
        "α\u{313}\u{301}\u{345} η\u{314}\u{342}\u{345} Ω\u{313}\u{300} ι\u{308}\u{301} υ\u{345}",
    ),
    sample(
        "cyrillic",
        "Cyrl",
        "Съешь же ещё этих мягких французских булок, да выпей чаю.",
    ),
    sample(
        "cyrillic-extended",
        "Cyrl",
        "Ѓ ѓ Ќ ќ Ў ў Ґ ґ Ӂ Ӝ й и\u{306} ё е\u{308} Ѷ ѷ",
    ),
    localized("cyrillic-serbian", "Cyrl", "sr", "бгдпт б г д п т ћ ђ"),
    // CJK.
    sample("han", "Hani", "永和九年歲在癸丑暮春之初會于會稽山陰之蘭亭"),
    sample(
        "han-punctuation",
        "Hani",
        "「漢字」（全角）、。！？：；〈〉《》【】〔〕…‥",
    ),
    sample(
        "han-variation-selectors",
        "Hani",
        "葛\u{E0100}葛\u{E0101}辻\u{E0100}辻\u{E0101}邊\u{E0102}禰\u{FE00}",
    ),
    sample(
        "hiragana",
        "Hira",
        "ひらがな、がぎぐげご。か\u{3099}は\u{309A}ゔゕゖ",
    ),
    sample("katakana", "Kana", "カタカナ ガ カ\u{3099} ヴ ㇰ ｶﾀｶﾅ ｶﾞ ー"),
    sample("hangul", "Hang", "한국어 텍스트, 안녕하세요."),
    sample(
        "hangul-jamo",
        "Hang",
        "\u{1100}\u{1161}\u{11A8} \u{1112}\u{1161}\u{11AB} \u{1100}\u{1161} \u{115F}\u{1160} \
         \u{1100}\u{1176}\u{11FA} \u{1100}\u{1161}\u{302E}",
    ),
    // Hebrew.
    sample("hebrew", "Hebr", "שלום עולם"),
    sample("hebrew-niqqud", "Hebr", "בְּרֵאשִׁית בָּרָא אֱלֹהִים אֵת הַשָּׁמַיִם"),
    sample(
        "hebrew-marks",
        "Hebr",
        "וַֽיְהִי הָֽאָדָם ש\u{5C1}\u{5BC} ש\u{5BC}\u{5C1} שׁ שׂ בּ וֹ כּ\u{5B8} ל\u{5B9}\u{5BC}",
    ),
    sample("hebrew-numbers", "Hebr", "מחיר 123.45 ש״ח (בערך) 10%"),
    // Arabic, Persian and Urdu.
    sample("arabic", "Arab", "السلام عليكم"),
    sample(
        "arabic-joining",
        "Arab",
        "ب بب ببب ك كك ه هه ي يي ع عع ئ ئئ",
    ),
    sample("arabic-lam-alef", "Arab", "لا لأ لإ لآ الله للا"),
    sample("arabic-tashkeel", "Arab", "بِسْمِ ٱللَّٰهِ ٱلرَّحْمَٰنِ ٱلرَّحِيمِ"),
    sample("arabic-digits", "Arab", "١٢٣ ٤٥٦ ۷۸۹ 123 ٫ ٬ ٪ ؟ ،"),
    sample("arabic-tatweel", "Arab", "كـــتـــاب ـبـ ـ"),
    sample(
        "arabic-joiners",
        "Arab",
        "ب\u{200D} \u{200D}ب ب\u{200C}ب ب\u{200D}\u{200C}ب لا ل\u{200C}ا ل\u{200D}ا",
    ),
    sample(
        "arabic-mark-order",
        "Arab",
        "ب\u{651}\u{64E} ب\u{64E}\u{651} ب\u{670}\u{651} ب\u{655}\u{650} ا\u{653} ا\u{654}",
    ),
    localized(
        "persian",
        "Arab",
        "fa",
        "فارسی: گچپژ ی ک ۱۲۳۴ می\u{200C}خواهم",
    ),
    localized(
        "urdu",
        "Arab",
        "ur",
        "اردو زبان: ہے ٹ ڈ ڑ ں ے ۓ ۴۵۶۷ نستعلیق",
    ),
    sample("arabic-mixed", "Arab", "العربية English 123 مرحبا"),
    // Indic.
    sample("devanagari", "Deva", "नमस्ते दुनिया"),
    sample("devanagari-conjuncts", "Deva", "क्ष त्र ज्ञ श्र द्ध द्य ह्म स्त्र"),
    sample("devanagari-reph", "Deva", "र्क र्त्य कर्म धर्म र्\u{200D}य"),
    sample("devanagari-matras", "Deva", "कि की कु कू कृ के कै को कौ कं कः कँ"),
    sample(
        "devanagari-nukta",
        "Deva",
        "क़ ख़ ग़ ज़ ड़ ढ़ फ़ य़ क\u{93C} ड\u{93C}\u{94D}",
    ),
    sample(
        "devanagari-joiners",
        "Deva",
        "क्\u{200D}ष क्\u{200C}ष \u{93F}क \u{94D} १२३",
    ),
    sample(
        "bengali",
        "Beng",
        "বাংলা ভাষা ক্ষ জ্ঞ র্ক কো কৌ ৎ য় ড় ঢ় ক\u{9C7}\u{9BE}",
    ),
    sample("tamil", "Taml", "தமிழ் மொழி கொ கோ கௌ க்ஷ ஸ்ரீ க\u{BC6}\u{BBE}"),
    sample("gujarati", "Gujr", "ગુજરાતી ભાષા ક્ષ જ્ઞ ર્ક કિ કી"),
    sample("gurmukhi", "Guru", "ਪੰਜਾਬੀ ਭਾਸ਼ਾ ਕਿ ਕੀ ਖ਼ ਗ਼ ਜ਼ ੜ ਫ਼ ਸ\u{A3C}"),
    sample("kannada", "Knda", "ಕನ್ನಡ ಭಾಷೆ ಕ್ಷ ರ್ಕ ಕೊ ಕೋ ಕೈ"),
    sample(
        "malayalam",
        "Mlym",
        "മലയാളം ഭാഷ ക്ഷ ന്റ ൺ ൻ കൊ കോ കൗ ന\u{D4D}\u{200D}",
    ),
    sample("telugu", "Telu", "తెలుగు భాష క్ష ర్క కొ కో కై"),
    sample("sinhala", "Sinh", "සිංහල භාෂාව ශ්\u{200D}රී ක්\u{200D}ෂ කො කෝ කෞ"),
    // Southeast Asian.
    sample("thai", "Thai", "ภาษาไทย สวัสดี กำ น้ำ ปั่น ป่ ฝื่ ญู ฐุ"),
    sample(
        "thai-marks",
        "Thai",
        "ก\u{E33} ก\u{E48}\u{E33} ก\u{E34}\u{E48} ก\u{E31}\u{E49} ป\u{E35}\u{E48} ฎ\u{E38} ญ\u{E39}",
    ),
    sample("khmer", "Khmr", "ភាសាខ្មែរ ស្រី ក្ស ក្រ ព្រះ កៀ កោ កៅ"),
    sample(
        "myanmar",
        "Mymr",
        "မြန်မာဘာသာ ကြ ကျ ကွ ကှ ကြွ ကျွ နိုင်ငံ င\u{103A}\u{1039}က",
    ),
    // Emoji.
    sample(
        "emoji",
        "Zyyy",
        "😀 👍 👍🏽 👩\u{200D}💻 👨\u{200D}👩\u{200D}👧\u{200D}👦 🏳\u{FE0F}\u{200D}🌈 ❤\u{FE0F} \
         ☺\u{FE0E} ☺\u{FE0F} 🇩🇪 🇯🇵 #\u{FE0F}\u{20E3} 1\u{FE0F}\u{20E3} 🧑🏿\u{200D}🦱",
    ),
    // Mixed directions.
    sample("mixed-hebrew-latin", "Hebr", "Hello שלום world"),
    sample("mixed-brackets", "Latn", "abc (אבג) def [עברית] {x}"),
    sample("mixed-arabic-latin", "Arab", "عربي English عربي 123 (نص)"),
    sample(
        "mixed-quote",
        "Latn",
        "The title is \"مفتاح معايير الويب!\" in Arabic.",
    ),
    sample(
        "mixed-explicit",
        "Latn",
        "a\u{202B}embedded RTL\u{202C} \u{2067}isolate\u{2069} \u{202E}override\u{202C} \
         \u{2068}שלום\u{2069}!",
    ),
    sample("mixed-numbers", "Hebr", "מספר 12-34 ו-56.78 +1 −2 1,000"),
    sample(
        "mixed-paragraphs",
        "Hebr",
        "a\u{2029}שלום\u{2029}b\nعربي\rc",
    ),
    sample(
        "mixed-angle-brackets",
        "Hebr",
        "א\u{2329}ב\u{232A} \u{3008}ג\u{3009} (a[b)c]",
    ),
];

/// The direction a script is shaped in.
pub fn direction(script: &str) -> &'static str {
    match script {
        "Arab" | "Hebr" | "Syrc" | "Thaa" | "Nkoo" | "Adlm" | "Mand" | "Samr" => "rtl",
        _ => "ltr",
    }
}

/// The opposite direction.
pub fn opposite(direction: &str) -> &'static str {
    if direction == "rtl" { "ltr" } else { "rtl" }
}

/// The language libass shapes a script with (`script_to_language` in
/// ass_shaper.c); `c`, HarfBuzz's default language in the C locale, where libass
/// has none.
pub fn language(script: &str) -> &'static str {
    match script {
        "Arab" => "ar",
        "Armn" => "hy",
        "Beng" => "bn",
        "Cans" => "iu",
        "Cher" => "chr",
        "Copt" => "cop",
        "Cyrl" => "ru",
        "Deva" => "hi",
        "Geor" => "ka",
        "Grek" => "el",
        "Gujr" => "gu",
        "Guru" => "pa",
        "Hang" => "ko",
        "Hebr" => "he",
        "Hira" | "Kana" => "ja",
        "Knda" => "kn",
        "Laoo" => "lo",
        "Latn" => "en",
        "Mlym" => "ml",
        "Mong" => "mn",
        "Orya" => "or",
        "Syrc" => "syr",
        "Taml" => "ta",
        "Telu" => "te",
        "Thai" => "th",
        "Tibt" => "bo",
        "Ethi" => "am",
        "Khmr" => "km",
        "Mymr" => "my",
        "Sinh" => "si",
        "Thaa" => "dv",
        "Buhd" => "bku",
        "Hano" => "hnn",
        "Tglg" => "tl",
        "Tagb" => "tbw",
        "Ugar" => "uga",
        "Bugi" => "bug",
        "Xpeo" => "peo",
        "Sylo" => "syl",
        "Nkoo" => "nko",
        _ => "c",
    }
}

/// The characters random strings of a script are drawn from: its blocks, and
/// joiners, spaces and marks shared with other scripts.
pub struct Alphabet {
    pub script: &'static str,
    pub ranges: &'static [(u32, u32)],
}

/// Characters mixed into every script's random strings.
const SHARED: &[u32] = &[
    0x20, 0x200C, 0x200D, 0x25CC, 0x34F, 0xAD, 0x2060, 0xFE00, 0x300, 0x301, 0x323, 0x30, 0x2D,
    0xA0,
];

pub const ALPHABETS: &[Alphabet] = &[
    Alphabet {
        script: "Latn",
        ranges: &[
            (0x41, 0x5A),
            (0x61, 0x7A),
            (0xC0, 0x24F),
            (0x300, 0x36F),
            (0x1E00, 0x1EFF),
        ],
    },
    Alphabet {
        script: "Grek",
        ranges: &[(0x370, 0x3FF), (0x1F00, 0x1FFE), (0x300, 0x345)],
    },
    Alphabet {
        script: "Cyrl",
        ranges: &[(0x400, 0x52F), (0x300, 0x308), (0x483, 0x489)],
    },
    Alphabet {
        script: "Hebr",
        ranges: &[(0x591, 0x5F4), (0x5D0, 0x5EA), (0xFB1D, 0xFB4F)],
    },
    Alphabet {
        script: "Arab",
        ranges: &[
            (0x600, 0x6FF),
            (0x621, 0x64A),
            (0x64B, 0x65F),
            (0x750, 0x77F),
            (0x8A0, 0x8FF),
            (0xFB50, 0xFDFF),
            (0xFE70, 0xFEFC),
        ],
    },
    Alphabet {
        script: "Deva",
        ranges: &[
            (0x900, 0x97F),
            (0x915, 0x939),
            (0x93E, 0x94D),
            (0xA8E0, 0xA8FF),
        ],
    },
    Alphabet {
        script: "Beng",
        ranges: &[(0x980, 0x9FF), (0x995, 0x9B9), (0x9BE, 0x9CD)],
    },
    Alphabet {
        script: "Taml",
        ranges: &[(0xB80, 0xBFF), (0xB95, 0xBB9), (0xBBE, 0xBCD)],
    },
    Alphabet {
        script: "Gujr",
        ranges: &[(0xA80, 0xAFF), (0xA95, 0xAB9), (0xABE, 0xACD)],
    },
    Alphabet {
        script: "Guru",
        ranges: &[(0xA00, 0xA7F), (0xA15, 0xA39), (0xA3E, 0xA4D)],
    },
    Alphabet {
        script: "Knda",
        ranges: &[(0xC80, 0xCFF), (0xC95, 0xCB9), (0xCBE, 0xCCD)],
    },
    Alphabet {
        script: "Mlym",
        ranges: &[(0xD00, 0xD7F), (0xD15, 0xD3A), (0xD3E, 0xD4D)],
    },
    Alphabet {
        script: "Telu",
        ranges: &[(0xC00, 0xC7F), (0xC15, 0xC39), (0xC3E, 0xC4D)],
    },
    Alphabet {
        script: "Sinh",
        ranges: &[(0xD80, 0xDFF), (0xD9A, 0xDC6), (0xDCF, 0xDDF)],
    },
    Alphabet {
        script: "Thai",
        ranges: &[(0xE00, 0xE7F), (0xE01, 0xE2E), (0xE30, 0xE4E)],
    },
    Alphabet {
        script: "Khmr",
        ranges: &[
            (0x1780, 0x17FF),
            (0x1780, 0x17A2),
            (0x17B6, 0x17D3),
            (0x19E0, 0x19FF),
        ],
    },
    Alphabet {
        script: "Mymr",
        ranges: &[
            (0x1000, 0x109F),
            (0x1000, 0x1021),
            (0x102B, 0x103E),
            (0xAA60, 0xAA7F),
        ],
    },
    Alphabet {
        script: "Hani",
        ranges: &[
            (0x4E00, 0x9FFF),
            (0x3400, 0x4DBF),
            (0xE0100, 0xE01EF),
            (0x3000, 0x303F),
        ],
    },
    Alphabet {
        script: "Hira",
        ranges: &[(0x3041, 0x309F), (0x3099, 0x309C), (0x3000, 0x303F)],
    },
    Alphabet {
        script: "Kana",
        ranges: &[(0x30A0, 0x30FF), (0xFF65, 0xFF9F), (0x31F0, 0x31FF)],
    },
    Alphabet {
        script: "Hang",
        ranges: &[
            (0xAC00, 0xD7A3),
            (0x1100, 0x11FF),
            (0x3130, 0x318F),
            (0xA960, 0xA97F),
        ],
    },
    Alphabet {
        script: "Zyyy",
        ranges: &[
            (0x1F300, 0x1F6FF),
            (0x1F900, 0x1F9FF),
            (0x1F3FB, 0x1F3FF),
            (0x2600, 0x27BF),
            (0x1F1E6, 0x1F1FF),
            (0xFE0E, 0xFE0F),
            (0x2000, 0x206F),
            (0x20D0, 0x20F0),
        ],
    },
];

/// A deterministic generator (SplitMix64), so the corpus is the same on every run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`; `n` is not zero.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// A random string of `alphabet`: mostly its own characters, a quarter shared ones.
pub fn random_text(alphabet: &Alphabet, length: usize, rng: &mut Rng) -> Vec<char> {
    (0..length)
        .map(|_| {
            let code = if rng.below(4) == 0 {
                rng.pick(SHARED)
            } else {
                let (first, last) = rng.pick(alphabet.ranges);
                first + rng.below((last - first + 1) as usize) as u32
            };
            char::from_u32(code).unwrap_or('\u{FFFD}')
        })
        .collect()
}

/// Characters of every bidi class, grouped so each class is drawn as often.
const BIDI_CLASSES: &[&[u32]] = &[
    // L, R, AL
    &[0x61, 0x62, 0x7A, 0xE9, 0x3B1, 0x5D1, 0x30A2],
    &[0x5D0, 0x5D1, 0x5EA, 0x7C0, 0xFB1D],
    &[0x627, 0x628, 0x644, 0x6CC, 0x710, 0x780],
    // EN, AN, ES, ET, CS
    &[0x30, 0x31, 0x39, 0x6F0, 0x6F9, 0xB2, 0xFF11],
    &[0x660, 0x669, 0x66B, 0x66C, 0x600, 0x6DD],
    &[0x2B, 0x2D, 0x207A, 0xFE62],
    &[0x23, 0x24, 0x25, 0xA2, 0xB0, 0x20AC, 0x2030],
    &[0x2C, 0x2E, 0x3A, 0x2F, 0xA0, 0x60C, 0x202F],
    // NSM, BN
    &[0x300, 0x301, 0x64B, 0x5B4, 0x93C, 0x20D0],
    &[0xAD, 0x200B, 0x200C, 0x200D, 0x2060, 0xFEFF, 0x1],
    // B, S, WS
    &[0xA, 0xD, 0x1C, 0x1D, 0x1E, 0x85, 0x2029],
    &[0x9, 0xB, 0x1F],
    &[0x20, 0xC, 0x2000, 0x2028, 0x3000],
    // ON and brackets
    &[
        0x21, 0x22, 0x26, 0x27, 0x2A, 0x3B, 0x3C, 0x3D, 0x3E, 0x40, 0x5C, 0x7E,
    ],
    &[
        0x28, 0x29, 0x5B, 0x5D, 0x7B, 0x7D, 0x2329, 0x232A, 0x3008, 0x3009, 0xFF08, 0xFF09,
    ],
    &[0x28, 0x29, 0x2985, 0x2986, 0xF3A, 0xF3B, 0x2045, 0x2046],
    // Explicit embeddings, overrides and isolates; LRM, RLM, ALM
    &[0x202A, 0x202B, 0x202C, 0x202D, 0x202E],
    &[0x2066, 0x2067, 0x2068, 0x2069],
    &[0x200E, 0x200F, 0x61C],
];

/// A random string over every bidi class.
pub fn random_bidi_text(length: usize, rng: &mut Rng) -> Vec<char> {
    (0..length)
        .map(|_| {
            let class = rng.pick(BIDI_CLASSES);
            char::from_u32(rng.pick(class)).unwrap_or('\u{FFFD}')
        })
        .collect()
}

/// The characters random Arabic strings for FriBidi's shaping are drawn from.
const SIMPLE_ARABIC: &[(u32, u32)] = &[
    (0x621, 0x64A),
    (0x621, 0x64A),
    (0x64B, 0x655),
    (0x670, 0x6D3),
    (0x640, 0x640),
    (0x200C, 0x200D),
    (0x20, 0x20),
    (0x28, 0x29),
    (0x61, 0x63),
    (0x660, 0x669),
    (0xFE70, 0xFEFC),
];

pub fn random_arabic_text(length: usize, rng: &mut Rng) -> Vec<char> {
    (0..length)
        .map(|_| {
            let (first, last) = rng.pick(SIMPLE_ARABIC);
            let code = first + rng.below((last - first + 1) as usize) as u32;
            char::from_u32(code).unwrap_or('\u{FFFD}')
        })
        .collect()
}

/// Deeply nested embeddings and isolates, past FriBidi's maximum depth of 125.
pub fn nesting_texts() -> Vec<(String, Vec<char>)> {
    let nest = |open: char, depth: usize, middle: &str, close: char| -> Vec<char> {
        let mut text = vec![open; depth];
        text.extend(middle.chars());
        text.extend(std::iter::repeat_n(close, depth));
        text
    };
    let alternating: Vec<char> = (0..140)
        .map(|i| if i % 2 == 0 { '\u{202B}' } else { '\u{202A}' })
        .chain("aא1".chars())
        .chain(std::iter::repeat_n('\u{202C}', 140))
        .collect();
    vec![
        (
            "nesting-rle-130".into(),
            nest('\u{202B}', 130, "aב2", '\u{202C}'),
        ),
        (
            "nesting-lri-130".into(),
            nest('\u{2066}', 130, "בa2", '\u{2069}'),
        ),
        (
            "nesting-rlo-64".into(),
            nest('\u{202E}', 64, "abc", '\u{202C}'),
        ),
        ("nesting-alternating".into(), alternating),
        (
            "nesting-unmatched".into(),
            "\u{2069}\u{202C}a\u{2067}b\u{202B}c\u{2066}א"
                .chars()
                .collect(),
        ),
    ]
}

/// A label that names two samples, if any: labels name what each text exercises
/// and must be unique.
pub fn duplicate_label() -> Option<&'static str> {
    let mut labels: Vec<&'static str> = SAMPLES.iter().map(|s| s.label).collect();
    labels.sort_unstable();
    labels.windows(2).find(|w| w[0] == w[1]).map(|w| w[0])
}
