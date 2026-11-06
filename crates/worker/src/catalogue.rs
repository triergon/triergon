//! Independent source-fact serialization. No serialization work enters timing.
use std::{fmt::Write as _, io::Read, sync::atomic::AtomicBool};
use triergon::{metadata::Document, typed::BlockingReader as Demuxer};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes},
    hex, quoted,
};

fn unsigned(id: u64) -> bool {
    matches!(
        id,
        0x2ad7b1
            | 0x46ae
            | 0x73c4
            | 0x45bc
            | 0x45bd
            | 0x45db
            | 0x45dd
            | 0x91
            | 0x92
            | 0x98
            | 0x4598
            | 0x6ebc
            | 0x63c3
            | 0x89
            | 0x6955
            | 0x6922
            | 0x68ca
            | 0x63c5
            | 0x63c9
            | 0x63c4
            | 0x63c6
            | 0x4484
    )
}
fn textual(id: u64) -> bool {
    matches!(
        id,
        0x7ba9
            | 0x4d80
            | 0x5741
            | 0x467e
            | 0x466e
            | 0x5654
            | 0x85
            | 0x45a3
            | 0x4487
            | 0x4660
            | 0x437c
            | 0x437d
            | 0x437e
            | 0x63ca
            | 0x447a
            | 0x447b
    )
}
fn document(out: &mut String, d: &Document) -> Result<(), Box<dyn std::error::Error>> {
    write!(
        out,
        "{{\"kind\":{},\"source_offset\":{},\"raw\":{},\"nodes\":[",
        d.kind(),
        d.source_offset().map_or("null".into(), |v| v.to_string()),
        quoted(&hex(d.raw()))
    )?;
    for (i, e) in d.elements().enumerate() {
        if i != 0 {
            out.push(',');
        }
        let range = e.range();
        write!(
            out,
            "{{\"id\":{},\"range\":[{},{}],\"parent\":{},\"next\":{},\"uint\":{},\"text\":{},\"signed\":{},\"float_bits\":{}}}",
            e.id(),
            range.start,
            range.end,
            e.parent()
                .map_or("null".into(), |v| v.ordinal().to_string()),
            e.subtree_end(),
            if unsigned(e.id()) {
                e.uint()?.to_string()
            } else {
                "null".into()
            },
            if textual(e.id()) {
                quoted(e.text()?)
            } else {
                "null".into()
            },
            if e.id() == 0x4461 {
                e.sint()?.to_string()
            } else {
                "null".into()
            },
            if e.id() == 0x4489 {
                quoted(&format!("{:016x}", e.float()?.to_bits()))
            } else {
                "null".into()
            }
        )?;
    }
    out.push_str("]}");
    Ok(())
}
pub fn source<R: Read>(mut d: Demuxer<R>) -> Result<(), Box<dyn std::error::Error>> {
    while d.next_packet(&AtomicBool::new(false))?.is_some() {}
    let mut out = String::from("{\"schema\":1,\"documents\":[");
    let mut documents: Vec<_> = d.metadata_documents().iter().collect();
    documents.sort_by_key(|d| d.source_offset());
    for (i, d) in documents.into_iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        document(&mut out, d)?;
    }
    out.push_str("],\"tracks\":[");
    for (i, t) in d.tracks().iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        let optional = |v: Option<bool>| v.map_or("null".into(), |v| v.to_string());
        write!(
            out,
            "{{\"raw_track\":{},\"number\":{},\"uid\":{},\"name\":{},\"legacy_language\":{},\"bcp47\":{},\"effective_language\":{},\"flags\":[{},{},{},{},{},{},{},{},{}]}}",
            quoted(&hex(&t.source_metadata)),
            t.number,
            t.uid,
            quoted(&t.name),
            quoted(&t.language),
            t.language_bcp47.as_deref().map_or("null".into(), quoted),
            quoted(t.effective_language()),
            t.enabled,
            t.default,
            t.forced,
            t.lacing,
            optional(t.hearing_impaired),
            optional(t.visual_impaired),
            optional(t.text_descriptions),
            optional(t.original),
            optional(t.commentary)
        )?;
    }
    out.push_str("]}");
    println!("{out}");
    Ok(())
}

use std::borrow::Cow;
use triergon::metadata::{self, Chapter, Element, SimpleTag};
type Error = Box<dyn std::error::Error>;
type Pair<'a> = (Cow<'a, str>, Cow<'a, str>);

/// Dictionaries commonly contain just one borrowed title. Keep that case inline.
#[derive(Default)]
pub enum Dictionary<'a> {
    #[default]
    Empty,
    One(&'a str, &'a str),
    Rows(Vec<Pair<'a>>),
}
impl<'a> Dictionary<'a> {
    fn set(
        &mut self,
        key: impl Into<Cow<'a, str>>,
        value: impl Into<Cow<'a, str>>,
    ) -> Result<(), Error> {
        let pair = (key.into(), value.into());
        let mut rows = match std::mem::take(self) {
            Self::Empty => {
                if let (Cow::Borrowed(k), Cow::Borrowed(v)) = &pair {
                    *self = Self::One(k, v);
                    return Ok(());
                }
                Vec::new()
            }
            Self::One(k, v) => {
                if k.eq_ignore_ascii_case(&pair.0)
                    && let (Cow::Borrowed(n), Cow::Borrowed(w)) = &pair
                {
                    *self = Self::One(n, w);
                    return Ok(());
                }
                let mut rows = Vec::new();
                rows.try_reserve_exact(2)?;
                rows.push((Cow::Borrowed(k), Cow::Borrowed(v)));
                rows
            }
            Self::Rows(rows) => rows,
        };
        if let Some(old) = rows
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&pair.0))
        {
            *old = pair;
        } else {
            if rows.len() == rows.capacity() {
                let target = rows.len().saturating_mul(2).max(1);
                rows.try_reserve_exact(target - rows.len())?;
            }
            rows.push(pair);
        }
        *self = Self::Rows(rows);
        Ok(())
    }
    fn output(&self, mut out: Option<&mut String>, digest: &mut u64) -> Result<(), Error> {
        match self {
            Self::Empty => {
                fold(digest, 0);
                if let Some(o) = out {
                    o.push_str("[]");
                }
            }
            Self::One(k, v) => {
                fold(digest, 1);
                fold_bytes(digest, k.as_bytes());
                fold_bytes(digest, v.as_bytes());
                if let Some(o) = out {
                    write!(o, "[[{},{}]]", quoted(k), quoted(v))?;
                }
            }
            Self::Rows(rows) => {
                let mut order: Vec<_> = (0..rows.len()).collect();
                order.sort_unstable_by(|a, b| rows[*a].0.cmp(&rows[*b].0));
                fold(digest, rows.len() as u64);
                if let Some(o) = out.as_deref_mut() {
                    o.push('[');
                }
                for (i, n) in order.into_iter().enumerate() {
                    let (k, v) = &rows[n];
                    fold_bytes(digest, k.as_bytes());
                    fold_bytes(digest, v.as_bytes());
                    if let Some(o) = out.as_deref_mut() {
                        if i != 0 {
                            o.push(',');
                        }
                        write!(o, "[{},{}]", quoted(k), quoted(v))?;
                    }
                }
                if let Some(o) = out {
                    o.push(']');
                }
            }
        }
        Ok(())
    }
}
struct ChapterView<'a> {
    source: Chapter<'a>,
    tags: Dictionary<'a>,
}
struct AttachmentView<'a> {
    uid: u64,
    tags: Dictionary<'a>,
    data: &'a [u8],
}
pub struct CatalogueView<'a> {
    global: Dictionary<'a>,
    tracks: Vec<(u64, u32, Dictionary<'a>)>,
    chapters: Vec<ChapterView<'a>>,
    attachments: Vec<AttachmentView<'a>>,
}
fn utc_date(ns: i64) -> String {
    let micros = i128::from(ns / 1000) + 978_307_200_000_000_i128;
    let seconds = micros.div_euclid(1_000_000);
    let sub = micros.rem_euclid(1_000_000);
    let mut days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let leap = |y: i128| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mut year = 1970;
    while days < 0 {
        year -= 1;
        days += if leap(year) { 366 } else { 365 };
    }
    loop {
        let n = if leap(year) { 366 } else { 365 };
        if days < n {
            break;
        }
        days -= n;
        year += 1;
    }
    let mut month = 1;
    for n in [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ] {
        if days < n {
            break;
        }
        days -= n;
        month += 1;
    }
    format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}.{sub:06}Z",
        days + 1,
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}
fn flatten_tag<'a>(
    tag: SimpleTag<'a>,
    prefix: &str,
    out: &mut Dictionary<'a>,
) -> Result<(), Error> {
    let name = tag.name()?;
    let key = if prefix.is_empty() {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(format!("{prefix}/{name}"))
    };
    if let Some(value) = tag.text()? {
        let language = tag.0.string(0x447a)?.unwrap_or("und");
        let output_key = if language != "und" && !language.is_empty() {
            Cow::Owned(format!("{key}-{language}"))
        } else {
            key.clone()
        };
        out.set(output_key, value)?;
    }
    for child in tag.children() {
        flatten_tag(child, &key, out)?;
    }
    Ok(())
}
fn tag_values<'a>(tag: metadata::Tag<'a>, out: &mut Dictionary<'a>) -> Result<(), Error> {
    for value in tag.values() {
        flatten_tag(value, "", out)?;
    }
    Ok(())
}
fn target_id(target: Option<Element<'_>>, kind: u64) -> Result<Option<u64>, Error> {
    Ok(target
        .and_then(|e| e.children().filter(|v| v.id() == kind).last())
        .map(Element::uint)
        .transpose()?
        .filter(|v| *v != 0))
}
impl<'a> CatalogueView<'a> {
    pub fn new<R: Read>(d: &'a Demuxer<R>) -> Result<Self, Error> {
        let mut this = Self {
            global: Dictionary::default(),
            tracks: Vec::new(),
            chapters: Vec::new(),
            attachments: Vec::new(),
        };
        for t in d.tracks() {
            let mut flags = u32::from(t.default) | if t.forced { 64 } else { 0 };
            for (value, bit) in [
                (t.original, 4),
                (t.commentary, 8),
                (t.hearing_impaired, 128),
                (t.visual_impaired, 256),
            ] {
                if value == Some(true) {
                    flags |= bit;
                }
            }
            if t.original == Some(false) {
                flags |= 2;
            }
            let mut tags = Dictionary::default();
            // The pinned native dictionary omits the exact undefined code. The
            // source track retains it, and later explicit tags can override it.
            if t.language.as_ref() != "und" {
                tags.set("language", t.language.as_ref())?;
            }
            if !t.name.is_empty() {
                tags.set("title", t.name.as_str())?;
            }
            this.tracks.push((t.uid, flags, tags));
        }
        let mut last_start = i64::MIN;
        for doc in d.metadata_documents() {
            if let Some(info) = doc.info() {
                if let Some(v) = info.title()? {
                    this.global.set("title", v)?;
                }
                if let Some(v) = info.muxing_app()? {
                    this.global.set("encoder", v)?;
                }
                if let Some(v) = info.date_utc_ns()? {
                    this.global.set("creation_time", utc_date(v))?;
                }
            }
            for edition in doc.editions() {
                for chapter in edition.chapters() {
                    let start = i64::try_from(chapter.start_ns()?)?;
                    if start < last_start {
                        continue;
                    }
                    last_start = start;
                    let mut tags = Dictionary::default();
                    for display in chapter.displays() {
                        tags.set("title", display.title()?)?;
                    }
                    this.chapters.push(ChapterView {
                        source: chapter,
                        tags,
                    });
                }
            }
            if doc.kind() == metadata::ATTACHMENTS {
                for e in doc.root().children().filter(|e| e.id() == 0x61a7) {
                    let data = e.field(0x465c).ok_or("missing attachment data")?.raw();
                    if data.is_empty() {
                        continue;
                    }
                    let mut tags = Dictionary::default();
                    tags.set(
                        "filename",
                        e.string(0x466e)?.ok_or("missing attachment filename")?,
                    )?;
                    tags.set(
                        "mimetype",
                        e.string(0x4660)?.ok_or("missing attachment MIME")?,
                    )?;
                    if let Some(v) = e.string(0x467e)? {
                        tags.set("title", v)?;
                    }
                    this.attachments.push(AttachmentView {
                        uid: e.number(0x46ae)?.ok_or("missing attachment UID")?,
                        tags,
                        data,
                    });
                }
            }
        }
        for doc in d.metadata_documents() {
            for tag in doc.tags() {
                let target = tag.targets().map(|v| v.0);
                let track = target_id(target, 0x63c5)?;
                let chapter = target_id(target, 0x63c4)?;
                let attachment = target_id(target, 0x63c6)?;
                if let Some(uid) = attachment {
                    for a in &mut this.attachments {
                        if a.uid == uid {
                            tag_values(tag, &mut a.tags)?;
                        }
                    }
                } else if let Some(uid) = chapter {
                    for c in &mut this.chapters {
                        if c.source.uid()? == uid {
                            tag_values(tag, &mut c.tags)?;
                        }
                    }
                } else if let Some(uid) = track {
                    for (id, _, dict) in &mut this.tracks {
                        if *id == uid {
                            tag_values(tag, dict)?;
                        }
                    }
                } else {
                    tag_values(tag, &mut this.global)?;
                }
            }
        }
        Ok(this)
    }
    pub fn output(&self, mut out: Option<&mut String>, digest: &mut u64) -> Result<(), Error> {
        if let Some(o) = out.as_deref_mut() {
            o.push_str(",\"catalogue\":{\"global\":");
        }
        self.global.output(out.as_deref_mut(), digest)?;
        fold(digest, self.tracks.len() as u64);
        if let Some(o) = out.as_deref_mut() {
            o.push_str(",\"tracks\":[");
        }
        for (i, (_, flags, tags)) in self.tracks.iter().enumerate() {
            fold(digest, u64::from(*flags));
            if let Some(o) = out.as_deref_mut() {
                if i != 0 {
                    o.push(',');
                }
                write!(o, "{{\"disposition\":{flags},\"tags\":")?;
            }
            tags.output(out.as_deref_mut(), digest)?;
            if let Some(o) = out.as_deref_mut() {
                o.push('}');
            }
        }
        fold(digest, self.chapters.len() as u64);
        if let Some(o) = out.as_deref_mut() {
            o.push_str("],\"chapters\":[");
        }
        for (i, c) in self.chapters.iter().enumerate() {
            let id = c.source.uid()?;
            let start = i64::try_from(c.source.start_ns()?)?;
            let end = c
                .source
                .end_ns()?
                .map(i64::try_from)
                .transpose()?
                .unwrap_or(i64::MIN + 1);
            for v in [id, start as u64, end as u64] {
                fold(digest, v);
            }
            if let Some(o) = out.as_deref_mut() {
                if i != 0 {
                    o.push(',');
                }
                write!(
                    o,
                    "{{\"id\":{},\"start_ns\":{start},\"end_ns\":{end},\"tags\":",
                    id as i64
                )?;
            }
            c.tags.output(out.as_deref_mut(), digest)?;
            if let Some(o) = out.as_deref_mut() {
                o.push('}');
            }
        }
        fold(digest, self.attachments.len() as u64);
        if let Some(o) = out.as_deref_mut() {
            o.push_str("],\"attachments\":[");
        }
        for (i, a) in self.attachments.iter().enumerate() {
            if let Some(o) = out.as_deref_mut() {
                if i != 0 {
                    o.push(',');
                }
                o.push_str("{\"tags\":");
            }
            a.tags.output(out.as_deref_mut(), digest)?;
            fold(digest, a.data.len() as u64);
            fold_bytes(digest, a.data);
            if let Some(o) = out.as_deref_mut() {
                write!(o, ",\"data\":{}}}", quoted(&hex(a.data)))?;
            }
        }
        if let Some(o) = out {
            o.push_str("]}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use triergon::Limits;

    fn check_language(input: &[u8], legacy: &str, bcp47: Option<&str>, expected: &str) {
        let mut demux = Demuxer::open_seekable(Cursor::new(input), Limits::default()).unwrap();
        while demux
            .next_packet(&AtomicBool::new(false))
            .unwrap()
            .is_some()
        {}
        let track = &demux.tracks()[0];
        assert_eq!(track.language.as_ref(), legacy);
        assert_eq!(track.language_bcp47.as_deref(), bcp47);
        assert_eq!(track.effective_language(), bcp47.unwrap_or(legacy));
        let view = CatalogueView::new(&demux).unwrap();
        let mut actual = String::new();
        view.tracks[0].2.output(Some(&mut actual), &mut 0).unwrap();
        assert_eq!(actual, expected);
    }

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!(
                "../tests/fixtures/catalogue-language/",
                $name,
                ".mkv"
            ))
        };
    }

    #[test]
    fn native_undefined_language_omission_preserves_source_language_and_bcp47() {
        for input in [
            fixture!("und").as_slice(),
            fixture!("nul-padded").as_slice(),
        ] {
            check_language(input, "und", None, r#"[["title","Language probe"]]"#);
        }
        check_language(
            fixture!("legacy-und-ietf"),
            "und",
            Some("en-GB"),
            r#"[["title","Language probe"]]"#,
        );
    }

    #[test]
    fn native_defined_language_tags_keep_exact_case_and_codes() {
        for (input, language) in [
            (fixture!("eng").as_slice(), "eng"),
            (fixture!("upper").as_slice(), "UND"),
            (fixture!("mixed").as_slice(), "Und"),
            (fixture!("unknown").as_slice(), "xyz"),
            (fixture!("no-linguistic").as_slice(), "zxx"),
            (fixture!("multi").as_slice(), "mul"),
        ] {
            check_language(
                input,
                language,
                None,
                &format!(r#"[["language","{language}"],["title","Language probe"]]"#),
            );
        }
    }

    #[test]
    fn native_language_defaults_do_not_replace_source_bcp47() {
        for input in [fixture!("absent").as_slice(), fixture!("empty").as_slice()] {
            check_language(
                input,
                "eng",
                None,
                r#"[["language","eng"],["title","Language probe"]]"#,
            );
        }
        for (input, ietf) in [
            (fixture!("ietf-only").as_slice(), "en-GB"),
            (fixture!("legacy-eng-ietf-und").as_slice(), "und"),
        ] {
            check_language(
                input,
                "eng",
                Some(ietf),
                r#"[["language","eng"],["title","Language probe"]]"#,
            );
        }
    }

    #[test]
    fn explicit_language_tags_override_the_native_track_dictionary() {
        check_language(
            fixture!("tag-overrides-und"),
            "und",
            None,
            r#"[["language","French"],["title","Language probe"]]"#,
        );
        check_language(
            fixture!("tag-und-overrides-eng"),
            "eng",
            None,
            r#"[["language","und"],["title","Language probe"]]"#,
        );
    }
}
