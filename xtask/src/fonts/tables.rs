//! `cargo xtask fonts tables`: the tables triergon-font takes from FreeType,
//! read from the FreeType 2.15.0 release the font reference is built from.
//!
//! FreeType builds the Unicode charmap of a CFF face without one from its glyph
//! names, through the Adobe Glyph List it compiles into `psnames/pstables.h`
//! (a compressed trie), and lists a CFF face's predefined encodings and charsets
//! from the tables of `cff/cffload.c`. Its auto-hinter assigns glyphs to styles
//! by the script ranges of `autofit/afranges.c`, in the script and style order
//! of `afscript.h` and `afstyles.h`, finds blue zones from the strings of
//! `afblue.c` and adjusts accents by the database of `afadjust.c`. The
//! generated files hold exactly what those arrays hold, for FreeType's default
//! configuration (`AF_CONFIG_OPTION_CJK` and `AF_CONFIG_OPTION_INDIC` on, no
//! HarfBuzz); every array is found by its declaration and its length checked,
//! so a different FreeType layout cannot be misread.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::{fetch, sha256};

const USAGE: &str = "usage: cargo xtask fonts tables [--check]

  Regenerate crates/font/src/postscript/tables.rs and
  crates/font/src/autofit/tables.rs from the FreeType release `cargo xtask
  fonts prepare` unpacks; --check fails if a committed file differs.";

const POSTSCRIPT_OUTPUT: &str = "crates/font/src/postscript/tables.rs";
const AUTOFIT_OUTPUT: &str = "crates/font/src/autofit/tables.rs";

/// The files of the FreeType tree read, with their hashes.
const POSTSCRIPT_SOURCES: [(&str, &str); 2] = [
    (
        "src/psnames/pstables.h",
        "1095bb32a4346a0961baa081a9d275c48feb8a8b9c94ac534d2047d504aa9abf",
    ),
    (
        "src/cff/cffload.c",
        "40a2a68659e28a552cb5646874916633d73114940b8aefcd0cbe34af8e685460",
    ),
];

const AUTOFIT_SOURCES: [(&str, &str); 7] = [
    (
        "src/autofit/afscript.h",
        "c623bcd6176627184808fe171e7d3d515a0e3335be594823c6720fd0714be5b9",
    ),
    (
        "src/autofit/afstyles.h",
        "d409ff2483e005f8d8d42e1bc5713711b26bf598c3e5d65de75912649c1fdc22",
    ),
    (
        "src/autofit/afranges.c",
        "ae5edcb10d55499a8e56b92bd724fc82bf265de8ea69e0ee5bdc9a33e7f8ccb1",
    ),
    (
        "src/autofit/afblue.h",
        "fb5cd1dda3def04abc79671b6425939f0b4297e63aaf521bc842d61eecbe70c7",
    ),
    (
        "src/autofit/afblue.c",
        "16ed687694c4c31a29a3fe8e2732e210774b200cc633be8cf9cb70b50c109f80",
    ),
    (
        "src/autofit/afadjust.h",
        "1bb4ecb0bf2a21c3b043902f95f1532804cad0ec3cad12dbed1dc5dfc27823a0",
    ),
    (
        "src/autofit/afadjust.c",
        "44e9fbb2d6dd2f05bb7742dfcdd236d0a5617ed98ef362ebf2cb12ca6d1b7028",
    ),
];

/// The largest source, `pstables.h`, is under 300 KiB.
const SOURCE_LIMIT: usize = 1 << 20;

const WIDTH: usize = 100;

/// The configuration macros FreeType's default `ftoption.h` defines that the
/// auto-hinter sources test.
const DEFINED: [&str; 2] = ["AF_CONFIG_OPTION_CJK", "AF_CONFIG_OPTION_INDIC"];

/// What the 2.15.0 sources hold, so that a misread cannot pass unnoticed:
/// scripts, styles, script ranges (both kinds, terminators excluded), the
/// bytes of `af_blue_strings`, the entries of `af_blue_stringsets` and those
/// of the adjustment database.
const SCRIPTS: usize = 60;
const STYLES: usize = 87;
const RANGES: usize = 363;
const BLUE_STRING_BYTES: usize = 5531;
const BLUE_STRINGSETS: usize = 265;
const ADJUSTMENTS: usize = 935;

/// `AF_BLUE_STRING_MAX_LEN`: the clusters of one blue string FreeType keeps
/// room for.
const BLUE_STRING_MAX_LEN: usize = 51;
/// `AF_BLUE_STRINGSET_MAX_LEN`: the blue zones of one style.
const BLUE_STRINGSET_MAX_LEN: usize = 8;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err(USAGE.into()),
    };
    let tree = root
        .join(".research/freetype")
        .join(format!("freetype-{}", super::VERSION));
    let read = |sources: &[(&str, &str)]| -> Result<Vec<String>, String> {
        sources
            .iter()
            .map(|&(path, hash)| {
                let path = tree.join(path);
                if !path.exists() {
                    return Err(format!(
                        "{}: missing; `cargo xtask fonts prepare` unpacks the FreeType release",
                        path.display()
                    ));
                }
                let data = fetch::read_bounded(&path, SOURCE_LIMIT)?;
                if sha256::hex(&data) != hash {
                    return Err(format!(
                        "{}: differs from FreeType {}",
                        path.display(),
                        super::VERSION
                    ));
                }
                String::from_utf8(data).map_err(|_| format!("{}: not UTF-8", path.display()))
            })
            .collect()
    };
    let postscript = read(&POSTSCRIPT_SOURCES)?;
    let autofit = read(&AUTOFIT_SOURCES)?;
    let outputs = [
        (POSTSCRIPT_OUTPUT, generate(&postscript[0], &postscript[1])?),
        (AUTOFIT_OUTPUT, generate_autofit(&autofit)?),
    ];
    let mut stale = Vec::new();
    for (output, text) in &outputs {
        let path = root.join(output);
        if fs::read(&path).ok().as_deref() == Some(text.as_bytes()) {
            println!("{output}: up to date");
        } else if check {
            stale.push(*output);
        } else {
            fetch::publish(&path, text.as_bytes())?;
            println!("{output}: {} bytes", text.len());
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} differ from what `cargo xtask fonts tables` generates",
            stale.join(" and ")
        ))
    }
}

/// The header of a generated file: what it holds and the sources it is read
/// from, with their hashes.
fn header(what: &str, sources: &[(&str, &str)]) -> String {
    let mut out = format!(
        "// @generated by `cargo xtask fonts tables`; do not edit.\n\
         // The {what} of FreeType {0}, from freetype-{0}.tar.xz\n",
        super::VERSION,
    );
    out.push_str(&format!("//     sha256 {}:\n", super::ARCHIVE_SHA256));
    for (path, hash) in sources {
        out.push_str(&format!("//   {path}\n//     sha256 {hash}\n"));
    }
    out
}

fn generate(pstables: &str, cffload: &str) -> Result<String, String> {
    let mut out = header("PostScript glyph-name tables", &POSTSCRIPT_SOURCES);

    let glyph_list = numbers(
        pstables,
        "const unsigned char  ft_adobe_glyph_list[55997L]",
        55_997,
    )?;
    let bytes = glyph_list
        .iter()
        .map(|&n| u8::try_from(n).map_err(|_| "ft_adobe_glyph_list: not a byte".to_owned()))
        .collect::<Result<Vec<u8>, String>>()?;
    array(
        &mut out,
        "`ft_adobe_glyph_list`: the Adobe Glyph List as FreeType compresses it, a\n\
         trie of glyph-name letters ending in 16-bit Unicode values.",
        "ADOBE_GLYPH_LIST",
        "u8",
        bytes.iter().map(u8::to_string),
    );

    let pool = numbers(pstables, "const char  ft_standard_glyph_names[3696]", 3_696)?;
    let offsets = numbers(pstables, "const short  ft_sid_names[FT_NUM_SID_NAMES]", 391)?;
    let mut names = Vec::new();
    for offset in offsets {
        let start = usize::try_from(offset).map_err(|_| "ft_sid_names: negative offset")?;
        let rest = pool
            .get(start..)
            .ok_or("ft_sid_names: offset outside the names")?;
        let end = rest
            .iter()
            .position(|&c| c == 0)
            .ok_or("ft_standard_glyph_names: unterminated name")?;
        let name: String = rest[..end]
            .iter()
            .map(|&c| {
                u8::try_from(c)
                    .ok()
                    .filter(u8::is_ascii_graphic)
                    .map(char::from)
            })
            .collect::<Option<String>>()
            .ok_or("ft_standard_glyph_names: not a glyph name")?;
        names.push(format!("\"{name}\""));
    }
    array(
        &mut out,
        "The CFF standard strings (`ft_sid_names` into `ft_standard_glyph_names`):\n\
         the glyph names of SIDs 0 to 390.",
        "STANDARD_STRINGS",
        "&str",
        names.into_iter(),
    );

    for (name, length, doc) in [
        (
            "cff_standard_encoding",
            256,
            "The Standard encoding: the SID of each code.",
        ),
        (
            "cff_expert_encoding",
            256,
            "The Expert encoding: the SID of each code.",
        ),
        (
            "cff_isoadobe_charset",
            229,
            "The ISOAdobe charset: the SID of each glyph.",
        ),
        (
            "cff_expert_charset",
            166,
            "The Expert charset: the SID of each glyph.",
        ),
        (
            "cff_expertsubset_charset",
            87,
            "The ExpertSubset charset: the SID of each glyph.",
        ),
    ] {
        let values = numbers(
            cffload,
            &format!("static const FT_UShort  {name}[{length}]"),
            length,
        )?;
        let constant = name.trim_start_matches("cff_").to_uppercase();
        array(
            &mut out,
            &format!("`{name}`. {doc}"),
            &constant,
            "u16",
            values.iter().map(i64::to_string),
        );
    }
    Ok(out)
}

/// The values of the array declared by `declaration`: numbers and character
/// literals between the braces after it, which must be `length` many.
fn numbers(text: &str, declaration: &str, length: usize) -> Result<Vec<i64>, String> {
    let start = text
        .find(declaration)
        .ok_or_else(|| format!("no `{declaration}`"))?;
    let rest = &text[start + declaration.len()..];
    let open = rest
        .find('{')
        .ok_or_else(|| format!("{declaration}: no values"))?;
    // Nothing but the initialiser's `=` and preprocessor lines may come first.
    let before: String = rest[..open]
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    if before.trim() != "=" {
        return Err(format!("{declaration}: unexpected `{}`", before.trim()));
    }
    let body = &rest[open + 1..];
    let close = body
        .find('}')
        .ok_or_else(|| format!("{declaration}: no end"))?;
    let mut values = Vec::new();
    for token in body[..close].split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let value = match token.as_bytes() {
            [b'\'', c, b'\''] => i64::from(*c),
            _ => token
                .parse()
                .map_err(|_| format!("{declaration}: unexpected `{token}`"))?,
        };
        values.push(value);
    }
    if values.len() != length {
        return Err(format!(
            "{declaration}: {} values, not {length}",
            values.len()
        ));
    }
    Ok(values)
}

/// A static array of `items`, laid out in rows of 100 columns.
fn array(out: &mut String, doc: &str, name: &str, ty: &str, items: impl Iterator<Item = String>) {
    let items: Vec<String> = items.collect();
    out.push('\n');
    doc_comment(out, doc, "");
    out.push_str(&format!(
        "pub(crate) static {name}: [{ty}; {}] = [\n",
        items.len()
    ));
    rows(out, &items, "    ");
    out.push_str("];\n");
}

/// `doc` as `///` lines indented by `indent`.
fn doc_comment(out: &mut String, doc: &str, indent: &str) {
    for line in doc.lines() {
        out.push_str(indent);
        out.push_str("///");
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
}

/// `items`, each followed by a comma, in rows that fit 100 columns.
fn rows(out: &mut String, items: &[String], indent: &str) {
    let mut row = String::new();
    for item in items {
        if !row.is_empty() && indent.len() + row.len() + item.len() + 2 > WIDTH {
            out.push_str(&format!("{indent}{row}\n"));
            row.clear();
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(item);
        row.push(',');
    }
    if !row.is_empty() {
        out.push_str(&format!("{indent}{row}\n"));
    }
}

/// A C source with its comments removed, its conditional blocks resolved for
/// FreeType's default configuration, and its macro definitions set aside.
struct Source {
    /// The active lines that are not preprocessor directives.
    text: String,
    /// Each `#define` in an active block: the name and the rest of the
    /// definition, continuation lines joined.
    defines: Vec<(String, String)>,
}

impl Source {
    fn new(file: &str, text: &str) -> Result<Self, String> {
        let text = strip_comments(text).map_err(|e| format!("{file}: {e}"))?;
        let text = text.replace("\\\n", " ");
        let mut defined: Vec<String> = DEFINED.iter().map(|&s| s.to_owned()).collect();
        // Whether each enclosing block is active, and whether an earlier
        // branch of it was.
        let mut blocks: Vec<(bool, bool)> = Vec::new();
        let mut out = String::new();
        let mut defines = Vec::new();
        for line in text.lines() {
            let active = blocks.iter().all(|&(active, _)| active);
            let Some(directive) = line.trim_start().strip_prefix('#') else {
                if active {
                    out.push_str(line);
                    out.push('\n');
                }
                continue;
            };
            let mut words = directive.split_whitespace();
            let keyword = words.next().unwrap_or_default();
            let argument = words.next().unwrap_or_default();
            match keyword {
                // Blocks inside an inactive one stay inactive throughout.
                "if" | "ifdef" | "ifndef" if !active => blocks.push((false, true)),
                "ifdef" | "ifndef" => {
                    let is = defined.iter().any(|d| d == argument);
                    let taken = is == (keyword == "ifdef");
                    blocks.push((taken, taken));
                }
                "if" => {
                    let taken = match argument {
                        "0" => false,
                        "1" => true,
                        _ => return Err(format!("{file}: unexpected `#{}`", directive.trim())),
                    };
                    blocks.push((taken, taken));
                }
                "else" => {
                    let block = blocks
                        .last_mut()
                        .ok_or_else(|| format!("{file}: `#else` outside a block"))?;
                    *block = (!block.1, true);
                }
                "endif" => {
                    blocks
                        .pop()
                        .ok_or_else(|| format!("{file}: `#endif` outside a block"))?;
                }
                "define" if active => {
                    let rest = directive.trim_start().trim_start_matches("define").trim();
                    let end = rest
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(rest.len());
                    defined.push(rest[..end].to_owned());
                    defines.push((rest[..end].to_owned(), rest[end..].trim().to_owned()));
                }
                "define" | "undef" | "include" => {}
                _ if !active => {}
                _ => return Err(format!("{file}: unexpected `#{}`", directive.trim())),
            }
        }
        if !blocks.is_empty() {
            return Err(format!("{file}: unterminated `#if`"));
        }
        Ok(Self { text: out, defines })
    }

    /// The definition of macro `name`.
    fn define(&self, name: &str) -> Result<&str, String> {
        self.defines
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, body)| body.as_str())
            .ok_or_else(|| format!("no `#define {name}`"))
    }

    /// The initialiser of the array declared by `declaration`: the text
    /// between the braces after its `=`.
    fn array(&self, declaration: &str) -> Result<&str, String> {
        let start = self
            .text
            .find(declaration)
            .ok_or_else(|| format!("no `{declaration}`"))?;
        let rest = self.text[start + declaration.len()..].trim_start();
        let rest = rest
            .strip_prefix('=')
            .ok_or_else(|| format!("{declaration}: no initialiser"))?
            .trim_start();
        if !rest.starts_with('{') {
            return Err(format!("{declaration}: no values"));
        }
        let end = closing(rest, '{', '}').ok_or_else(|| format!("{declaration}: no end"))?;
        Ok(&rest[1..end])
    }
}

/// `text` without its C comments; string and character literals are kept.
fn strip_comments(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                loop {
                    let c = chars.next().ok_or("unterminated comment")?;
                    // Comments spanning lines keep their line breaks.
                    if c == '\n' {
                        out.push('\n');
                    }
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'/') => {
                while chars.peek().is_some_and(|&c| c != '\n') {
                    chars.next();
                }
            }
            '"' | '\'' => {
                out.push(c);
                loop {
                    let d = chars.next().ok_or("unterminated literal")?;
                    out.push(d);
                    if d == '\\' {
                        out.push(chars.next().ok_or("unterminated literal")?);
                    } else if d == c {
                        break;
                    } else if d == '\n' {
                        return Err("unterminated literal".into());
                    }
                }
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// The index of the bracket closing the one `text` starts with, skipping
/// string and character literals.
fn closing(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0_usize;
    let mut literal: Option<char> = None;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if let Some(quote) = literal {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote {
                literal = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => literal = Some(c),
            c if c == open => depth += 1,
            c if c == close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The arguments of every call of macro `name` in `text`, in order, split at
/// the commas outside brackets and literals and trimmed.
fn calls<'a>(text: &'a str, name: &str) -> Result<Vec<Vec<&'a str>>, String> {
    Ok(placed_calls(text, name)?
        .into_iter()
        .map(|(_, args)| args)
        .collect())
}

/// [`calls`], each with the offset of the call in `text`.
fn placed_calls<'a>(text: &'a str, name: &str) -> Result<Vec<(usize, Vec<&'a str>)>, String> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(name) {
        let start = from + at;
        from = start + name.len();
        let before = text[..start].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let rest = text[from..].trim_start();
        if !rest.starts_with('(') {
            continue;
        }
        let end = closing(rest, '(', ')').ok_or_else(|| format!("{name}: unterminated call"))?;
        found.push((start, arguments(&rest[1..end])));
        from = text.len() - rest.len() + end;
    }
    Ok(found)
}

/// `text` split at the commas outside brackets and literals, each part
/// trimmed.
fn arguments(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut literal: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if let Some(quote) = literal {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote {
                literal = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => literal = Some(c),
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts
}

/// The bytes of one C character or string literal body after a backslash:
/// `\xHH...` (all hex digits that follow), an octal escape or a simple one.
fn escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<u8, String> {
    let c = chars.next().ok_or("unterminated escape")?;
    let value = match c {
        'x' => {
            let mut value = 0_u32;
            let mut digits = 0;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(16)) {
                chars.next();
                value = value * 16 + d;
                digits += 1;
                if value > 0xff {
                    return Err("hex escape beyond a byte".into());
                }
            }
            if digits == 0 {
                return Err("empty hex escape".into());
            }
            value
        }
        '0'..='7' => {
            let mut value = c.to_digit(8).unwrap_or_default();
            for _ in 0..2 {
                match chars.peek().and_then(|c| c.to_digit(8)) {
                    Some(d) => {
                        chars.next();
                        value = value * 8 + d;
                    }
                    None => break,
                }
            }
            value
        }
        'n' => 10,
        't' => 9,
        '\\' | '\'' | '"' => u32::from(c),
        _ => return Err(format!("unexpected escape `\\{c}`")),
    };
    u8::try_from(value).map_err(|_| "escape beyond a byte".to_owned())
}

/// The bytes of adjacent C string literals, as the compiler joins them.
fn c_string(text: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut chars = text.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '"' => loop {
                match chars.next().ok_or("unterminated string")? {
                    '"' => break,
                    '\\' => bytes.push(escape(&mut chars)?),
                    c if c.is_ascii() => bytes.push(c as u8),
                    c => return Err(format!("non-ASCII `{c}` in a string")),
                }
            },
            _ => return Err(format!("`{text}`: not a string literal")),
        }
    }
    Ok(bytes)
}

/// The byte of a C character literal.
fn c_char(text: &str) -> Result<u8, String> {
    let body = text
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .ok_or_else(|| format!("`{text}`: not a character literal"))?;
    let mut chars = body.chars().peekable();
    let byte = match chars.next() {
        Some('\\') => escape(&mut chars)?,
        Some(c) if c.is_ascii() => c as u8,
        _ => return Err(format!("`{text}`: not a byte")),
    };
    if chars.next().is_some() {
        return Err(format!("`{text}`: more than one character"));
    }
    Ok(byte)
}

/// A C integer constant: decimal or `0x` hexadecimal, with an optional `U`
/// or `L` suffix.
fn c_number(text: &str) -> Result<u64, String> {
    let digits = text.trim().trim_end_matches(['U', 'u', 'L', 'l']);
    let value = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => digits.parse(),
    };
    value.map_err(|_| format!("`{text}`: not a number"))
}

/// The values of the members of the enumeration `name`, in order: each
/// given as a number, as another member plus a number, or as the previous
/// value plus one.
fn enumeration(source: &Source, name: &str) -> Result<Vec<(String, u64)>, String> {
    let declaration = format!("typedef enum  {name}_");
    // The declaration of `name`, not of a longer name it begins.
    let rest = source
        .text
        .match_indices(&declaration)
        .map(|(at, _)| source.text[at + declaration.len()..].trim_start())
        .find(|rest| rest.starts_with('{'))
        .ok_or_else(|| format!("no `{declaration}`"))?;
    let end = closing(rest, '{', '}').ok_or_else(|| format!("{name}: no end"))?;
    let mut members: Vec<(String, u64)> = Vec::new();
    for member in arguments(&rest[1..end]) {
        if member.is_empty() {
            continue;
        }
        let (ident, value) = match member.split_once('=') {
            None => {
                let next = members.last().map_or(0, |&(_, v)| v + 1);
                (member, next)
            }
            Some((ident, expression)) => {
                let mut value = 0;
                for term in expression.split('+') {
                    let term = term.trim();
                    value += match members.iter().find(|(n, _)| n == term) {
                        Some(&(_, v)) => v,
                        None => c_number(term)?,
                    };
                }
                (ident.trim(), value)
            }
        };
        members.push((ident.to_owned(), value));
    }
    Ok(members)
}

/// One script of `afscript.h`.
struct Script {
    lower: String,
    upper: String,
    top_to_bottom: bool,
    standard: String,
}

/// One style of `afstyles.h`.
struct Style {
    name: String,
    writing_system: &'static str,
    script: usize,
    stringset: Option<String>,
    default_coverage: bool,
}

fn generate_autofit(texts: &[String]) -> Result<String, String> {
    let source = |i: usize| Source::new(AUTOFIT_SOURCES[i].0, &texts[i]);
    let (afscript, afstyles, afranges) = (source(0)?, source(1)?, source(2)?);
    let (afblue_h, afblue_c, afadjust_h, afadjust_c) =
        (source(3)?, source(4)?, source(5)?, source(6)?);

    let scripts = scripts(&afscript)?;
    let styles = styles(&afstyles, &scripts)?;

    let mut out = header(
        "auto-hinter script, style and adjustment tables",
        &AUTOFIT_SOURCES,
    );
    out.push_str(
        "\nuse super::style::{BlueString, Script, Style, WritingSystem};\n\
         use WritingSystem::{Cjk, Dummy, Indic, Latin};\n",
    );

    // The scripts and their ranges.
    let mut ranges_total = 0;
    let mut entries = Vec::new();
    for script in &scripts {
        let mut fields = Vec::new();
        for kind in ["", "nonbase_"] {
            let declaration = format!(
                "const AF_Script_UniRangeRec  af_{}_{kind}uniranges[]",
                script.lower
            );
            let ranges = ranges(&afranges, &declaration)?;
            ranges_total += ranges.len();
            let items: Vec<String> = ranges
                .iter()
                .map(|&(a, b)| format!("(0x{a:04X}, 0x{b:04X})"))
                .collect();
            fields.push(items);
        }
        entries.push((script, fields));
    }
    if scripts.len() != SCRIPTS || ranges_total != RANGES {
        return Err(format!(
            "afscript.h and afranges.c: {} scripts with {ranges_total} ranges, not {SCRIPTS} with {RANGES}",
            scripts.len()
        ));
    }
    out.push_str(&format!(
        "\n/// `af_script_classes`: the scripts of `afscript.h` in order, with the\n\
         /// character ranges of `afranges.c` (`af_<script>_uniranges` and\n\
         /// `af_<script>_nonbase_uniranges`, without their terminators), the\n\
         /// direction their horizontal edges are hinted in and the characters\n\
         /// their standard stem widths are measured on.\n\
         pub(crate) static SCRIPTS: [Script; {}] = [\n",
        scripts.len()
    ));
    for (script, fields) in entries {
        out.push_str(&format!("    // {}\n    Script {{\n", script.lower));
        for (name, items) in ["ranges", "nonbase"].iter().zip(fields) {
            if items.is_empty() {
                out.push_str(&format!("        {name}: &[],\n"));
            } else {
                out.push_str(&format!("        {name}: &[\n"));
                rows(&mut out, &items, "            ");
                out.push_str("        ],\n");
            }
        }
        out.push_str(&format!(
            "        top_to_bottom: {},\n        standard: {},\n    }},\n",
            script.top_to_bottom,
            rust_string(&script.standard)
        ));
    }
    out.push_str("];\n");

    // The blue strings of the styles of the latin writing system.
    let strings = enumeration(&afblue_h, "AF_Blue_String")?;
    let stringsets = enumeration(&afblue_h, "AF_Blue_Stringset")?;
    let pool: Vec<u8> = arguments(afblue_c.array("af_blue_strings[]")?)
        .into_iter()
        .filter(|t| !t.is_empty())
        .map(c_char)
        .collect::<Result<_, _>>()?;
    if pool.len() != BLUE_STRING_BYTES {
        return Err(format!(
            "af_blue_strings: {} bytes, not {BLUE_STRING_BYTES}",
            pool.len()
        ));
    }
    let sets: Vec<Vec<&str>> = arguments(afblue_c.array("af_blue_stringsets[]")?)
        .into_iter()
        .filter(|t| !t.is_empty())
        .map(|entry| {
            entry
                .strip_prefix('{')
                .and_then(|e| e.strip_suffix('}'))
                .map(arguments)
                .ok_or_else(|| format!("af_blue_stringsets: unexpected `{entry}`"))
        })
        .collect::<Result<_, _>>()?;
    if sets.len() != BLUE_STRINGSETS || sets.iter().any(|s| s.len() != 2) {
        return Err(format!(
            "af_blue_stringsets: {} entries, not {BLUE_STRINGSETS} pairs",
            sets.len()
        ));
    }
    let properties: Vec<(&str, u64)> = [
        "AF_BLUE_PROPERTY_LATIN_TOP",
        "AF_BLUE_PROPERTY_LATIN_SUB_TOP",
        "AF_BLUE_PROPERTY_LATIN_NEUTRAL",
        "AF_BLUE_PROPERTY_LATIN_X_HEIGHT",
        "AF_BLUE_PROPERTY_LATIN_LONG",
        "AF_BLUE_PROPERTY_LATIN_CAPITAL_BOTTOM",
        "AF_BLUE_PROPERTY_LATIN_SMALL_BOTTOM",
    ]
    .into_iter()
    .map(|name| Ok((name, shift_value(afblue_h.define(name)?)?)))
    .collect::<Result<_, String>>()?;
    let value_of = |name: &str| -> Result<u64, String> {
        strings
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, v)| v)
            .ok_or_else(|| format!("no blue string `{name}`"))
    };
    let max_string = value_of("AF_BLUE_STRING_MAX")?;
    let mut emitted: HashMap<String, String> = HashMap::new();
    let mut blue_sets = String::new();
    for style in styles.iter().filter(|s| s.writing_system == "Latin") {
        let set = style
            .stringset
            .as_ref()
            .ok_or_else(|| format!("{}: no blue stringset", style.name))?;
        if emitted.contains_key(set) {
            continue;
        }
        let index = stringsets
            .iter()
            .find(|(n, _)| n == set)
            .map(|&(_, v)| v)
            .ok_or_else(|| format!("no stringset `{set}`"))?;
        let mut blues = Vec::new();
        for entry in sets.get(index as usize..).unwrap_or_default() {
            let string = value_of(entry[0])?;
            if string == max_string {
                break;
            }
            let mut flags = 0;
            for property in entry[1].split('|').map(str::trim) {
                flags |= match properties.iter().find(|(n, _)| *n == property) {
                    Some(&(_, v)) => v,
                    None if property == "0" => 0,
                    None => return Err(format!("{set}: unexpected property `{property}`")),
                };
            }
            blues.push((blue_string(&pool, string)?, flags));
        }
        if blues.len() >= BLUE_STRINGSET_MAX_LEN {
            return Err(format!("{set}: {} blue strings", blues.len()));
        }
        let constant = set
            .strip_prefix("AF_BLUE_STRINGSET_")
            .ok_or_else(|| format!("`{set}`: not a stringset"))?;
        let constant = format!("BLUES_{constant}");
        blue_sets.push_str(&format!(
            "\n/// `{set}`.\nstatic {constant}: [BlueString; {}] = [\n",
            blues.len()
        ));
        for (text, properties) in blues {
            blue_sets.push_str(&format!(
                "    BlueString {{\n        text: {},\n        properties: 0x{properties:02X},\n    }},\n",
                rust_string(&text)
            ));
        }
        blue_sets.push_str("];\n");
        emitted.insert(set.clone(), constant);
    }

    // The styles.
    if styles.len() != STYLES {
        return Err(format!("afstyles.h: {} styles, not {STYLES}", styles.len()));
    }
    out.push_str(&format!(
        "\n/// `af_style_classes`: the styles of `afstyles.h` in order: their writing\n\
         /// system, script, blue strings (for the latin writing system) and whether\n\
         /// they cover characters by the script's ranges (`AF_COVERAGE_DEFAULT`)\n\
         /// rather than by an OpenType feature, which needs HarfBuzz.\n\
         pub(crate) static STYLES: [Style; {}] = [\n",
        styles.len()
    ));
    for style in &styles {
        let blues = match (&style.stringset, style.writing_system) {
            (Some(set), "Latin") => format!("&{}", emitted[set]),
            _ => "&[]".to_owned(),
        };
        out.push_str(&format!(
            "    // {}\n    Style {{\n        writing_system: {},\n        script: {},\n        \
             blues: {blues},\n        default_coverage: {},\n    }},\n",
            style.name, style.writing_system, style.script, style.default_coverage
        ));
    }
    out.push_str("];\n");
    let index = |name: &str| {
        styles
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| format!("no style `{name}`"))
    };
    out.push_str(&format!(
        "\n/// `AF_STYLE_FALLBACK`: the style of glyphs no script covers, CJK\n\
         /// ideographs (`AF_STYLE_HANI_DFLT`).\n\
         pub(crate) const STYLE_FALLBACK: u16 = {};\n",
        index("hani_dflt")?
    ));
    out.push_str(&blue_sets);

    // The adjustment database, in FreeType's order.
    let flags: Vec<(String, u64)> = afadjust_h
        .defines
        .iter()
        .filter(|(n, _)| n.starts_with("AF_ADJUST_") || n.starts_with("AF_IGNORE_"))
        .map(|(n, v)| Ok((n.clone(), c_number(v)?)))
        .collect::<Result<_, String>>()?;
    let mut adjustments = Vec::new();
    for entry in
        arguments(afadjust_c.array("static AF_AdjustmentDatabaseEntry  adjustment_database[]")?)
    {
        if entry.is_empty() {
            continue;
        }
        let fields = entry
            .strip_prefix('{')
            .and_then(|e| e.strip_suffix('}'))
            .map(arguments)
            .filter(|f| f.len() == 2)
            .ok_or_else(|| format!("adjustment_database: unexpected `{entry}`"))?;
        let code = c_number(fields[0])?;
        let mut value = 0;
        for flag in fields[1].split('|').map(str::trim) {
            value |= flags
                .iter()
                .find(|(n, _)| n == flag)
                .map(|&(_, v)| v)
                .ok_or_else(|| format!("adjustment_database: unexpected `{flag}`"))?;
        }
        adjustments.push(format!("(0x{code:04X}, 0x{value:04X})"));
    }
    if adjustments.len() != ADJUSTMENTS {
        return Err(format!(
            "adjustment_database: {} entries, not {ADJUSTMENTS}",
            adjustments.len()
        ));
    }
    out.push_str(&format!(
        "\n/// `adjustment_database`: the vertical adjustment flags (`AF_ADJUST_*` and\n\
         /// `AF_IGNORE_*`) of each code point, in FreeType's order, which is not\n\
         /// sorted throughout; its binary search sees the order as it is.\n\
         pub(crate) static ADJUSTMENTS: [(u32, u16); {}] = [\n",
        adjustments.len()
    ));
    rows(&mut out, &adjustments, "    ");
    out.push_str("];\n");
    Ok(out)
}

/// The scripts of `afscript.h`, in order.
fn scripts(source: &Source) -> Result<Vec<Script>, String> {
    calls(&source.text, "SCRIPT")?
        .into_iter()
        .map(|args| {
            let [lower, upper, _, _, hinting, standard] = args[..] else {
                return Err(format!("SCRIPT: {} arguments", args.len()));
            };
            let top_to_bottom = match hinting {
                "HINTING_TOP_TO_BOTTOM" => true,
                "HINTING_BOTTOM_TO_TOP" => false,
                _ => return Err(format!("{lower}: unexpected `{hinting}`")),
            };
            let standard = String::from_utf8(c_string(standard)?)
                .map_err(|_| format!("{lower}: standard characters not UTF-8"))?;
            clusters(&standard).map_err(|e| format!("{lower}: {e}"))?;
            Ok(Script {
                lower: lower.to_owned(),
                upper: upper.to_owned(),
                top_to_bottom,
                standard,
            })
        })
        .collect()
}

/// The styles of `afstyles.h`, in order, with their macros expanded.
fn styles(source: &Source, scripts: &[Script]) -> Result<Vec<Style>, String> {
    // The feature styles `META_STYLE_LATIN` adds before a script's default.
    let meta: Vec<(String, String)> = calls(source.define("META_STYLE_LATIN")?, "STYLE_LATIN")?
        .into_iter()
        .map(|args| match args[..] {
            [_, _, feature, _, _, _, coverage] => Ok((feature.to_owned(), coverage.to_owned())),
            _ => Err(format!("STYLE_LATIN: {} arguments", args.len())),
        })
        .collect::<Result<_, String>>()?;
    let script = |upper: &str| {
        scripts
            .iter()
            .position(|s| s.upper == upper)
            .ok_or_else(|| format!("no script `{upper}`"))
    };
    // Every style invocation, in the order the header lists them.
    let mut found: Vec<(usize, &str, Vec<&str>)> = Vec::new();
    for name in ["STYLE", "META_STYLE_LATIN", "STYLE_DEFAULT_INDIC"] {
        for (at, args) in placed_calls(&source.text, name)? {
            found.push((at, name, args));
        }
    }
    found.sort_by_key(|&(at, _, _)| at);
    let mut styles = Vec::new();
    for (_, name, args) in found {
        match (name, &args[..]) {
            ("STYLE", [s, _, _, ws, sc, ss, c]) => {
                let writing_system = match *ws {
                    "AF_WRITING_SYSTEM_LATIN" => "Latin",
                    "AF_WRITING_SYSTEM_CJK" => "Cjk",
                    "AF_WRITING_SYSTEM_INDIC" => "Indic",
                    "AF_WRITING_SYSTEM_DUMMY" => "Dummy",
                    _ => return Err(format!("{s}: unexpected `{ws}`")),
                };
                let upper = sc
                    .strip_prefix("AF_SCRIPT_")
                    .ok_or_else(|| format!("{s}: unexpected `{sc}`"))?;
                styles.push(Style {
                    name: (*s).to_owned(),
                    writing_system,
                    script: script(upper)?,
                    stringset: Some((*ss).to_owned()),
                    default_coverage: *c == "AF_COVERAGE_DEFAULT",
                });
            }
            ("META_STYLE_LATIN", [s, upper, _]) => {
                for (feature, coverage) in &meta {
                    styles.push(Style {
                        name: format!("{s}_{feature}"),
                        writing_system: "Latin",
                        script: script(upper)?,
                        stringset: Some(format!("AF_BLUE_STRINGSET_{upper}")),
                        default_coverage: coverage == "DEFAULT",
                    });
                }
            }
            ("STYLE_DEFAULT_INDIC", [s, upper, _]) => styles.push(Style {
                name: format!("{s}_dflt"),
                writing_system: "Indic",
                script: script(upper)?,
                stringset: None,
                default_coverage: true,
            }),
            _ => return Err(format!("{name}: {} arguments", args.len())),
        }
    }
    Ok(styles)
}

/// The ranges of the array declared by `declaration`, without the
/// terminating `(0, 0)`, which must come last and only there.
fn ranges(source: &Source, declaration: &str) -> Result<Vec<(u64, u64)>, String> {
    let mut ranges: Vec<(u64, u64)> = calls(source.array(declaration)?, "AF_UNIRANGE_REC")?
        .into_iter()
        .map(|args| match args[..] {
            [a, b] => Ok((c_number(a)?, c_number(b)?)),
            _ => Err(format!("{declaration}: unexpected range")),
        })
        .collect::<Result<_, String>>()?;
    if ranges.pop() != Some((0, 0)) || ranges.iter().any(|&(first, _)| first == 0) {
        return Err(format!("{declaration}: not terminated by its only (0, 0)"));
    }
    Ok(ranges)
}

/// The NUL-terminated string at `offset` of `af_blue_strings`, checked to
/// be clusters FreeType's reader without HarfBuzz walks safely.
fn blue_string(pool: &[u8], offset: u64) -> Result<String, String> {
    let offset = usize::try_from(offset).map_err(|_| "blue string offset")?;
    let starts_string = offset == 0 || pool.get(offset - 1) == Some(&0);
    let rest = pool
        .get(offset..)
        .filter(|_| starts_string)
        .ok_or_else(|| format!("blue string offset {offset}: not a string's start"))?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or("af_blue_strings: unterminated string")?;
    let text = String::from_utf8(rest[..end].to_vec())
        .map_err(|_| format!("blue string at {offset}: not UTF-8"))?;
    let count = clusters(&text).map_err(|e| format!("blue string at {offset}: {e}"))?;
    if count > BLUE_STRING_MAX_LEN {
        return Err(format!("blue string at {offset}: {count} clusters"));
    }
    Ok(text)
}

/// The number of space-separated clusters of `text`, which must not end in
/// a space: FreeType's reader would then step past the string's end.
fn clusters(text: &str) -> Result<usize, String> {
    if text.ends_with(' ') {
        return Err("a space ends the string".into());
    }
    Ok(text.split(' ').filter(|c| !c.is_empty()).count())
}

/// A shift macro's value, `( 1U << n )`.
fn shift_value(definition: &str) -> Result<u64, String> {
    let inner = definition
        .trim()
        .strip_prefix('(')
        .and_then(|d| d.strip_suffix(')'))
        .ok_or_else(|| format!("`{definition}`: not a shift"))?;
    let (one, shift) = inner
        .split_once("<<")
        .ok_or_else(|| format!("`{definition}`: not a shift"))?;
    let shift = c_number(shift)?;
    if c_number(one)? != 1 || shift > 15 {
        return Err(format!("`{definition}`: not a flag"));
    }
    Ok(1 << shift)
}

/// `text` as a Rust string literal, ASCII with `\u{…}` escapes.
fn rust_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            _ => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{Source, arguments, c_char, c_string, calls, enumeration, numbers};

    #[test]
    fn reads_numbers_and_letters_after_the_declaration() {
        let text = "  const char  names[4]\n#ifdef X\n  =\n  {\n    'a', 0,\n 12, 7 /* x */\n  }\n";
        assert!(
            numbers(text, "const char  names[4]", 4).is_err(),
            "comments are not values"
        );
        let text = "  const char  names[4]\n#ifdef X\n  =\n  {\n    'a', 0,\n 12, 7\n  }\n";
        assert_eq!(
            numbers(text, "const char  names[4]", 4).unwrap(),
            [97, 0, 12, 7]
        );
        assert!(numbers(text, "const char  names[4]", 5).is_err());
        assert!(numbers(text, "const char  other[4]", 4).is_err());
    }

    #[test]
    fn resolves_the_default_configuration_and_reads_literals() {
        let text = "#ifdef AF_CONFIG_OPTION_CJK\nA /* x, y */\n#else\nB\n#endif\n\
                    #if 0\nC\n#endif\n#ifndef H_\n#define H_\n#define M( a ) \\\n  f( a )\nD\n#endif\n";
        let source = Source::new("test", text).unwrap();
        assert_eq!(
            source.text.split_whitespace().collect::<Vec<_>>(),
            ["A", "D"]
        );
        assert_eq!(
            source
                .define("M")
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["(", "a", ")", "f(", "a", ")"]
        );
        assert_eq!(
            calls("X( a, \"b,c\" ), XY( 1 ), X( ( 1, 2 ) )", "X").unwrap(),
            [vec!["a", "\"b,c\""], vec!["( 1, 2 )"]]
        );
        assert_eq!(arguments("{ A, 1 | 2 }, x"), ["{ A, 1 | 2 }", "x"]);
        assert_eq!(
            c_string("\"\\xD5\\xBD \" \"o\"").unwrap(),
            [0xD5, 0xBD, b' ', b'o']
        );
        assert_eq!(c_char("'\\xF0'").unwrap(), 0xF0);
        assert_eq!(c_char("'\\0'").unwrap(), 0);
        assert_eq!(c_char("' '").unwrap(), b' ');
        let source = Source::new(
            "test",
            "typedef enum  E_\n{\n  A = 0,\n  B = 5,\n  c = B + 2,\n  MAX\n} E;\n",
        )
        .unwrap();
        let members = enumeration(&source, "E").unwrap();
        assert_eq!(
            members,
            [
                ("A".to_owned(), 0),
                ("B".to_owned(), 5),
                ("c".to_owned(), 7),
                ("MAX".to_owned(), 8)
            ]
        );
    }
}
