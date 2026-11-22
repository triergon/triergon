use std::ffi::OsString;

use triergon_core::{Budget, BudgetVec};
use triergon_subtitles::compat::dvd::VobSubEntry;
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
    let mut header = Vec::new();
    let mut tracks = Vec::new();
    let mut packets = Vec::new();
    let mut langidx = 0;
    let mut delay = 0_i64;
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
                tracks.push((index.unwrap_or(0), language));
            }
            Ok(Line::Delay(value)) => delay = value,
            Ok(Line::Timestamp {
                milliseconds,
                filepos,
            }) => {
                let Some(track) = tracks.len().checked_sub(1) else {
                    return records.refused();
                };
                let Some(pts) = milliseconds.checked_add(delay) else {
                    return records.refused();
                };
                packets.push((pts, filepos, track));
            }
            Ok(Line::Setting(line)) if tracks.is_empty() => {
                header.extend_from_slice(line);
                header.push(b'\n');
            }
            Ok(Line::Setting(_)) => {}
            Err(_) => return records.refused(),
        }
    }
    if tracks.is_empty() {
        return records.refused();
    }
    records.row(3, &[comments, blanks], &[])?;
    for (ordinal, &(id, language)) in tracks.iter().enumerate() {
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
        records.row(2, &[ordinal as u64, header.len() as u64], &header)?;
    }
    // An index entry names a source position. The demuxer only publishes a
    // packet when a program-stream unit can be read there.
    packets.sort_by_key(|&(pts, pos, track)| (track, pos, pts));
    let pool = Budget::new(16 * 1024 * 1024).map_err(|error| error.to_string())?;
    let mut output = BudgetVec::new(&pool);
    let mut admitted = Vec::new();
    for (i, &(pts, pos, track)) in packets.iter().enumerate() {
        let Some(data) = usize::try_from(pos).ok().and_then(|at| sub.get(at..)) else {
            continue;
        };
        let entry = VobSubEntry {
            milliseconds: pts,
            filepos: pos,
            end: packets
                .get(i + 1)
                .filter(|next| next.2 == track)
                .map(|next| next.1),
            substream: tracks[track].0,
        };
        if entry
            .read(data, &mut output)
            .map_err(|error| error.to_string())?
        {
            admitted.push((pts, pos, track));
        }
    }
    admitted.sort_by_key(|&(pts, pos, track)| (pts, track, pos));
    for (pts, pos, track) in admitted {
        records.row(1, &[track as u64, pts as u64, pos], &[])?;
    }
    records.row(7, &[], &[])
}
