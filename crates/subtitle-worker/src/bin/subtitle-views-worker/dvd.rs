use std::ffi::OsString;

use std::collections::BTreeMap;
use triergon_core::{Budget, BudgetVec};
use triergon_subtitles::compat::dvd::VobSubIndex;
use triergon_subtitles::dvd::vobsub::{Line, Lines};

use super::protocol::Operation;
use super::text::Records;

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [_index_path, sub_path] = arguments else {
        return Err(format!("usage: {prefix} INDEX_PATH SUB_PATH"));
    };
    let sub = std::fs::read(sub_path).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "vobsub-index",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            index_view(&mut records, input, &sub)
        }),
    })
}

fn index_view(records: &mut Records<'_, '_>, input: &[u8], sub: &[u8]) -> Result<(), String> {
    let pool = Budget::new(16 * 1024 * 1024).map_err(|error| error.to_string())?;
    let Ok(index) = VobSubIndex::parse(&pool, input) else {
        return records.refused();
    };
    if index.streams().is_empty() {
        return records.refused();
    }
    let mut languages = BTreeMap::new();
    let mut langidx = 0;
    let (mut comments, mut blanks) = (0, 0);
    for (_, line) in Lines::new(input) {
        match line {
            Ok(Line::Blank) => blanks += 1,
            Ok(Line::Comment) => comments += 1,
            Ok(Line::LanguageIndex(index)) => {
                if let Some(value) = std::str::from_utf8(index).ok().and_then(|s| s.parse().ok()) {
                    langidx = value;
                }
            }
            Ok(Line::Track { language, index }) => {
                languages.entry(index.unwrap_or(0)).or_insert(language);
            }
            Ok(_) => {}
            Err(_) => return records.refused(),
        }
    }
    records.row(3, &[comments, blanks], &[])?;
    let header = index.configuration();
    for (ordinal, &id) in index.streams().iter().enumerate() {
        let language = languages.get(&id).ok_or("missing indexed track language")?;
        records.row(
            2,
            &[
                ordinal as u64,
                id.into(),
                (ordinal == langidx).into(),
                language.len() as u64,
            ],
            language,
        )?;
        records.row(2, &[ordinal as u64, header.len() as u64], header)?;
    }
    let mut output = BudgetVec::new(&pool);
    let mut queues: Vec<_> = (0..index.streams().len())
        .map(|track| index.entries(track).peekable())
        .collect();
    // Present the earliest queue head while preserving file order within a track.
    while let Some((track, _)) = queues
        .iter_mut()
        .enumerate()
        .filter_map(|(track, entries)| Some((track, entries.peek()?.milliseconds)))
        .min_by_key(|&(_, pts)| pts)
    {
        let entry = queues[track].next().ok_or("missing index entry")?;
        let pos = entry.filepos;
        let Some(data) = usize::try_from(pos).ok().and_then(|at| sub.get(at..)) else {
            continue;
        };
        if entry
            .read(data, &mut output)
            .map_err(|error| error.to_string())?
        {
            records.row(1, &[track as u64, entry.milliseconds as u64, pos], &[])?;
        }
    }
    records.row(7, &[], &[])
}
