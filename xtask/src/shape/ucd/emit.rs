//! Writing the generated Rust table files.
//!
//! The files are excluded from rustfmt (`#[rustfmt::skip] mod tables;`): long
//! arrays are laid out here as rows filled to 100 columns, which rustfmt would
//! otherwise turn into one value per line.

use super::Generated;
use super::pack::{Trie, width};

const WIDTH: usize = 100;

pub struct Writer {
    text: String,
    data_bytes: usize,
}

impl Writer {
    pub fn new(header: &str) -> Self {
        Self {
            text: header.to_owned(),
            data_bytes: 0,
        }
    }

    /// Append `text` verbatim.
    pub fn raw(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// `/// doc` lines, one per line of `doc`.
    fn doc(&mut self, doc: &str) {
        self.text.push('\n');
        for line in doc.lines() {
            self.text
                .push_str(if line.is_empty() { "///" } else { "/// " });
            self.text.push_str(line);
            self.text.push('\n');
        }
    }

    /// A `Trie` static.
    pub fn trie(&mut self, doc: &str, name: &str, trie: &Trie) {
        self.doc(doc);
        let types = [&trie.top, &trie.middle, &trie.leaves].map(|stage| type_name(stage));
        self.text.push_str(&format!(
            "pub(super) static {name}: Trie<{}, {}, {}> = Trie {{\n    \
             limit: 0x{:X},\n    default: {},\n    leaf_bits: {},\n    middle_bits: {},\n",
            types[0],
            types[1],
            types[2],
            trie.limit,
            trie.default,
            trie.leaf_bits,
            trie.middle_bits,
        ));
        for (field, stage) in [
            ("top", &trie.top),
            ("middle", &trie.middle),
            ("leaves", &trie.leaves),
        ] {
            self.text.push_str(&format!("    {field}: &["));
            self.items(8, stage.iter().map(u16::to_string));
            self.text.push_str("    ],\n");
        }
        self.text.push_str("};\n");
        self.data_bytes += trie.bytes();
    }

    /// A static array `name: [ty; N]` of `items`, each `item_bytes` large.
    pub fn array(&mut self, doc: &str, name: &str, ty: &str, items: &[String], item_bytes: usize) {
        self.doc(doc);
        self.text.push_str(&format!(
            "pub(super) static {name}: [{ty}; {}] = [",
            items.len()
        ));
        self.items(4, items.iter().cloned());
        self.text.push_str("];\n");
        self.data_bytes += items.len() * item_bytes;
    }

    /// Comma-terminated `items` in rows indented by `indent`, after an opening
    /// bracket that ends the current line.
    fn items(&mut self, indent: usize, items: impl Iterator<Item = String>) {
        let mut row = String::new();
        for item in items {
            if !row.is_empty() && indent + row.len() + item.len() + 2 > WIDTH {
                self.row(indent, &row);
                row.clear();
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(&item);
            row.push(',');
        }
        if !row.is_empty() {
            self.row(indent, &row);
        }
        if self.text.ends_with('[') {
            // An empty array stays on one line.
            return;
        }
        self.text.push_str(&" ".repeat(indent - 4));
    }

    fn row(&mut self, indent: usize, row: &str) {
        if self.text.ends_with('[') {
            self.text.push('\n');
        }
        self.text.push_str(&" ".repeat(indent));
        self.text.push_str(row);
        self.text.push('\n');
    }

    /// Count static data written by [`Writer::raw`] or referenced from an array.
    pub fn add_data(&mut self, bytes: usize) {
        self.data_bytes += bytes;
    }

    pub fn finish(self) -> Generated {
        Generated {
            text: self.text,
            data_bytes: self.data_bytes,
        }
    }
}

fn type_name(values: &[u16]) -> &'static str {
    if width(values) == 1 { "u8" } else { "u16" }
}

/// A code point as `0x0041`.
pub fn hex(cp: u32) -> String {
    format!("0x{cp:04X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lays_out_rows_and_counts_data() {
        let mut writer = Writer::new("// header\n");
        let items: Vec<String> = (0..40).map(|i| format!("{i:5}")).collect();
        writer.array("Doc.\n\nMore.", "XS", "u16", &items, 2);
        writer.array("Empty.", "NONE", "u8", &[], 1);
        let generated = writer.finish();
        assert_eq!(generated.data_bytes, 80);
        let lines: Vec<&str> = generated.text.lines().collect();
        assert_eq!(
            lines[..5],
            ["// header", "", "/// Doc.", "///", "/// More."]
        );
        assert_eq!(lines[5], "pub(super) static XS: [u16; 40] = [");
        assert!(lines.iter().all(|line| line.len() <= WIDTH));
        assert!(lines.contains(&"];"));
        assert!(lines.contains(&"pub(super) static NONE: [u8; 0] = [];"));
    }
}
