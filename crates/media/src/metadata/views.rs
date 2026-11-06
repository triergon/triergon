//! Borrowed typed views over validated source metadata.
use super::{CHAPTERS, Document, Element, INFO, TAGS};
use crate::{Error, Result};
/// Chapter/edition views preserve nesting; they do not execute ordered editions or menus.
#[derive(Debug, Clone, Copy)]
pub struct Edition<'a>(pub Element<'a>);
#[derive(Debug, Clone, Copy)]
pub struct Chapter<'a>(pub Element<'a>);
#[derive(Debug, Clone, Copy)]
pub struct ChapterDisplay<'a>(pub Element<'a>);
#[derive(Debug, Clone, Copy)]
pub struct Tag<'a>(pub Element<'a>);
#[derive(Debug, Clone, Copy)]
pub struct Targets<'a>(pub Element<'a>);
#[derive(Debug, Clone, Copy)]
pub struct SimpleTag<'a>(pub Element<'a>);
impl Document {
    pub fn editions(&self) -> impl Iterator<Item = Edition<'_>> {
        self.root()
            .children()
            .filter(|e| self.kind() == CHAPTERS && e.id() == 0x45b9)
            .map(Edition)
    }
    pub fn tags(&self) -> impl Iterator<Item = Tag<'_>> {
        self.root()
            .children()
            .filter(|e| self.kind() == TAGS && e.id() == 0x7373)
            .map(Tag)
    }
}
impl<'a> Edition<'a> {
    pub fn uid(self) -> Result<Option<u64>> {
        self.0.number(0x45bc)
    }
    pub fn is_default(self) -> Result<bool> {
        Ok(self.0.number(0x45db)?.unwrap_or(0) != 0)
    }
    pub fn ordered(self) -> Result<bool> {
        Ok(self.0.number(0x45dd)?.unwrap_or(0) != 0)
    }
    pub fn chapters(self) -> impl Iterator<Item = Chapter<'a>> {
        self.0.children().filter(|e| e.id() == 0xb6).map(Chapter)
    }
}
impl<'a> Chapter<'a> {
    pub fn uid(self) -> Result<u64> {
        self.0.number(0x73c4)?.ok_or(Error::Invalid("chapter UID"))
    }
    pub fn start_ns(self) -> Result<u64> {
        self.0.number(0x91)?.ok_or(Error::Invalid("chapter start"))
    }
    pub fn end_ns(self) -> Result<Option<u64>> {
        self.0.number(0x92)
    }
    pub fn hidden(self) -> Result<bool> {
        Ok(self.0.number(0x98)?.unwrap_or(0) != 0)
    }
    pub fn enabled(self) -> Result<bool> {
        Ok(self.0.number(0x4598)?.unwrap_or(1) != 0)
    }
    pub fn children(self) -> impl Iterator<Item = Chapter<'a>> {
        self.0.children().filter(|e| e.id() == 0xb6).map(Chapter)
    }
    pub fn displays(self) -> impl Iterator<Item = ChapterDisplay<'a>> {
        self.0
            .children()
            .filter(|e| e.id() == 0x80)
            .map(ChapterDisplay)
    }
}
impl<'a> ChapterDisplay<'a> {
    pub fn title(self) -> Result<&'a str> {
        self.0.string(0x85)?.ok_or(Error::Invalid("chapter title"))
    }
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
    pub fn targets(self) -> Option<Targets<'a>> {
        self.0.field(0x63c0).map(Targets)
    }
    pub fn values(self) -> impl Iterator<Item = SimpleTag<'a>> {
        self.0
            .children()
            .filter(|e| e.id() == 0x67c8)
            .map(SimpleTag)
    }
}
impl<'a> Targets<'a> {
    pub fn level(self) -> Result<u64> {
        Ok(self.0.number(0x68ca)?.unwrap_or(50))
    }
    pub fn type_name(self) -> Result<Option<&'a str>> {
        self.0.string(0x63ca)
    }
    pub fn uids(self, kind: u64) -> impl Iterator<Item = Result<u64>> {
        self.0
            .children()
            .filter(move |e| e.id() == kind)
            .map(Element::uint)
    }
}
impl<'a> SimpleTag<'a> {
    pub fn name(self) -> Result<&'a str> {
        self.0.string(0x45a3)?.ok_or(Error::Invalid("tag name"))
    }
    pub fn language(self) -> Result<&'a str> {
        Ok(self
            .0
            .string(0x447b)?
            .or(self.0.string(0x447a)?)
            .unwrap_or("und"))
    }
    pub fn is_default(self) -> Result<bool> {
        Ok(self.0.number(0x4484)?.unwrap_or(1) != 0)
    }
    pub fn text(self) -> Result<Option<&'a str>> {
        self.0.string(0x4487)
    }
    pub fn binary(self) -> Option<&'a [u8]> {
        self.0.field(0x4485).map(Element::raw)
    }
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
    pub fn info(&self) -> Option<SegmentInfo<'_>> {
        (self.kind() == INFO).then_some(SegmentInfo(self.root()))
    }
}
impl<'a> SegmentInfo<'a> {
    pub fn title(self) -> Result<Option<&'a str>> {
        self.0.string(0x7ba9)
    }
    pub fn muxing_app(self) -> Result<Option<&'a str>> {
        self.0.string(0x4d80)
    }
    pub fn writing_app(self) -> Result<Option<&'a str>> {
        self.0.string(0x5741)
    }
    pub fn timestamp_scale_ns(self) -> Result<u64> {
        Ok(self.0.number(0x2ad7b1)?.unwrap_or(1_000_000))
    }
    pub fn duration_ticks(self) -> Result<Option<f64>> {
        self.0.field(0x4489).map(Element::float).transpose()
    }
    pub fn date_utc_ns(self) -> Result<Option<i64>> {
        self.0.field(0x4461).map(Element::sint).transpose()
    }
}
