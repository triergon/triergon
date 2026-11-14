//! A TrueType instruction assembler: mnemonics as the TrueType instruction set
//! names them (Apple's TrueType Reference Manual, chapter 5 and appendix A;
//! OpenType, "The TrueType Instruction Set"), with variant flags written as the
//! specification writes them, `MDRP[10110]`.
//!
//! One instruction per line, or several separated by `;`:
//!
//! - `NAME` or `NAME[]` for an instruction without flags, `NAME[bits]` with
//!   exactly as many binary digits as the instruction has flags;
//! - `PUSHB v...`, `PUSHW v...`, `NPUSHB v...` and `NPUSHW v...` push their
//!   values with that instruction; `PUSH v...` picks the shortest encoding
//!   (bytes when every value is in 0..=255, else words);
//! - a value is a decimal or `0x` hexadecimal integer, or `@to-@from`, the
//!   distance in bytes from label `from` to label `to`, always pushed as a word;
//! - `@name:` defines a label at the next instruction;
//! - `BYTES b...` emits raw bytes, for opcodes the instruction set leaves
//!   undefined and for malformed streams.

use std::collections::HashMap;

/// An instruction's name, its lowest opcode and its number of flag bits.
const INSTRUCTIONS: &[(&str, u8, u32)] = &[
    ("SVTCA", 0x00, 1),
    ("SPVTCA", 0x02, 1),
    ("SFVTCA", 0x04, 1),
    ("SPVTL", 0x06, 1),
    ("SFVTL", 0x08, 1),
    ("SPVFS", 0x0A, 0),
    ("SFVFS", 0x0B, 0),
    ("GPV", 0x0C, 0),
    ("GFV", 0x0D, 0),
    ("SFVTPV", 0x0E, 0),
    ("ISECT", 0x0F, 0),
    ("SRP0", 0x10, 0),
    ("SRP1", 0x11, 0),
    ("SRP2", 0x12, 0),
    ("SZP0", 0x13, 0),
    ("SZP1", 0x14, 0),
    ("SZP2", 0x15, 0),
    ("SZPS", 0x16, 0),
    ("SLOOP", 0x17, 0),
    ("RTG", 0x18, 0),
    ("RTHG", 0x19, 0),
    ("SMD", 0x1A, 0),
    ("ELSE", 0x1B, 0),
    ("JMPR", 0x1C, 0),
    ("SCVTCI", 0x1D, 0),
    ("SSWCI", 0x1E, 0),
    ("SSW", 0x1F, 0),
    ("DUP", 0x20, 0),
    ("POP", 0x21, 0),
    ("CLEAR", 0x22, 0),
    ("SWAP", 0x23, 0),
    ("DEPTH", 0x24, 0),
    ("CINDEX", 0x25, 0),
    ("MINDEX", 0x26, 0),
    ("ALIGNPTS", 0x27, 0),
    ("UTP", 0x29, 0),
    ("LOOPCALL", 0x2A, 0),
    ("CALL", 0x2B, 0),
    ("FDEF", 0x2C, 0),
    ("ENDF", 0x2D, 0),
    ("MDAP", 0x2E, 1),
    ("IUP", 0x30, 1),
    ("SHP", 0x32, 1),
    ("SHC", 0x34, 1),
    ("SHZ", 0x36, 1),
    ("SHPIX", 0x38, 0),
    ("IP", 0x39, 0),
    ("MSIRP", 0x3A, 1),
    ("ALIGNRP", 0x3C, 0),
    ("RTDG", 0x3D, 0),
    ("MIAP", 0x3E, 1),
    ("WS", 0x42, 0),
    ("RS", 0x43, 0),
    ("WCVTP", 0x44, 0),
    ("RCVT", 0x45, 0),
    ("GC", 0x46, 1),
    ("SCFS", 0x48, 0),
    ("MD", 0x49, 1),
    ("MPPEM", 0x4B, 0),
    ("MPS", 0x4C, 0),
    ("FLIPON", 0x4D, 0),
    ("FLIPOFF", 0x4E, 0),
    ("DEBUG", 0x4F, 0),
    ("LT", 0x50, 0),
    ("LTEQ", 0x51, 0),
    ("GT", 0x52, 0),
    ("GTEQ", 0x53, 0),
    ("EQ", 0x54, 0),
    ("NEQ", 0x55, 0),
    ("ODD", 0x56, 0),
    ("EVEN", 0x57, 0),
    ("IF", 0x58, 0),
    ("EIF", 0x59, 0),
    ("AND", 0x5A, 0),
    ("OR", 0x5B, 0),
    ("NOT", 0x5C, 0),
    ("DELTAP1", 0x5D, 0),
    ("SDB", 0x5E, 0),
    ("SDS", 0x5F, 0),
    ("ADD", 0x60, 0),
    ("SUB", 0x61, 0),
    ("DIV", 0x62, 0),
    ("MUL", 0x63, 0),
    ("ABS", 0x64, 0),
    ("NEG", 0x65, 0),
    ("FLOOR", 0x66, 0),
    ("CEILING", 0x67, 0),
    ("ROUND", 0x68, 2),
    ("NROUND", 0x6C, 2),
    ("WCVTF", 0x70, 0),
    ("DELTAP2", 0x71, 0),
    ("DELTAP3", 0x72, 0),
    ("DELTAC1", 0x73, 0),
    ("DELTAC2", 0x74, 0),
    ("DELTAC3", 0x75, 0),
    ("SROUND", 0x76, 0),
    ("S45ROUND", 0x77, 0),
    ("JROT", 0x78, 0),
    ("JROF", 0x79, 0),
    ("ROFF", 0x7A, 0),
    ("RUTG", 0x7C, 0),
    ("RDTG", 0x7D, 0),
    ("SANGW", 0x7E, 0),
    ("AA", 0x7F, 0),
    ("FLIPPT", 0x80, 0),
    ("FLIPRGON", 0x81, 0),
    ("FLIPRGOFF", 0x82, 0),
    ("SCANCTRL", 0x85, 0),
    ("SDPVTL", 0x86, 1),
    ("GETINFO", 0x88, 0),
    ("IDEF", 0x89, 0),
    ("ROLL", 0x8A, 0),
    ("MAX", 0x8B, 0),
    ("MIN", 0x8C, 0),
    ("SCANTYPE", 0x8D, 0),
    ("INSTCTRL", 0x8E, 0),
    ("GETVARIATION", 0x91, 0),
    ("MDRP", 0xC0, 5),
    ("MIRP", 0xE0, 5),
];

/// A value to push: a number, or a label distance resolved once every label
/// has an address.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Number(i32),
    Distance { to: String, from: String },
}

/// How a push encodes its values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Push {
    Bytes,
    Words,
    CountedBytes,
    CountedWords,
}

impl Push {
    const fn words(self) -> bool {
        matches!(self, Self::Words | Self::CountedWords)
    }

    const fn len(self, count: usize) -> usize {
        let header = match self {
            Self::Bytes | Self::Words => 1,
            Self::CountedBytes | Self::CountedWords => 2,
        };
        header + count * if self.words() { 2 } else { 1 }
    }
}

enum Item {
    Bytes(Vec<u8>),
    Push(Push, Vec<Value>),
    Label(String),
}

/// Assembles `source` (lines of the form the module describes) into an
/// instruction stream. `line` numbers the first line in error messages.
pub(super) fn assemble(source: &[(usize, &str)]) -> Result<Vec<u8>, String> {
    let mut items = Vec::new();
    for &(line, text) in source {
        for statement in text.split(';') {
            let tokens: Vec<&str> = statement.split_whitespace().collect();
            if tokens.is_empty() {
                continue;
            }
            items.push(statement_item(&tokens).map_err(|e| format!("line {line}: {e}"))?);
        }
    }
    // Every item's size is known before labels resolve: a label distance is a word.
    let mut labels = HashMap::new();
    let mut at = 0_usize;
    for item in &items {
        match item {
            Item::Bytes(bytes) => at += bytes.len(),
            Item::Push(kind, values) => at += kind.len(values.len()),
            Item::Label(name) => {
                if labels.insert(name.clone(), at).is_some() {
                    return Err(format!("label @{name} defined twice"));
                }
            }
        }
    }
    let mut out = Vec::with_capacity(at);
    for item in items {
        match item {
            Item::Bytes(bytes) => out.extend(bytes),
            Item::Push(kind, values) => {
                let count = values.len();
                match kind {
                    Push::Bytes => out.push(0xB0 + (count - 1) as u8),
                    Push::Words => out.push(0xB8 + (count - 1) as u8),
                    Push::CountedBytes => out.extend([0x40, count as u8]),
                    Push::CountedWords => out.extend([0x41, count as u8]),
                }
                for value in values {
                    let v = resolve(&value, &labels)?;
                    if kind.words() {
                        let word = i16::try_from(v)
                            .map_err(|_| format!("{v} does not fit a pushed word"))?;
                        out.extend(word.to_be_bytes());
                    } else {
                        out.push(u8::try_from(v).map_err(|_| format!("{v} is not a byte"))?);
                    }
                }
            }
            Item::Label(_) => {}
        }
    }
    Ok(out)
}

fn resolve(value: &Value, labels: &HashMap<String, usize>) -> Result<i32, String> {
    match value {
        Value::Number(n) => Ok(*n),
        Value::Distance { to, from } => {
            let at = |name: &String| {
                labels
                    .get(name)
                    .map(|&a| a as i64)
                    .ok_or_else(|| format!("undefined label @{name}"))
            };
            i32::try_from(at(to)? - at(from)?).map_err(|_| "label distance overflows".into())
        }
    }
}

fn statement_item(tokens: &[&str]) -> Result<Item, String> {
    let (head, args) = (tokens[0], &tokens[1..]);
    if let Some(label) = head.strip_prefix('@') {
        let name = label
            .strip_suffix(':')
            .filter(|n| !n.is_empty() && args.is_empty())
            .ok_or_else(|| format!("a label is written `@name:`, not `{}`", tokens.join(" ")))?;
        return Ok(Item::Label(name.to_owned()));
    }
    let push = match head {
        "PUSH" => Some(None),
        "PUSHB" => Some(Some(Push::Bytes)),
        "PUSHW" => Some(Some(Push::Words)),
        "NPUSHB" => Some(Some(Push::CountedBytes)),
        "NPUSHW" => Some(Some(Push::CountedWords)),
        _ => None,
    };
    if let Some(kind) = push {
        let values = args
            .iter()
            .map(|a| value(a))
            .collect::<Result<Vec<_>, _>>()?;
        return push_item(kind, values);
    }
    if head == "BYTES" {
        let bytes = args
            .iter()
            .map(|a| {
                number(a).and_then(|n| u8::try_from(n).map_err(|_| format!("{a} is not a byte")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Item::Bytes(bytes));
    }
    if !args.is_empty() {
        return Err(format!(
            "{head} takes its arguments from the stack; push them first"
        ));
    }
    Ok(Item::Bytes(vec![opcode(head)?]))
}

fn push_item(kind: Option<Push>, values: Vec<Value>) -> Result<Item, String> {
    let count = values.len();
    let fits_byte = |v: &Value| matches!(v, Value::Number(n) if (0..=255).contains(n));
    let kind = kind.unwrap_or_else(|| match (values.iter().all(fits_byte), count <= 8) {
        (true, true) => Push::Bytes,
        (true, false) => Push::CountedBytes,
        (false, true) => Push::Words,
        (false, false) => Push::CountedWords,
    });
    let range = match kind {
        Push::Bytes | Push::Words => 1..=8,
        Push::CountedBytes | Push::CountedWords => 0..=255,
    };
    if !range.contains(&count) {
        return Err(format!("{count} values do not fit this push"));
    }
    if !kind.words() && values.iter().any(|v| !fits_byte(v)) {
        return Err("a byte push takes values 0 to 255".into());
    }
    Ok(Item::Push(kind, values))
}

fn value(token: &str) -> Result<Value, String> {
    if let Some(rest) = token.strip_prefix('@') {
        let (to, from) = rest
            .split_once("-@")
            .ok_or_else(|| format!("a label distance is written `@to-@from`, not `{token}`"))?;
        return Ok(Value::Distance {
            to: to.to_owned(),
            from: from.to_owned(),
        });
    }
    number(token).map(Value::Number)
}

/// A decimal or `0x` hexadecimal integer, optionally negative.
pub(super) fn number(token: &str) -> Result<i32, String> {
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

/// The opcode of `NAME`, `NAME[]` or `NAME[bits]`.
fn opcode(token: &str) -> Result<u8, String> {
    let (name, bits) = match token.split_once('[') {
        Some((name, rest)) => (
            name,
            Some(
                rest.strip_suffix(']')
                    .ok_or_else(|| format!("unterminated flags in `{token}`"))?,
            ),
        ),
        None => (token, None),
    };
    let &(_, base, flags) = INSTRUCTIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .ok_or_else(|| format!("unknown instruction `{name}`"))?;
    let bits = bits.unwrap_or("");
    if bits.len() != flags as usize || !bits.bytes().all(|b| b == b'0' || b == b'1') {
        return Err(format!("{name} takes {flags} flag digits, as in `{token}`"));
    }
    let variant = bits
        .bytes()
        .fold(0_u8, |acc, b| (acc << 1) | u8::from(b == b'1'));
    Ok(base + variant)
}

#[cfg(test)]
mod tests {
    use super::assemble;

    fn asm(text: &str) -> Result<Vec<u8>, String> {
        let lines: Vec<(usize, &str)> = text.lines().enumerate().collect();
        assemble(&lines)
    }

    #[test]
    fn variants_add_their_flags_to_the_base_opcode() {
        // The specification's example: five flags 11101 on 0xC0 give 0xDD.
        assert_eq!(asm("MDRP[11101]"), Ok(vec![0xDD]));
        assert_eq!(
            asm("MIRP[00000]; SVTCA[1]; IUP[0]"),
            Ok(vec![0xE0, 0x01, 0x30])
        );
        assert_eq!(asm("ROUND[10]; NROUND[01]"), Ok(vec![0x6A, 0x6D]));
        assert_eq!(asm("ADD; ADD[]; GETINFO"), Ok(vec![0x60, 0x60, 0x88]));
        assert!(asm("MDRP[101]").is_err());
        assert!(asm("SVTCA").is_err());
        assert!(asm("FOO").is_err());
        assert!(asm("ADD 1").is_err());
    }

    #[test]
    fn pushes_pick_the_shortest_form_or_the_one_named() {
        assert_eq!(asm("PUSH 1 2 3"), Ok(vec![0xB2, 1, 2, 3]));
        assert_eq!(asm("PUSH -1 300"), Ok(vec![0xB9, 0xFF, 0xFF, 0x01, 0x2C]));
        assert_eq!(
            asm("PUSH 0 1 2 3 4 5 6 7 8"),
            Ok(vec![0x40, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8])
        );
        assert_eq!(asm("NPUSHW 0x10"), Ok(vec![0x41, 1, 0, 0x10]));
        assert_eq!(asm("NPUSHB"), Ok(vec![0x40, 0]));
        assert!(asm("PUSHB 256").is_err());
        assert!(asm("PUSHW 32768").is_err());
        assert!(asm("PUSHB").is_err());
    }

    #[test]
    fn label_distances_are_words_relative_to_the_jump() {
        // PUSHW @end-@jump (3 bytes); @jump: JMPR; ADD; @end:
        let code = asm("PUSHW @end-@jump\n@jump:\nJMPR\nADD\n@end:\nSUB").unwrap();
        assert_eq!(code, vec![0xB8, 0, 2, 0x1C, 0x60, 0x61]);
        let back = asm("@top:\nADD\nPUSH @top-@jump\n@jump:\nJMPR").unwrap();
        assert_eq!(back, vec![0x60, 0xB8, 0xFF, 0xFC, 0x1C]);
        assert!(asm("PUSH @nowhere-@top\n@top:").is_err());
        assert!(asm("@a:\n@a:").is_err());
    }

    #[test]
    fn raw_bytes_pass_through() {
        assert_eq!(asm("BYTES 0x28 0x7B 255"), Ok(vec![0x28, 0x7B, 0xFF]));
        assert!(asm("BYTES 256").is_err());
    }
}
