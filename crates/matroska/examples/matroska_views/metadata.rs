use std::collections::BTreeMap;
use std::ffi::OsString;

use triergon_matroska::metadata::{Attachment, Chapter, SimpleTag, Tag};
use triergon_matroska::{Reader, Result};

use super::protocol::Operation;
use super::side_data::{Input, Rows};

const TARGETS: [u64; 4] = [0x63c5, 0x63c9, 0x63c4, 0x63c6];

fn text(rows: &mut Rows<'_, '_>, field: u64, value: Option<&str>) -> Result<()> {
    rows.optional(3, field, value.map(str::as_bytes))
}

fn attachment(rows: &mut Rows<'_, '_>, view: &Attachment, title: Option<&str>) -> Result<()> {
    let data = view.data()?;
    rows.record(2, &[0, data.len() as u64], data)?;
    for (field, value) in [
        (1, Some(view.filename()?)),
        (2, Some(view.media_type()?)),
        (3, title),
    ] {
        rows.optional(2, field, value.map(str::as_bytes))?;
    }
    Ok(())
}

fn chapters(rows: &mut Rows<'_, '_>, chapter: Chapter<'_>, depth: u64) -> Result<()> {
    let end = chapter.end_ns()?;
    rows.record(
        3,
        &[
            4,
            depth,
            chapter.uid()?,
            chapter.start_ns()?,
            u64::from(end.is_some()),
            end.unwrap_or_default(),
            u64::from(chapter.hidden()?),
            u64::from(chapter.enabled()?),
        ],
        &[],
    )?;
    for display in chapter.displays() {
        text(rows, 5, Some(display.title()?))?;
        for language in display.languages() {
            text(rows, 6, Some(language?))?;
        }
    }
    for child in chapter.children() {
        chapters(rows, child, depth + 1)?;
    }
    Ok(())
}

fn simple_tag(rows: &mut Rows<'_, '_>, tag: SimpleTag<'_>, depth: u64) -> Result<()> {
    rows.record(3, &[9, depth, u64::from(tag.is_default()?)], &[])?;
    text(rows, 10, Some(tag.name()?))?;
    text(rows, 11, Some(tag.language()?))?;
    text(rows, 12, tag.text()?)?;
    rows.optional(3, 13, tag.binary())?;
    for child in tag.children() {
        simple_tag(rows, child, depth + 1)?;
    }
    Ok(())
}

fn source(reader: &Reader, rows: &mut Rows<'_, '_>) -> Result<()> {
    for doc in reader.catalogue() {
        if let Some(info) = doc.info() {
            text(rows, 0, info.title()?)?;
            text(rows, 1, info.muxing_app()?)?;
            text(rows, 2, info.writing_app()?)?;
        }
    }
    for doc in reader.catalogue() {
        for edition in doc.editions() {
            let uid = edition.uid()?;
            rows.record(
                3,
                &[
                    3,
                    u64::from(uid.is_some()),
                    uid.unwrap_or_default(),
                    u64::from(edition.is_default()?),
                    u64::from(edition.ordered()?),
                ],
                &[],
            )?;
            for chapter in edition.chapters() {
                chapters(rows, chapter, 0)?;
            }
        }
    }
    for doc in reader.catalogue() {
        for tag in doc.tags() {
            let target = tag.targets();
            rows.record(3, &[7, u64::from(target.is_some())], &[])?;
            if let Some(target) = target {
                rows.record(3, &[8, target.level()?], &[])?;
                text(rows, 14, target.type_name()?)?;
                for kind in TARGETS {
                    for uid in target.uids(kind) {
                        rows.record(3, &[15, kind, uid?], &[])?;
                    }
                }
            }
            for value in tag.values() {
                simple_tag(rows, value, 0)?;
            }
        }
    }
    for view in reader.attachments() {
        rows.record(2, &[4, view.uid()?], &[])?;
        attachment(rows, &view, view.description()?)?;
    }
    Ok(())
}

type Dictionary<'a> = BTreeMap<String, (String, &'a str)>;

fn set<'a>(dict: &mut Dictionary<'a>, key: &str, value: &'a str) {
    dict.insert(key.to_ascii_lowercase(), (key.to_owned(), value));
}

fn tag_values<'a>(dict: &mut Dictionary<'a>, value: SimpleTag<'a>, parent: &str) -> Result<()> {
    let name = value.name()?;
    let key = if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    };
    if let Some(data) = value.text()? {
        let language = value.language()?;
        if language == "und" || language.is_empty() || value.is_default()? {
            set(dict, &key, data);
        }
        if language != "und" && !language.is_empty() {
            set(dict, &format!("{key}-{language}"), data);
        }
    }
    for child in value.children() {
        tag_values(dict, child, &key)?;
    }
    Ok(())
}

fn apply<'a>(dict: &mut Dictionary<'a>, tag: Tag<'a>, prefix: &str) -> Result<()> {
    for value in tag.values() {
        tag_values(dict, value, prefix)?;
    }
    Ok(())
}

fn target(tag: Tag<'_>) -> Result<Option<(u64, u64)>> {
    if let Some(target) = tag.targets() {
        for kind in [0x63c6, 0x63c4, 0x63c5] {
            if let Some(uid) = target
                .uids(kind)
                .last()
                .transpose()?
                .filter(|&uid| uid != 0)
            {
                return Ok(Some((kind, uid)));
            }
        }
    }
    Ok(None)
}

fn dictionary(
    rows: &mut Rows<'_, '_>,
    scope: u64,
    index: u64,
    dict: &Dictionary<'_>,
) -> Result<()> {
    rows.record(3, &[scope, index], &[])?;
    for (key, value) in dict.values() {
        text(rows, 16, Some(key))?;
        text(rows, 17, Some(value))?;
    }
    Ok(())
}

/// Every attachment with the title RFC 9559 section 24.1 presents: a TITLE tag
/// that targets the attachment takes precedence over its FileDescription.
fn presented_attachments(reader: &Reader, rows: &mut Rows<'_, '_>) -> Result<()> {
    let attachments: Vec<_> = reader.attachments().collect();
    for view in &attachments {
        let mut dict = Dictionary::new();
        if let Some(description) = view.description()? {
            set(&mut dict, "title", description);
        }
        let uid = view.uid()?;
        for doc in reader.catalogue() {
            for tag in doc.tags() {
                if target(tag)? == Some((0x63c6, uid)) {
                    apply(&mut dict, tag, "")?;
                }
            }
        }
        attachment(rows, view, dict.get("title").map(|&(_, title)| title))?;
    }
    Ok(())
}

fn published(reader: &Reader, rows: &mut Rows<'_, '_>) -> Result<()> {
    let mut global = Dictionary::new();
    let mut streams = Vec::new();
    let mut chapters = Vec::new();
    let attachments: Vec<_> = reader.attachments().collect();
    for track in reader.tracks() {
        let mut dict = Dictionary::new();
        if !track.name.is_empty() {
            set(&mut dict, "title", track.name.as_str());
        }
        if track.language.as_ref() != "und" {
            set(&mut dict, "language", track.language.as_ref());
        }
        streams.push((track.uid, 0x63c5, dict));
    }
    for view in &attachments {
        if view.data()?.is_empty() {
            continue;
        }
        let mut dict = Dictionary::new();
        set(&mut dict, "filename", view.filename()?);
        set(&mut dict, "mimetype", view.media_type()?);
        if let Some(description) = view.description()? {
            set(&mut dict, "title", description);
        }
        streams.push((view.uid()?, 0x63c6, dict));
    }
    for doc in reader.catalogue() {
        if let Some(info) = doc.info() {
            if let Some(value) = info.title()? {
                set(&mut global, "title", value);
            }
            if let Some(value) = info.muxing_app()? {
                set(&mut global, "encoder", value);
            }
        }
        for edition in doc.editions() {
            for chapter in edition.chapters() {
                let start = chapter.start_ns()?;
                if chapters.last().is_some_and(|&(_, last, _, _)| start < last) {
                    continue;
                }
                let mut dict = Dictionary::new();
                for display in chapter.displays() {
                    set(&mut dict, "title", display.title()?);
                }
                chapters.push((
                    chapter.uid()?,
                    start,
                    chapter.end_ns()?.unwrap_or(i64::MIN as u64),
                    dict,
                ));
            }
        }
    }
    for doc in reader.catalogue() {
        for tag in doc.tags() {
            match target(tag)? {
                Some((0x63c4, uid)) => {
                    for (id, _, _, dict) in &mut chapters {
                        if *id == uid {
                            apply(dict, tag, "")?;
                        }
                    }
                }
                Some((kind, uid)) => {
                    for (id, stream_kind, dict) in &mut streams {
                        if (*id, *stream_kind) == (uid, kind) {
                            apply(dict, tag, "")?;
                        }
                    }
                }
                None => {
                    let prefix = tag
                        .targets()
                        .map(triergon_matroska::metadata::Targets::type_name)
                        .transpose()?
                        .flatten()
                        .unwrap_or_default();
                    apply(&mut global, tag, prefix)?;
                }
            }
        }
    }
    global.remove("creation_time");
    dictionary(rows, 0, 0, &global)?;
    for (index, (_, _, dict)) in streams.iter().enumerate() {
        dictionary(rows, 1, index as u64, dict)?;
    }
    for (index, (uid, start, end, dict)) in chapters.iter().enumerate() {
        rows.record(3, &[2, *uid, *start, *end, 1, 1_000_000_000], &[])?;
        dictionary(rows, 3, index as u64, dict)?;
    }
    Ok(())
}

pub(super) fn operation<'a>(
    prefix: &str,
    bytes: &'a [u8],
    args: &[OsString],
) -> std::result::Result<Operation<'a>, String> {
    let (name, read): (_, fn(&Reader, &mut Rows<'_, '_>) -> Result<()>) = match (prefix, args) {
        ("mkv-metadata-", []) => ("mkv-metadata-fields", published),
        ("mkv-metadata-", [arg]) if arg == "source" => ("mkv-metadata-oracle", source),
        ("mkv-attachments-", []) => ("mkv-attachments", presented_attachments),
        _ => return Err(format!("invalid {prefix} arguments")),
    };
    Ok(Operation {
        name,
        run: Box::new(move |work, trace| {
            let mut rows = Rows { work, trace };
            let result = Input::new(bytes, false).and_then(|mut input| {
                input.open()?;
                read(&input.reader, &mut rows)
            });
            rows.finish(result)
        }),
    })
}
