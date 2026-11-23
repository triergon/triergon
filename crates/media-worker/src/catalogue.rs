//! The segment metadata as source facts (`catalogue-source`) and as the reference's
//! catalogue of global, track, chapter and attachment dictionaries (`catalogue-`
//! views). Only the folds of the catalogue run inside timed work.
use crate::Error;
use std::{borrow::Cow, fmt::Write as _, io::Read, sync::atomic::AtomicBool};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes},
    hex, quoted,
};
use triergon_matroska::BlockingReader as Demuxer;
use triergon_matroska::metadata::{self, Chapter, Document, Element, SimpleTag};

// Element IDs (RFC 9559) the catalogue reads.
const DATE_UTC: u64 = 0x4461;
const DURATION: u64 = 0x4489;
const TAG_LANGUAGE: u64 = 0x447a;
const ATTACHED_FILE: u64 = 0x61a7;
const FILE_DATA: u64 = 0x465c;
const FILE_NAME: u64 = 0x466e;
const FILE_MEDIA_TYPE: u64 = 0x4660;
const FILE_DESCRIPTION: u64 = 0x467e;
const FILE_UID: u64 = 0x46ae;
const TAG_TRACK_UID: u64 = 0x63c5;
const TAG_CHAPTER_UID: u64 = 0x63c4;
const TAG_ATTACHMENT_UID: u64 = 0x63c6;

// The reference's stream disposition bits.
const DEFAULT: u32 = 1;
const DUB: u32 = 2;
const ORIGINAL: u32 = 4;
const COMMENT: u32 = 8;
const FORCED: u32 = 64;
const HEARING_IMPAIRED: u32 = 128;
const VISUAL_IMPAIRED: u32 = 256;

/// One document's kind, raw bytes and every node with its decoded value.
fn document(out: &mut String, d: &Document) -> Result<(), Error> {
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
            if e.is_unsigned() {
                e.uint()?.to_string()
            } else {
                "null".into()
            },
            if e.is_text() {
                quoted(e.text()?)
            } else {
                "null".into()
            },
            if e.id() == DATE_UTC {
                e.sint()?.to_string()
            } else {
                "null".into()
            },
            if e.id() == DURATION {
                quoted(&format!("{:016x}", e.float()?.to_bits()))
            } else {
                "null".into()
            }
        )?;
    }
    out.push_str("]}");
    Ok(())
}
/// `catalogue-source`: after reading every packet, the documents in source order and
/// every track's source fields and flags.
pub fn source<R: Read>(mut d: Demuxer<R>) -> Result<(), Error> {
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

type Pair<'a> = (Cow<'a, str>, Cow<'a, str>);

/// A tag dictionary: keys match without ASCII case and a later value replaces an
/// earlier one. Dictionaries commonly hold just one borrowed title, kept inline.
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
    /// Folds the entries in key order into `digest` and, with `out`, writes them as
    /// JSON pairs.
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
/// The catalogue the reference builds from the tracks and segment metadata.
pub struct CatalogueView<'a> {
    global: Dictionary<'a>,
    /// Each track's TrackUID, disposition bits and tags. A tuple: a named struct here
    /// changes the frame of the measured `catalogue-bench` loop.
    tracks: Vec<(u64, u32, Dictionary<'a>)>,
    chapters: Vec<ChapterView<'a>>,
    attachments: Vec<AttachmentView<'a>>,
}
/// The `creation_time` text of a Segment's DateUTC: the signed nanoseconds since
/// 2001-01-01T00:00:00Z, truncated toward zero to microseconds.
fn utc_date(ns: i64) -> String {
    /// Seconds from the Unix epoch to the Matroska epoch.
    const MATROSKA_EPOCH: i64 = 978_307_200;
    // Every i64 of nanoseconds lies within 1708 to 2293, so no step overflows.
    let micros = ns / 1000 + MATROSKA_EPOCH * 1_000_000;
    let seconds = micros.div_euclid(1_000_000);
    let sub = micros.rem_euclid(1_000_000);
    let time = seconds.rem_euclid(86_400);
    let days = i32::try_from(seconds.div_euclid(86_400)).expect("days of an i64 DateUTC");
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{sub:06}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

/// The proleptic Gregorian year, month and day of a day count from 1970-01-01, by
/// whole 400-year eras of 146,097 days, each counted from a March 1st.
fn civil_from_days(days: i32) -> (i32, i32, i32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    (era * 400 + year_of_era + i32::from(month <= 2), month, day)
}
/// A SimpleTag and its nested tags as `PARENT/NAME` keys, with `-LANGUAGE` for a
/// TagLanguage other than `und`.
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
        let language = tag.0.string(TAG_LANGUAGE)?.unwrap_or("und");
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
/// The last nonzero UID of element `kind` among a tag's Targets.
fn target_id(target: Option<Element<'_>>, kind: u64) -> Result<Option<u64>, Error> {
    Ok(target
        .and_then(|e| e.children().filter(|v| v.id() == kind).last())
        .map(Element::uint)
        .transpose()?
        .filter(|v| *v != 0))
}
impl<'a> CatalogueView<'a> {
    /// The catalogue of a reader that has read its segment metadata.
    pub fn new<R: Read>(d: &'a Demuxer<R>) -> Result<Self, Error> {
        let mut this = Self {
            global: Dictionary::default(),
            tracks: Vec::new(),
            chapters: Vec::new(),
            attachments: Vec::new(),
        };
        for t in d.tracks() {
            let mut disposition =
                if t.default { DEFAULT } else { 0 } | if t.forced { FORCED } else { 0 };
            for (value, bit) in [
                (t.original, ORIGINAL),
                (t.commentary, COMMENT),
                (t.hearing_impaired, HEARING_IMPAIRED),
                (t.visual_impaired, VISUAL_IMPAIRED),
            ] {
                if value == Some(true) {
                    disposition |= bit;
                }
            }
            if t.original == Some(false) {
                disposition |= DUB;
            }
            let mut tags = Dictionary::default();
            // The reference's track dictionary omits the language `und` in exactly
            // that case. The source track keeps it, and later explicit tags can
            // override it.
            if t.language.as_ref() != "und" {
                tags.set("language", t.language.as_ref())?;
            }
            if !t.name.is_empty() {
                tags.set("title", t.name.as_str())?;
            }
            this.tracks.push((t.uid, disposition, tags));
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
                for e in doc.root().children().filter(|e| e.id() == ATTACHED_FILE) {
                    let data = e.field(FILE_DATA).ok_or("missing attachment data")?.raw();
                    if data.is_empty() {
                        continue;
                    }
                    let mut tags = Dictionary::default();
                    tags.set(
                        "filename",
                        e.string(FILE_NAME)?.ok_or("missing attachment filename")?,
                    )?;
                    tags.set(
                        "mimetype",
                        e.string(FILE_MEDIA_TYPE)?
                            .ok_or("missing attachment MIME")?,
                    )?;
                    if let Some(v) = e.string(FILE_DESCRIPTION)? {
                        tags.set("title", v)?;
                    }
                    this.attachments.push(AttachmentView {
                        uid: e.number(FILE_UID)?.ok_or("missing attachment UID")?,
                        tags,
                        data,
                    });
                }
            }
        }
        for doc in d.metadata_documents() {
            for tag in doc.tags() {
                let target = tag.targets().map(|v| v.0);
                let track = target_id(target, TAG_TRACK_UID)?;
                let chapter = target_id(target, TAG_CHAPTER_UID)?;
                let attachment = target_id(target, TAG_ATTACHMENT_UID)?;
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
                    for (id, _, tags) in &mut this.tracks {
                        if *id == uid {
                            tag_values(tag, tags)?;
                        }
                    }
                } else {
                    tag_values(tag, &mut this.global)?;
                }
            }
        }
        Ok(this)
    }
    /// Folds the catalogue into `digest` and, with `out`, appends it as the trace's
    /// `catalogue` member.
    pub fn output(&self, mut out: Option<&mut String>, digest: &mut u64) -> Result<(), Error> {
        if let Some(o) = out.as_deref_mut() {
            o.push_str(",\"catalogue\":{\"global\":");
        }
        self.global.output(out.as_deref_mut(), digest)?;
        fold(digest, self.tracks.len() as u64);
        if let Some(o) = out.as_deref_mut() {
            o.push_str(",\"tracks\":[");
        }
        for (i, (_, disposition, tags)) in self.tracks.iter().enumerate() {
            fold(digest, u64::from(*disposition));
            if let Some(o) = out.as_deref_mut() {
                if i != 0 {
                    o.push(',');
                }
                write!(o, "{{\"disposition\":{disposition},\"tags\":")?;
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
    use triergon_matroska::Limits;

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

    #[test]
    fn creation_time_covers_every_date_utc() {
        for (ns, text) in [
            (0, "2001-01-01T00:00:00.000000Z"),
            (-1, "2001-01-01T00:00:00.000000Z"),
            (1000, "2001-01-01T00:00:00.000001Z"),
            (-1001, "2000-12-31T23:59:59.999999Z"),
            (-86_400_000_000_001, "2000-12-31T00:00:00.000000Z"),
            (36_720_000_000_000_000, "2002-03-02T00:00:00.000000Z"),
            (i64::MIN, "1708-09-22T00:12:43.145225Z"),
            (i64::MAX, "2293-04-11T23:47:16.854775Z"),
        ] {
            assert_eq!(utc_date(ns), text, "{ns}");
        }
    }

    #[test]
    fn civil_days_follow_the_gregorian_calendar() {
        let leap = |y: i32| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let (mut year, mut month, mut day) = (1600, 1, 1);
        for days in -135_140..120_000 {
            assert_eq!(civil_from_days(days), (year, month, day), "{days}");
            let length = match month {
                2 if leap(year) => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            day += 1;
            if day > length {
                (month, day) = (month + 1, 1);
                if month > 12 {
                    (year, month) = (year + 1, 1);
                }
            }
        }
    }
}
