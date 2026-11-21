//! Borrowed typed views over validated source metadata.
use super::{CHAPTERS, Document, Element, INFO, TAGS};
use crate::{Error, Result};
/// Chapter/edition views preserve nesting; they do not execute ordered editions or menus.
#[derive(Debug, Clone, Copy)]
pub struct Edition<'a>(pub Element<'a>);
/// One ChapterAtom, with its displays and nested chapters.
#[derive(Debug, Clone, Copy)]
pub struct Chapter<'a>(pub Element<'a>);
/// One ChapterDisplay: a chapter title and its languages.
#[derive(Debug, Clone, Copy)]
pub struct ChapterDisplay<'a>(pub Element<'a>);
/// One Tag: its Targets and SimpleTags.
#[derive(Debug, Clone, Copy)]
pub struct Tag<'a>(pub Element<'a>);
/// The Targets of a Tag: the level and the UIDs the tag applies to.
#[derive(Debug, Clone, Copy)]
pub struct Targets<'a>(pub Element<'a>);
/// One SimpleTag: a name, a string or binary value and nested SimpleTags.
#[derive(Debug, Clone, Copy)]
pub struct SimpleTag<'a>(pub Element<'a>);
impl Document {
    /// The EditionEntry elements of a Chapters document in source order; none for
    /// other kinds.
    pub fn editions(&self) -> impl Iterator<Item = Edition<'_>> {
        self.root()
            .children()
            .filter(|e| self.kind() == CHAPTERS && e.id() == 0x45b9)
            .map(Edition)
    }
    /// The Tag elements of a Tags document in source order; none for other kinds.
    pub fn tags(&self) -> impl Iterator<Item = Tag<'_>> {
        self.root()
            .children()
            .filter(|e| self.kind() == TAGS && e.id() == 0x7373)
            .map(Tag)
    }
}
impl<'a> Edition<'a> {
    /// EditionUID; `None` when absent.
    pub fn uid(self) -> Result<Option<u64>> {
        self.0.number(0x45bc)
    }
    /// EditionFlagDefault; `false` when absent.
    pub fn is_default(self) -> Result<bool> {
        Ok(self.0.number(0x45db)?.unwrap_or(0) != 0)
    }
    /// EditionFlagOrdered: whether the chapters define an ordered playback, which
    /// this crate reports but does not execute; `false` when absent.
    pub fn ordered(self) -> Result<bool> {
        Ok(self.0.number(0x45dd)?.unwrap_or(0) != 0)
    }
    /// The top-level ChapterAtoms in source order.
    pub fn chapters(self) -> impl Iterator<Item = Chapter<'a>> {
        self.0.children().filter(|e| e.id() == 0xb6).map(Chapter)
    }
}
impl<'a> Chapter<'a> {
    /// ChapterUID. Fails with [`Error::Invalid`] when absent.
    pub fn uid(self) -> Result<u64> {
        self.0.number(0x73c4)?.ok_or(Error::Invalid("chapter UID"))
    }
    /// ChapterTimeStart in nanoseconds. Fails with [`Error::Invalid`] when absent.
    pub fn start_ns(self) -> Result<u64> {
        self.0.number(0x91)?.ok_or(Error::Invalid("chapter start"))
    }
    /// ChapterTimeEnd in nanoseconds; `None` when absent.
    pub fn end_ns(self) -> Result<Option<u64>> {
        self.0.number(0x92)
    }
    /// ChapterFlagHidden; `false` when absent.
    pub fn hidden(self) -> Result<bool> {
        Ok(self.0.number(0x98)?.unwrap_or(0) != 0)
    }
    /// ChapterFlagEnabled; `true` when absent.
    pub fn enabled(self) -> Result<bool> {
        Ok(self.0.number(0x4598)?.unwrap_or(1) != 0)
    }
    /// The nested ChapterAtoms in source order.
    pub fn children(self) -> impl Iterator<Item = Chapter<'a>> {
        self.0.children().filter(|e| e.id() == 0xb6).map(Chapter)
    }
    /// The ChapterDisplay elements in source order.
    pub fn displays(self) -> impl Iterator<Item = ChapterDisplay<'a>> {
        self.0
            .children()
            .filter(|e| e.id() == 0x80)
            .map(ChapterDisplay)
    }
}
impl<'a> ChapterDisplay<'a> {
    /// ChapString, the chapter title. Fails with [`Error::Invalid`] when absent.
    pub fn title(self) -> Result<&'a str> {
        self.0.string(0x85)?.ok_or(Error::Invalid("chapter title"))
    }
    /// The ChapLanguageBCP47 values when any is present, otherwise the ChapLanguage
    /// values, in source order; `eng` alone when neither is present.
    pub fn languages(self) -> impl Iterator<Item = Result<&'a str>> {
        let ietf = self.0.field(0x437d).is_some();
        let id = if ietf { 0x437d } else { 0x437c };
        let absent = self.0.field(id).is_none();
        self.0
            .children()
            .filter(move |e| e.id() == id)
            .map(Element::text)
            .chain(absent.then_some(Ok("eng")))
    }
}
impl<'a> Tag<'a> {
    /// The Targets element; `None` when absent. An absent or empty Targets applies
    /// the tag to the whole Segment.
    pub fn targets(self) -> Option<Targets<'a>> {
        self.0.field(0x63c0).map(Targets)
    }
    /// The top-level SimpleTags in source order.
    pub fn values(self) -> impl Iterator<Item = SimpleTag<'a>> {
        self.0
            .children()
            .filter(|e| e.id() == 0x67c8)
            .map(SimpleTag)
    }
}
impl<'a> Targets<'a> {
    /// TargetTypeValue, the logical level the tag describes (for example 50 for an
    /// album, movie or episode); 50 when absent.
    pub fn level(self) -> Result<u64> {
        Ok(self.0.number(0x68ca)?.unwrap_or(50))
    }
    /// TargetType, an informational name for the level; `None` when absent.
    pub fn type_name(self) -> Result<Option<&'a str>> {
        self.0.string(0x63ca)
    }
    /// The values of every child with element ID `kind` in source order: `0x63c5`
    /// TagTrackUID, `0x63c9` TagEditionUID, `0x63c4` TagChapterUID or `0x63c6`
    /// TagAttachmentUID.
    pub fn uids(self, kind: u64) -> impl Iterator<Item = Result<u64>> {
        self.0
            .children()
            .filter(move |e| e.id() == kind)
            .map(Element::uint)
    }
}
impl<'a> SimpleTag<'a> {
    /// TagName. Fails with [`Error::Invalid`] when absent.
    pub fn name(self) -> Result<&'a str> {
        self.0.string(0x45a3)?.ok_or(Error::Invalid("tag name"))
    }
    /// TagLanguageBCP47 when present, otherwise TagLanguage; `und` when neither is.
    pub fn language(self) -> Result<&'a str> {
        Ok(self
            .0
            .string(0x447b)?
            .or(self.0.string(0x447a)?)
            .unwrap_or("und"))
    }
    /// TagDefault; `true` when absent.
    pub fn is_default(self) -> Result<bool> {
        Ok(self.0.number(0x4484)?.unwrap_or(1) != 0)
    }
    /// TagString; `None` when absent.
    pub fn text(self) -> Result<Option<&'a str>> {
        self.0.string(0x4487)
    }
    /// The TagBinary payload; `None` when absent.
    pub fn binary(self) -> Option<&'a [u8]> {
        self.0.field(0x4485).map(Element::raw)
    }
    /// The nested SimpleTags in source order.
    pub fn children(self) -> impl Iterator<Item = SimpleTag<'a>> {
        self.0
            .children()
            .filter(|e| e.id() == 0x67c8)
            .map(SimpleTag)
    }
}

/// Segment Info source values. DateUTC remains signed nanoseconds since 2001-01-01.
#[derive(Debug, Clone, Copy)]
pub struct SegmentInfo<'a>(pub Element<'a>);
impl Document {
    /// The segment information of an Info document; `None` for other kinds.
    pub fn info(&self) -> Option<SegmentInfo<'_>> {
        (self.kind() == INFO).then_some(SegmentInfo(self.root()))
    }
}
impl<'a> SegmentInfo<'a> {
    /// Title; `None` when absent.
    pub fn title(self) -> Result<Option<&'a str>> {
        self.0.string(0x7ba9)
    }
    /// MuxingApp; `None` when absent.
    pub fn muxing_app(self) -> Result<Option<&'a str>> {
        self.0.string(0x4d80)
    }
    /// WritingApp; `None` when absent.
    pub fn writing_app(self) -> Result<Option<&'a str>> {
        self.0.string(0x5741)
    }
    /// TimestampScale: nanoseconds per Segment tick; 1000000 when absent.
    pub fn timestamp_scale_ns(self) -> Result<u64> {
        Ok(self.0.number(0x2ad7b1)?.unwrap_or(1_000_000))
    }
    /// Duration of the Segment in Segment ticks, as stored; `None` when absent.
    pub fn duration_ticks(self) -> Result<Option<f64>> {
        self.0.field(0x4489).map(Element::float).transpose()
    }
    /// DateUTC in nanoseconds since 2001-01-01T00:00:00 UTC; `None` when absent.
    pub fn date_utc_ns(self) -> Result<Option<i64>> {
        self.0.field(0x4461).map(Element::sint).transpose()
    }
}
