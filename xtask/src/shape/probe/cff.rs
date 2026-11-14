//! A CFF or CFF2 font assembled from a short declarative description, for
//! probing how a shaping engine reads its charstrings.
//!
//! The description is line based; `#` starts a comment. Face settings:
//!
//! - `format cff` (the default) or `format cff2`: the outline table;
//! - `units-per-em N` (default 1000);
//! - `nominal-width N` and `default-width N`: every Private DICT's
//!   `nominalWidthX` and `defaultWidthX` (CFF only; default 0);
//! - `font-matrix A B C D E F`: the Top DICT's FontMatrix, as decimals;
//! - `cid`: a CID-keyed CFF table (ROS, FDArray and FDSelect) even with one
//!   Font DICT; more than one Font DICT makes one CID-keyed by itself;
//! - `variation-data K...` (CFF2): a VariationStore with one ItemVariationData
//!   per value, the K-th naming the first K regions of a one-axis region list.
//!
//! Blocks, each up to a line `end`, hold charstring tokens over any number of
//! lines:
//!
//! - `global`: the next global subroutine;
//! - `font-dict [vsindex N]`: starts a Font DICT (one is implied before the
//!   first); its Private DICT takes `vsindex N` (CFF2). No body: write `end`
//!   on the same line or the next;
//! - `local`: the next local subroutine of the current Font DICT;
//! - `glyph [fd N] [advance N] [sid N]`: the next glyph's charstring (glyph 0
//!   first), selected to Font DICT N (default 0), with its `hmtx` advance
//!   (default 500) and, in a name-keyed CFF table, the string ID N in the
//!   charset (default 390 + its index, a name of its own).
//!
//! `pad-global N` and `pad-local N` add N subroutines that only return, for
//! the bias of large subroutine INDEXes.
//!
//! Charstring tokens:
//!
//! - an integer from -32768 to 32767: in the shortest Type 2 encoding (one
//!   byte, two bytes, or `28` and a 16-bit value);
//! - `short:N`: the integer in the three-byte form (`28`);
//! - `fixed:N` (decimal, as `1.5`) or `fixed:0xHHHHHHHH`: a 16.16 number
//!   (`255`), the decimal rounded to the nearest 1/65536;
//! - an operator name (`rmoveto`, `hintmask`, `add`, ...), or `op:N` and
//!   `esc:N` for the one-byte operator N and the escaped operator `12 N`;
//! - `byte:N`, or `bytes:HEX...`: raw bytes, such as a hint mask;
//! - `TOKEN*N`: the token N times; `[` tokens `]*N`: the tokens between the
//!   brackets N times (`]` alone once). Groups nest.

/// The one-byte Type 2 and CFF2 operators by name.
const OPERATORS: [(&str, u8); 25] = [
    ("hstem", 1),
    ("vstem", 3),
    ("vmoveto", 4),
    ("rlineto", 5),
    ("hlineto", 6),
    ("vlineto", 7),
    ("rrcurveto", 8),
    ("callsubr", 10),
    ("return", 11),
    ("endchar", 14),
    ("vsindex", 15),
    ("blend", 16),
    ("hstemhm", 18),
    ("hintmask", 19),
    ("cntrmask", 20),
    ("rmoveto", 21),
    ("hmoveto", 22),
    ("vstemhm", 23),
    ("rcurveline", 24),
    ("rlinecurve", 25),
    ("vvcurveto", 26),
    ("hhcurveto", 27),
    ("callgsubr", 29),
    ("vhcurveto", 30),
    ("hvcurveto", 31),
];

/// The escaped (`12 N`) Type 2 operators by name.
const ESCAPED: [(&str, u8); 25] = [
    ("dotsection", 0),
    ("and", 3),
    ("or", 4),
    ("not", 5),
    ("abs", 9),
    ("add", 10),
    ("sub", 11),
    ("div", 12),
    ("neg", 14),
    ("eq", 15),
    ("drop", 18),
    ("put", 20),
    ("get", 21),
    ("ifelse", 22),
    ("random", 23),
    ("mul", 24),
    ("sqrt", 26),
    ("dup", 27),
    ("exch", 28),
    ("index", 29),
    ("roll", 30),
    ("hflex", 34),
    ("flex", 35),
    ("hflex1", 36),
    ("flex1", 37),
];

/// A parsed description.
#[derive(Debug)]
pub(super) struct Description {
    cff2: bool,
    units_per_em: u16,
    nominal_width: i32,
    default_width: i32,
    matrix: Option<[String; 6]>,
    cid: bool,
    variation_data: Vec<u16>,
    global: Vec<Vec<u8>>,
    dicts: Vec<FontDict>,
    glyphs: Vec<Glyph>,
}

#[derive(Debug, Default)]
struct FontDict {
    vsindex: Option<i32>,
    local: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct Glyph {
    fd: u8,
    advance: u16,
    sid: Option<u16>,
    charstring: Vec<u8>,
}

impl Description {
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let mut d = Self {
            cff2: false,
            units_per_em: 1000,
            nominal_width: 0,
            default_width: 0,
            matrix: None,
            cid: false,
            variation_data: Vec::new(),
            global: Vec::new(),
            dicts: Vec::new(),
            glyphs: Vec::new(),
        };
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.split('#').next().unwrap_or("").trim()))
            .filter(|(_, l)| !l.is_empty());
        while let Some((line, text)) = lines.next() {
            let fail = |e: String| format!("line {line}: {e}");
            let mut words = text.split_whitespace();
            let key = words.next().unwrap_or("");
            let args: Vec<&str> = words.collect();
            match key {
                "format" => {
                    d.cff2 = match args.as_slice() {
                        ["cff"] => false,
                        ["cff2"] => true,
                        _ => return Err(fail("format is cff or cff2".into())),
                    }
                }
                "units-per-em" => d.units_per_em = single(&args).map_err(fail)?,
                "nominal-width" => d.nominal_width = single(&args).map_err(fail)?,
                "default-width" => d.default_width = single(&args).map_err(fail)?,
                "cid" if args.is_empty() => d.cid = true,
                "font-matrix" => {
                    let matrix: [String; 6] = args
                        .iter()
                        .map(|a| decimal_text(a).map(str::to_owned))
                        .collect::<Result<Vec<_>, _>>()
                        .and_then(|m| m.try_into().map_err(|_| "six values".to_owned()))
                        .map_err(fail)?;
                    d.matrix = Some(matrix);
                }
                "variation-data" => {
                    d.variation_data = args
                        .iter()
                        .map(|a| integer(a).and_then(|n| narrow(n, a)))
                        .collect::<Result<_, _>>()
                        .map_err(fail)?;
                }
                "global" => d.global.push(block(&args, &mut lines).map_err(fail)?),
                "pad-global" => {
                    let n: u16 = single(&args).map_err(fail)?;
                    d.global.extend((0..n).map(|_| vec![11]));
                }
                "pad-local" => {
                    let n: u16 = single(&args).map_err(fail)?;
                    if d.dicts.is_empty() {
                        d.dicts.push(FontDict::default());
                    }
                    if let Some(dict) = d.dicts.last_mut() {
                        dict.local.extend((0..n).map(|_| vec![11]));
                    }
                }
                "font-dict" => {
                    let rest = args.strip_suffix(&["end"]).unwrap_or(&args);
                    let vsindex = match rest {
                        [] => None,
                        ["vsindex", n] => Some(integer(n).map_err(fail)?),
                        _ => return Err(fail("font-dict [vsindex N]".into())),
                    };
                    if rest.len() == args.len() {
                        block(&[], &mut lines).map_err(fail)?;
                    }
                    d.dicts.push(FontDict {
                        vsindex,
                        local: Vec::new(),
                    });
                }
                "local" => {
                    let charstring = block(&args, &mut lines).map_err(fail)?;
                    if d.dicts.is_empty() {
                        d.dicts.push(FontDict::default());
                    }
                    if let Some(dict) = d.dicts.last_mut() {
                        dict.local.push(charstring);
                    }
                }
                "glyph" => {
                    let (mut fd, mut advance, mut sid) = (0, 500, None);
                    for option in args.chunks(2) {
                        match option {
                            ["fd", n] => {
                                fd = integer(n).and_then(|v| narrow(v, n)).map_err(fail)?;
                            }
                            ["advance", n] => {
                                advance = integer(n).and_then(|v| narrow(v, n)).map_err(fail)?;
                            }
                            ["sid", n] => {
                                sid = Some(integer(n).and_then(|v| narrow(v, n)).map_err(fail)?);
                            }
                            _ => return Err(fail("glyph [fd N] [advance N] [sid N]".into())),
                        }
                    }
                    let charstring = block(&[], &mut lines).map_err(fail)?;
                    d.glyphs.push(Glyph {
                        fd,
                        advance,
                        sid,
                        charstring,
                    });
                }
                _ => return Err(fail(format!("unknown setting `{key}`"))),
            }
        }
        if d.dicts.is_empty() {
            d.dicts.push(FontDict::default());
        }
        if d.glyphs.is_empty() {
            return Err("a description needs a glyph".into());
        }
        if let Some(g) = d
            .glyphs
            .iter()
            .position(|g| usize::from(g.fd) >= d.dicts.len())
        {
            return Err(format!(
                "glyph {g} selects a Font DICT the description lacks"
            ));
        }
        if !d.cff2 && !d.variation_data.is_empty() {
            return Err("variation-data is for CFF2".into());
        }
        Ok(d)
    }

    /// The font file: an `OTTO` sfnt with the outline table.
    pub(super) fn build(&self) -> Vec<u8> {
        let glyphs = self.glyphs.len() as u16;
        let outlines = if self.cff2 {
            (*b"CFF2", self.cff2_table())
        } else {
            (*b"CFF ", self.cff_table())
        };
        sfnt(vec![
            (*b"head", head(self.units_per_em)),
            (*b"hhea", hhea(glyphs, self.units_per_em)),
            (*b"maxp", maxp(glyphs)),
            (*b"OS/2", os2(self.units_per_em)),
            (
                *b"hmtx",
                self.glyphs
                    .iter()
                    .flat_map(|g| u16s(&[g.advance, 0]))
                    .collect(),
            ),
            (*b"cmap", cmap()),
            (*b"name", name()),
            (*b"post", post()),
            outlines,
        ])
    }

    fn cid_keyed(&self) -> bool {
        self.cid || self.dicts.len() > 1
    }

    /// A CFF table: name-keyed with one Private DICT, or CID-keyed with a Font
    /// DICT and Private DICT for each of the description's Font DICTs. Every
    /// offset uses the five-byte DICT integer, so the layout is computed with
    /// placeholders first.
    fn cff_table(&self) -> Vec<u8> {
        let cid = self.cid_keyed();
        let count = self.glyphs.len();
        // Name-keyed glyphs after .notdef are named by SIDs 391 and on; a
        // CID-keyed font's charset holds CIDs, and its ROS takes two strings.
        let strings: Vec<Vec<u8>> = if cid {
            vec![b"Adobe".to_vec(), b"Identity".to_vec()]
        } else {
            (1..count).map(|g| format!("g{g}").into_bytes()).collect()
        };
        let mut charset = vec![0];
        charset.extend(
            self.glyphs
                .iter()
                .enumerate()
                .skip(1)
                .flat_map(|(g, glyph)| {
                    let id = match glyph.sid {
                        Some(sid) if !cid => sid,
                        _ if cid => g as u16,
                        _ => 390 + g as u16,
                    };
                    id.to_be_bytes()
                }),
        );
        let privates: Vec<(Vec<u8>, Vec<u8>)> = self
            .dicts
            .iter()
            .map(|dict| {
                let private = |subrs: i32| {
                    let mut p = dict_int(self.nominal_width);
                    p.push(21);
                    p.extend(dict_int(self.default_width));
                    p.push(20);
                    if !dict.local.is_empty() {
                        p.extend(dict_int(subrs));
                        p.push(19);
                    }
                    p
                };
                let len = private(0).len() as i32;
                (private(len), index(&dict.local, false))
            })
            .collect();
        let top = |at: &Layout| {
            let mut t = Vec::new();
            if cid {
                t.extend(dict_int(391));
                t.extend(dict_int(392));
                t.extend(dict_int(0));
                t.extend([12, 30]);
            }
            if let Some(matrix) = &self.matrix {
                for value in matrix {
                    t.extend(real(value));
                }
                t.extend([12, 7]);
            }
            t.extend(dict_int(at.charset));
            t.push(15);
            t.extend(dict_int(at.charstrings));
            t.push(17);
            if cid {
                t.extend(dict_int(count as i32));
                t.extend([12, 34]);
                t.extend(dict_int(at.fd_array));
                t.extend([12, 36]);
                t.extend(dict_int(at.fd_select));
                t.extend([12, 37]);
            } else {
                t.extend(dict_int(privates[0].0.len() as i32));
                t.extend(dict_int(at.privates[0]));
                t.push(18);
            }
            t
        };
        let font_dicts = |at: &Layout| -> Vec<Vec<u8>> {
            privates
                .iter()
                .zip(&at.privates)
                .map(|((private, _), &offset)| {
                    let mut f = dict_int(private.len() as i32);
                    f.extend(dict_int(offset));
                    f.push(18);
                    f
                })
                .collect()
        };
        let charstrings = index(&self.charstrings(), false);
        let fd_select = self.fd_select();
        let layout = |at: &Layout| {
            let mut next = 4 + index(&[b"Probe".to_vec()], false).len();
            next += index(&[top(at)], false).len();
            next += index(&strings, false).len();
            next += index(&self.global, false).len();
            let mut l = Layout {
                charstrings: next as i32,
                ..Layout::default()
            };
            next += charstrings.len();
            l.charset = next as i32;
            next += charset.len();
            if cid {
                l.fd_select = next as i32;
                next += fd_select.len();
                l.fd_array = next as i32;
                next += index(&font_dicts(at), false).len();
            }
            for (private, local) in &privates {
                l.privates.push(next as i32);
                next += private.len() + local.len();
            }
            l
        };
        let placeholder = Layout {
            privates: vec![0; privates.len()],
            ..Layout::default()
        };
        let at = layout(&layout(&placeholder));
        let mut out = vec![1, 0, 4, 4];
        out.extend(index(&[b"Probe".to_vec()], false));
        out.extend(index(&[top(&at)], false));
        out.extend(index(&strings, false));
        out.extend(index(&self.global, false));
        out.extend(charstrings);
        out.extend(charset);
        if cid {
            out.extend(fd_select);
            out.extend(index(&font_dicts(&at), false));
        }
        for (private, local) in privates {
            out.extend(private);
            out.extend(local);
        }
        out
    }

    /// A CFF2 table: a Font DICT and Private DICT for each of the
    /// description's Font DICTs, an FDSelect when there is more than one, and
    /// the VariationStore of `variation-data`.
    fn cff2_table(&self) -> Vec<u8> {
        let privates: Vec<(Vec<u8>, Vec<u8>)> = self
            .dicts
            .iter()
            .map(|dict| {
                let private = |subrs: i32| {
                    let mut p = Vec::new();
                    if let Some(vsindex) = dict.vsindex {
                        p.extend(dict_int(vsindex));
                        p.push(22);
                    }
                    if !dict.local.is_empty() {
                        p.extend(dict_int(subrs));
                        p.push(19);
                    }
                    p
                };
                let len = private(0).len() as i32;
                (private(len), index(&dict.local, true))
            })
            .collect();
        let several = self.dicts.len() > 1;
        let store = self.variation_store();
        let top = |at: &Layout| {
            let mut t = Vec::new();
            if let Some(matrix) = &self.matrix {
                for value in matrix {
                    t.extend(real(value));
                }
                t.extend([12, 7]);
            }
            t.extend(dict_int(at.charstrings));
            t.push(17);
            if store.is_some() {
                t.extend(dict_int(at.store));
                t.push(24);
            }
            t.extend(dict_int(at.fd_array));
            t.extend([12, 36]);
            if several {
                t.extend(dict_int(at.fd_select));
                t.extend([12, 37]);
            }
            t
        };
        let font_dicts = |at: &Layout| -> Vec<Vec<u8>> {
            privates
                .iter()
                .zip(&at.privates)
                .map(|((private, _), &offset)| {
                    let mut f = dict_int(private.len() as i32);
                    f.extend(dict_int(offset));
                    f.push(18);
                    f
                })
                .collect()
        };
        let charstrings = index(&self.charstrings(), true);
        let fd_select = self.fd_select();
        let layout = |at: &Layout| {
            let mut next = 5 + top(at).len() + index(&self.global, true).len();
            let mut l = Layout {
                charstrings: next as i32,
                ..Layout::default()
            };
            next += charstrings.len();
            if let Some(store) = &store {
                l.store = next as i32;
                next += store.len();
            }
            if several {
                l.fd_select = next as i32;
                next += fd_select.len();
            }
            l.fd_array = next as i32;
            next += index(&font_dicts(at), true).len();
            for (private, local) in &privates {
                l.privates.push(next as i32);
                next += private.len() + local.len();
            }
            l
        };
        let placeholder = Layout {
            privates: vec![0; privates.len()],
            ..Layout::default()
        };
        let at = layout(&layout(&placeholder));
        let top = top(&at);
        let mut out = vec![2, 0, 5];
        out.extend((top.len() as u16).to_be_bytes());
        out.extend(top);
        out.extend(index(&self.global, true));
        out.extend(charstrings);
        if let Some(store) = store {
            out.extend(store);
        }
        if several {
            out.extend(fd_select);
        }
        out.extend(index(&font_dicts(&at), true));
        for (private, local) in privates {
            out.extend(private);
            out.extend(local);
        }
        out
    }

    fn charstrings(&self) -> Vec<Vec<u8>> {
        self.glyphs.iter().map(|g| g.charstring.clone()).collect()
    }

    /// FDSelect format 3: one range per run of glyphs with the same Font DICT.
    fn fd_select(&self) -> Vec<u8> {
        let mut ranges: Vec<(u16, u8)> = Vec::new();
        for (g, glyph) in self.glyphs.iter().enumerate() {
            if ranges.last().is_none_or(|&(_, fd)| fd != glyph.fd) {
                ranges.push((g as u16, glyph.fd));
            }
        }
        let mut out = vec![3];
        out.extend((ranges.len() as u16).to_be_bytes());
        for (first, fd) in ranges {
            out.extend(first.to_be_bytes());
            out.push(fd);
        }
        out.extend((self.glyphs.len() as u16).to_be_bytes());
        out
    }

    /// The VariationStore: its length, then an ItemVariationStore over a
    /// one-axis region list of as many regions as the largest ItemVariationData
    /// names, each peaking at the axis maximum.
    fn variation_store(&self) -> Option<Vec<u8>> {
        let regions = *self.variation_data.iter().max()?;
        let mut list = u16s(&[1, regions]);
        for _ in 0..regions {
            list.extend(u16s(&[0, 0x4000, 0x4000]));
        }
        let data: Vec<Vec<u8>> = self
            .variation_data
            .iter()
            .map(|&k| {
                let mut d = u16s(&[0, 0, k]);
                d.extend(u16s(&(0..k).collect::<Vec<_>>()));
                d
            })
            .collect();
        let header = 8 + 4 * data.len();
        let mut store = 1_u16.to_be_bytes().to_vec();
        store.extend((header as u32).to_be_bytes());
        store.extend((data.len() as u16).to_be_bytes());
        let mut at = header + list.len();
        for d in &data {
            store.extend((at as u32).to_be_bytes());
            at += d.len();
        }
        store.extend(list);
        store.extend(data.concat());
        let mut out = (store.len() as u16).to_be_bytes().to_vec();
        out.extend(store);
        Some(out)
    }
}

/// Where a table's structures start, for the offsets its DICTs hold.
#[derive(Default)]
struct Layout {
    charstrings: i32,
    charset: i32,
    fd_select: i32,
    fd_array: i32,
    store: i32,
    privates: Vec<i32>,
}

/// A charstring from the tokens of a block: those after the keyword on its
/// first line, then every line up to `end`.
fn block<'a>(
    first: &[&str],
    lines: &mut impl Iterator<Item = (usize, &'a str)>,
) -> Result<Vec<u8>, String> {
    let mut tokens: Vec<String> = first.iter().map(|&t| t.to_owned()).collect();
    while !tokens.iter().any(|t| t == "end") {
        let (_, text) = lines.next().ok_or("a block without `end`")?;
        tokens.extend(text.split_whitespace().map(str::to_owned));
    }
    let end = tokens
        .iter()
        .position(|t| t == "end")
        .unwrap_or(tokens.len());
    if end + 1 != tokens.len() {
        return Err("tokens after `end`".into());
    }
    let mut out = Vec::new();
    let mut rest = &tokens[..end];
    while !rest.is_empty() {
        rest = sequence(rest, &mut out)?;
    }
    Ok(out)
}

/// The bytes of the first token of `tokens`, or of the group `[ ... ]` it
/// starts, either repeated when followed by `*N`; and the tokens after it.
fn sequence<'t>(tokens: &'t [String], out: &mut Vec<u8>) -> Result<&'t [String], String> {
    let (first, rest) = tokens.split_first().ok_or("an empty group")?;
    if first == "[" {
        let mut inner = Vec::new();
        let mut rest = rest;
        loop {
            let next = rest.first().ok_or("a group without `]`")?;
            if let Some(count) = next.strip_prefix(']') {
                let count = match count.strip_prefix('*') {
                    Some(n) => usize::try_from(integer(n)?).map_err(|_| format!("`{next}`"))?,
                    None if count.is_empty() => 1,
                    None => return Err(format!("`{next}` is not `]` or `]*N`")),
                };
                out.extend(inner.repeat(count));
                return Ok(&rest[1..]);
            }
            rest = sequence(rest, &mut inner)?;
        }
    }
    let (token, count) = match first.rsplit_once('*') {
        Some((token, n)) => (
            token,
            usize::try_from(integer(n)?).map_err(|_| format!("`{first}`"))?,
        ),
        None => (first.as_str(), 1),
    };
    out.extend(assemble(token)?.repeat(count));
    Ok(rest)
}

/// The bytes of one charstring token.
fn assemble(token: &str) -> Result<Vec<u8>, String> {
    if let Some(&(_, op)) = OPERATORS.iter().find(|(name, _)| *name == token) {
        return Ok(vec![op]);
    }
    if let Some(&(_, op)) = ESCAPED.iter().find(|(name, _)| *name == token) {
        return Ok(vec![12, op]);
    }
    let byte = |text: &str| integer(text).and_then(|v| narrow::<u8>(v, text));
    if let Some((kind, value)) = token.split_once(':') {
        return match kind {
            "op" => Ok(vec![byte(value)?]),
            "esc" => Ok(vec![12, byte(value)?]),
            "byte" => Ok(vec![byte(value)?]),
            "bytes" => hex_bytes(value),
            "short" => {
                let v: i16 = narrow(integer(value)?, value)?;
                let mut out = vec![28];
                out.extend(v.to_be_bytes());
                Ok(out)
            }
            "fixed" => {
                let raw = match value.strip_prefix("0x") {
                    Some(hex) => u32::from_str_radix(hex, 16)
                        .map_err(|_| format!("`{token}` is not a 32-bit value"))?,
                    None => fixed(value)?,
                };
                let mut out = vec![255];
                out.extend(raw.to_be_bytes());
                Ok(out)
            }
            _ => Err(format!("unknown token `{token}`")),
        };
    }
    let value = integer(token)?;
    Ok(match value {
        -107..=107 => vec![(value + 139) as u8],
        108..=1131 => {
            let v = value - 108;
            vec![(v / 256 + 247) as u8, (v % 256) as u8]
        }
        -1131..=-108 => {
            let v = -value - 108;
            vec![(v / 256 + 251) as u8, (v % 256) as u8]
        }
        -32768..=32767 => {
            let mut out = vec![28];
            out.extend((value as i16).to_be_bytes());
            out
        }
        _ => return Err(format!("`{token}` does not fit a charstring number")),
    })
}

/// A decimal as 16.16 bits, rounded to the nearest 1/65536.
fn fixed(text: &str) -> Result<u32, String> {
    let value: f64 = decimal_text(text)?
        .parse()
        .map_err(|_| format!("`{text}` is not a decimal"))?;
    let scaled = (value * 65536.0).round();
    if !(-2_147_483_648.0..=2_147_483_647.0).contains(&scaled) {
        return Err(format!("`{text}` does not fit 16.16"));
    }
    Ok((scaled as i32).cast_unsigned())
}

fn hex_bytes(text: &str) -> Result<Vec<u8>, String> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return Err(format!("`{text}` is not hexadecimal bytes"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&text[i..i + 2], 16)
                .map_err(|_| format!("`{text}` is not hexadecimal"))
        })
        .collect()
}

/// A decimal number's text: an optional `-`, digits and at most one `.`.
fn decimal_text(text: &str) -> Result<&str, String> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let valid = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1
        && digits != ".";
    valid
        .then_some(text)
        .ok_or_else(|| format!("`{text}` is not a decimal"))
}

fn integer(token: &str) -> Result<i32, String> {
    let (negative, digits) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let magnitude = match digits.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(hex, 16),
        None => digits.parse::<i64>(),
    }
    .map_err(|_| format!("`{token}` is not a number"))?;
    i32::try_from(if negative { -magnitude } else { magnitude })
        .map_err(|_| format!("`{token}` does not fit 32 bits"))
}

fn narrow<T: TryFrom<i32>>(value: i32, text: &str) -> Result<T, String> {
    T::try_from(value).map_err(|_| format!("{text} is out of range"))
}

fn single<T: TryFrom<i32>>(args: &[&str]) -> Result<T, String> {
    match args {
        [one] => narrow(integer(one)?, one),
        _ => Err("expected one value".into()),
    }
}

/// A DICT integer in the five-byte form (`29`), so offsets do not move.
fn dict_int(value: i32) -> Vec<u8> {
    let mut v = vec![29];
    v.extend(value.to_be_bytes());
    v
}

/// A DICT real (TN 5176 Table 5) from its decimal text.
fn real(text: &str) -> Vec<u8> {
    let mut nibbles: Vec<u8> = text
        .bytes()
        .map(|c| match c {
            b'.' => 0xa,
            b'-' => 0xe,
            digit => digit - b'0',
        })
        .collect();
    nibbles.push(0xf);
    if !nibbles.len().is_multiple_of(2) {
        nibbles.push(0xf);
    }
    let mut out = vec![30];
    out.extend(nibbles.chunks(2).map(|pair| (pair[0] << 4) | pair[1]));
    out
}

/// An INDEX with four-byte offsets; a CFF2 INDEX has a 32-bit count.
fn index(objects: &[Vec<u8>], cff2: bool) -> Vec<u8> {
    let count = objects.len() as u32;
    let mut out = if cff2 {
        count.to_be_bytes().to_vec()
    } else {
        (count as u16).to_be_bytes().to_vec()
    };
    if objects.is_empty() {
        return out;
    }
    out.push(4);
    let mut offset = 1_u32;
    out.extend(offset.to_be_bytes());
    for object in objects {
        offset += object.len() as u32;
        out.extend(offset.to_be_bytes());
    }
    for object in objects {
        out.extend(object);
    }
    out
}

fn u16s(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn head(units_per_em: u16) -> Vec<u8> {
    let mut t = 0x0001_0000_u32.to_be_bytes().to_vec();
    t.extend([0; 4]); // fontRevision
    t.extend([0; 4]); // checkSumAdjustment, set with the file
    t.extend(0x5F0F_3CF5_u32.to_be_bytes());
    t.extend(u16s(&[0, units_per_em]));
    t.extend([0; 16]); // created, modified
    t.extend([0; 8]); // the box
    t.extend(u16s(&[0, 8, 2, 0, 0])); // macStyle, lowestRecPPEM, direction, loca, glyf
    t
}

fn hhea(glyphs: u16, units_per_em: u16) -> Vec<u8> {
    let ascender = units_per_em - units_per_em / 5;
    let mut t = 0x0001_0000_u32.to_be_bytes().to_vec();
    t.extend(u16s(&[
        ascender,
        (ascender as i16 - units_per_em as i16) as u16,
        0,
    ]));
    t.extend(u16s(&[units_per_em, 0, 0, units_per_em, 1, 0, 0]));
    t.extend([0; 8]);
    t.extend(u16s(&[0, glyphs]));
    t
}

/// A version 0.5 `maxp`, as CFF faces carry.
fn maxp(glyphs: u16) -> Vec<u8> {
    let mut t = 0x0000_5000_u32.to_be_bytes().to_vec();
    t.extend(glyphs.to_be_bytes());
    t
}

fn os2(units_per_em: u16) -> Vec<u8> {
    let ascender = units_per_em - units_per_em / 5;
    let descender = ascender as i16 - units_per_em as i16;
    let mut t = Vec::with_capacity(96);
    t.extend(u16s(&[4, 0, 400, 5]));
    t.extend([0; 24]); // fsType, sub- and superscripts, strikeout, sFamilyClass
    t.extend([0; 10]); // panose
    t.extend([0; 16]); // ulUnicodeRange1..4
    t.extend(*b"TRGN");
    t.extend(u16s(&[0x0040, 0, 0xFFFF, ascender, descender as u16, 0]));
    t.extend(u16s(&[ascender, descender.unsigned_abs()]));
    t.extend([0; 8]); // ulCodePageRange1, 2
    t.extend([0; 10]); // sxHeight, sCapHeight, usDefaultChar, usBreakChar, usMaxContext
    t
}

/// A format 4 `cmap` that maps nothing.
fn cmap() -> Vec<u8> {
    let mut t = u16s(&[0, 1, 3, 1, 0, 12]);
    t.extend(u16s(&[4, 24, 0, 2, 2, 0, 0, 0xFFFF, 0, 0xFFFF, 1, 0]));
    t
}

fn name() -> Vec<u8> {
    let records: [(u16, &str); 4] = [
        (1, "Triergon Probe"),
        (2, "Regular"),
        (4, "Triergon Probe Regular"),
        (6, "TriergonProbe-Regular"),
    ];
    let mut strings = Vec::new();
    let mut t = u16s(&[0, records.len() as u16, (6 + 12 * records.len()) as u16]);
    for (id, text) in records {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        t.extend(u16s(&[
            3,
            1,
            0x0409,
            id,
            utf16.len() as u16,
            strings.len() as u16,
        ]));
        strings.extend(utf16);
    }
    t.extend(strings);
    t
}

fn post() -> Vec<u8> {
    let mut t = vec![0, 3, 0, 0]; // version 3.0: no glyph names
    t.extend([0; 4]); // italicAngle
    t.extend(u16s(&[(-100_i16) as u16, 50]));
    t.extend([0; 20]); // isFixedPitch, memory hints
    t
}

/// The font file: the table directory, then each table padded to four bytes,
/// with every checksum and `head`'s checksum adjustment.
fn sfnt(mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|t| t.0);
    let count = tables.len();
    let search = 1_usize << count.ilog2();
    let mut out = b"OTTO".to_vec();
    out.extend(u16s(&[
        count as u16,
        (16 * search) as u16,
        count.ilog2() as u16,
        (16 * (count - search)) as u16,
    ]));
    let mut offset = 12 + 16 * count;
    let mut head_at = None;
    for (tag, data) in &tables {
        if tag == b"head" {
            head_at = Some(offset);
        }
        out.extend(tag);
        out.extend(checksum(data).to_be_bytes());
        out.extend((offset as u32).to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &tables {
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if let Some(at) = head_at {
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum(&out));
        out[at + 8..at + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    out
}

/// The sum of the big-endian 32-bit words of `data`, zero padded.
fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0_u32, |sum, chunk| {
        let mut word = [0; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

#[cfg(test)]
mod tests {
    use super::{Description, assemble};

    #[test]
    fn tokens_take_the_type_2_encodings() {
        assert_eq!(assemble("0").unwrap(), [139]);
        assert_eq!(assemble("-107").unwrap(), [32]);
        assert_eq!(assemble("108").unwrap(), [247, 0]);
        assert_eq!(assemble("-1131").unwrap(), [254, 255]);
        assert_eq!(assemble("1132").unwrap(), [28, 4, 108]);
        assert_eq!(assemble("short:5").unwrap(), [28, 0, 5]);
        assert_eq!(assemble("fixed:1.5").unwrap(), [255, 0, 1, 0x80, 0]);
        assert_eq!(assemble("fixed:-0.5").unwrap(), [255, 0xFF, 0xFF, 0x80, 0]);
        assert!(assemble("40000").is_err());
        assert_eq!(assemble("hintmask").unwrap(), [19]);
        assert_eq!(assemble("flex1").unwrap(), [12, 37]);
        assert_eq!(assemble("esc:16").unwrap(), [12, 16]);
        assert_eq!(assemble("bytes:c0ff").unwrap(), [0xC0, 0xFF]);
        assert!(assemble("nonsense").is_err());
    }

    /// The table of `font` with `tag`.
    fn table<'a>(font: &'a [u8], tag: &[u8; 4]) -> &'a [u8] {
        let count = usize::from(u16::from_be_bytes([font[4], font[5]]));
        let record = (0..count)
            .map(|i| 12 + 16 * i)
            .find(|&r| &font[r..r + 4] == tag)
            .unwrap();
        let word = |at: usize| u32::from_be_bytes(font[at..at + 4].try_into().unwrap()) as usize;
        &font[word(record + 8)..][..word(record + 12)]
    }

    #[test]
    fn a_cff_description_builds_its_charstrings_and_subroutines() {
        let d = Description::parse(
            "# a square\nnominal-width 10\nglobal 0 0 rlineto return end\n\
             local\n  50 hlineto return\nend\nglyph advance 300\n  endchar\nend\n\
             glyph\n  10 20 rmoveto -107 callsubr\n  -107 callgsubr endchar\nend\n",
        )
        .unwrap();
        let font = d.build();
        assert_eq!(&font[..4], b"OTTO");
        let cff = table(&font, b"CFF ");
        assert_eq!(&cff[..4], [1, 0, 4, 4]);
        let find = |needle: &[u8]| cff.windows(needle.len()).any(|w| w == needle);
        assert!(find(&[149, 159, 21, 32, 10, 32, 29, 14]));
        assert!(find(&[189, 6, 11]));
        assert!(find(&[139, 139, 5, 11]));
        assert_eq!(&table(&font, b"hmtx")[..2], 300_u16.to_be_bytes());
    }

    #[test]
    fn a_cff2_description_has_its_header_and_store() {
        let d = Description::parse(
            "format cff2\nvariation-data 2\nfont-dict vsindex 0 end\n\
             glyph\n  10 1 2 1 blend hmoveto\nend\n",
        )
        .unwrap();
        let font = d.build();
        let cff2 = table(&font, b"CFF2");
        assert_eq!(&cff2[..3], [2, 0, 5]);
        assert!(cff2.windows(6).any(|w| w == [149, 140, 141, 140, 16, 22]));
    }

    #[test]
    fn tokens_and_groups_repeat() {
        let d = Description::parse("glyph\n  1*3 [ 2 [ hlineto ]*2 ]*2 endchar\nend\n").unwrap();
        assert_eq!(
            d.glyphs[0].charstring,
            [140, 140, 140, 141, 6, 6, 141, 6, 6, 14]
        );
        assert!(Description::parse("glyph\n  [ 1 endchar\nend\n").is_err());
    }

    #[test]
    fn mistakes_are_reported_with_their_line() {
        assert!(Description::parse("glyph\n rmoveto\n").is_err());
        let error = Description::parse("glyph\n  frobnicate\nend\n").unwrap_err();
        assert!(error.contains("frobnicate"), "{error}");
        let error = Description::parse("glyph fd 1\nend\n").unwrap_err();
        assert!(error.contains("Font DICT"), "{error}");
    }
}
