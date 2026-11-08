//! Parsing the Unicode Character Database text files.

use std::ops::RangeInclusive;

/// The number of Unicode code points.
pub const CODE_POINTS: usize = 0x11_0000;

/// One line of UnicodeData.txt.
pub struct Record<'a> {
    pub code_point: u32,
    pub name: &'a str,
    pub general_category: &'a str,
    pub combining_class: u8,
    pub bidi_class: &'a str,
    /// The decomposition field: empty, a canonical mapping (`0041 0300`) or a
    /// tagged compatibility mapping (`<final> 0627`).
    pub decomposition: &'a str,
}

impl Record<'_> {
    /// The canonical decomposition mapping, if the record has one.
    pub fn canonical_decomposition(&self) -> Result<Option<Vec<u32>>, String> {
        if self.decomposition.is_empty() || self.decomposition.starts_with('<') {
            return Ok(None);
        }
        self.decomposition
            .split(' ')
            .map(code_point)
            .collect::<Result<_, _>>()
            .map(Some)
    }

    /// A compatibility decomposition `<tag> a b ...` as its tag and code points.
    pub fn tagged_decomposition(&self) -> Result<Option<(&str, Vec<u32>)>, String> {
        let Some(rest) = self.decomposition.strip_prefix('<') else {
            return Ok(None);
        };
        let (tag, mapping) = rest
            .split_once("> ")
            .ok_or_else(|| format!("U+{:04X}: malformed decomposition", self.code_point))?;
        let mapping = mapping
            .split(' ')
            .map(code_point)
            .collect::<Result<_, _>>()?;
        Ok(Some((tag, mapping)))
    }
}

/// Every line of UnicodeData.txt, in file order.
pub fn unicode_data(text: &str) -> Result<Vec<Record<'_>>, String> {
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(';').collect();
            if fields.len() != 15 {
                return Err(format!("UnicodeData.txt: malformed line {line}"));
            }
            Ok(Record {
                code_point: code_point(fields[0])?,
                name: fields[1],
                general_category: fields[2],
                combining_class: fields[3]
                    .parse()
                    .map_err(|_| format!("UnicodeData.txt: malformed line {line}"))?,
                bidi_class: fields[4],
                decomposition: fields[5],
            })
        })
        .collect()
}

/// Every code point the records describe, with `<..., First>` and `<..., Last>`
/// pairs expanded into the range between them, as the UCD defines them.
pub fn expanded<'r, 'a>(
    records: &'r [Record<'a>],
) -> Result<Vec<(RangeInclusive<u32>, &'r Record<'a>)>, String> {
    let mut ranges = Vec::new();
    let mut iter = records.iter();
    while let Some(record) = iter.next() {
        if record.name.ends_with(", First>") {
            let last = iter
                .next()
                .filter(|last| last.name.ends_with(", Last>"))
                .ok_or_else(|| format!("U+{:04X}: range without a end", record.code_point))?;
            ranges.push((record.code_point..=last.code_point, record));
        } else {
            ranges.push((record.code_point..=record.code_point, record));
        }
    }
    Ok(ranges)
}

/// The data lines of a semicolon-separated UCD file, without comments, each split
/// into trimmed fields.
pub fn fields(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines().filter_map(|line| {
        let data = line.split('#').next().unwrap_or("").trim();
        (!data.is_empty()).then(|| data.split(';').map(str::trim).collect())
    })
}

/// A field `XXXX` or `XXXX..YYYY`.
pub fn range(field: &str) -> Result<RangeInclusive<u32>, String> {
    let range = match field.split_once("..") {
        Some((first, last)) => code_point(first)?..=code_point(last)?,
        None => code_point(field)?..=code_point(field)?,
    };
    if range.is_empty() {
        return Err(format!("empty range {field}"));
    }
    Ok(range)
}

/// A hexadecimal code point.
pub fn code_point(field: &str) -> Result<u32, String> {
    u32::from_str_radix(field, 16)
        .ok()
        .filter(|&value| (value as usize) < CODE_POINTS)
        .ok_or_else(|| format!("malformed code point {field:?}"))
}

/// A property value for every code point, starting from `default`.
pub fn filled<T: Clone>(default: T) -> Vec<T> {
    vec![default; CODE_POINTS]
}

/// Set `values` over `range`.
pub fn set<T: Clone>(values: &mut [T], range: RangeInclusive<u32>, value: &T) {
    for slot in &mut values[*range.start() as usize..=*range.end() as usize] {
        slot.clone_from(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records_and_ranges() {
        let text = "0041;LATIN CAPITAL LETTER A;Lu;0;L;;;;;N;;;;0061;\n\
                    00C0;LATIN CAPITAL LETTER A WITH GRAVE;Lu;0;L;0041 0300;;;;N;;;;00E0;\n\
                    AC00;<Hangul Syllable, First>;Lo;0;L;;;;;N;;;;;\n\
                    D7A3;<Hangul Syllable, Last>;Lo;0;L;;;;;N;;;;;\n\
                    FE8E;ARABIC LETTER ALEF FINAL FORM;Lo;0;AL;<final> 0627;;;;N;;;;;";
        let records = unicode_data(text).unwrap();
        assert_eq!(
            records[1].canonical_decomposition().unwrap(),
            Some(vec![0x41, 0x300])
        );
        assert_eq!(records[4].canonical_decomposition().unwrap(), None);
        let (tag, mapping) = records[4].tagged_decomposition().unwrap().unwrap();
        assert_eq!((tag, mapping), ("final", vec![0x627]));
        let ranges: Vec<_> = expanded(&records)
            .unwrap()
            .into_iter()
            .map(|(range, _)| range)
            .collect();
        assert_eq!(
            ranges,
            [0x41..=0x41, 0xC0..=0xC0, 0xAC00..=0xD7A3, 0xFE8E..=0xFE8E]
        );
    }

    #[test]
    fn parses_fields_without_comments() {
        let lines: Vec<_> = fields("# header\n\n0600..0605 ; Arab # Cf\n0640;  Zyyy\n").collect();
        assert_eq!(lines, [vec!["0600..0605", "Arab"], vec!["0640", "Zyyy"]]);
        assert_eq!(range("0600..0605").unwrap(), 0x600..=0x605);
        assert!(range("0605..0600").is_err());
        assert!(code_point("110000").is_err());
    }
}
