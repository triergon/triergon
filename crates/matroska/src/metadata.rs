//! Bounded source chapters, tags and attachments from RFC 9559 §§5.1.6–8.
//!
//! A document owns its input once. Compact nodes index that shared storage;
//! neither binary attachment data nor strings are copied into each view.
use crate::{
    Error, Result, ebml,
    storage::{SharedBytes, StorageVec},
};
use std::{mem::size_of, ops::Range, sync::Arc};
use triergon_core::{Budget, Reservation};

/// EBML element ID of Info, the [`Document::kind`] of segment information.
pub const INFO: u64 = 0x1549a966;
/// EBML element ID of Attachments, the [`Document::kind`] of attached files.
pub const ATTACHMENTS: u64 = 0x1941a469;
/// EBML element ID of Chapters, the [`Document::kind`] of editions and chapters.
pub const CHAPTERS: u64 = 0x1043a770;
/// EBML element ID of Tags, the [`Document::kind`] of tags.
pub const TAGS: u64 = 0x1254c367;
pub(crate) const SEGMENT: u64 = 0x18538067;
pub(crate) const CLUSTER: u64 = 0x1f43b675;
pub(crate) const TRACKS: u64 = 0x1654ae6b;
pub(crate) const CUES: u64 = 0x1c53bb6b;
pub(crate) const SEEK_HEAD: u64 = 0x114d9b74;

/// The Segment children that carry metadata: every level-one element except Cluster.
pub(crate) fn is_segment_metadata(id: u64) -> bool {
    matches!(
        id,
        INFO | TRACKS | CUES | SEEK_HEAD | ATTACHMENTS | CHAPTERS | TAGS
    )
}

#[derive(Clone, Copy, Debug)]
struct Node {
    id: u32,
    header: u32,
    next: u16,
    parent: u16,
}

/// One validated Info, Chapters, Tags or Attachments element: its payload and a
/// preorder arena of its nodes. Readers hand it out as a [`SharedDocument`].
#[derive(Debug)]
pub struct Document {
    raw: SharedBytes,
    nodes: StorageVec<Node>,
    attachment_bytes: usize,
    source_offset: Option<u64>,
    // Reserve the enclosing Arc/object before Arc::new; release children first.
    _pool: Budget,
    owner_charge: Reservation,
}

/// Shared immutable source document. The private Arc prevents weak handles from
/// retaining an allocation after its document's capacity charge has been released.
#[derive(Clone, Debug)]
pub struct SharedDocument(Arc<Document>);

impl std::ops::Deref for SharedDocument {
    type Target = Document;

    fn deref(&self) -> &Document {
        &self.0
    }
}

impl SharedDocument {
    /// The AttachedFile entries of an Attachments document in source order; none for
    /// other kinds. Each handle keeps the document alive.
    pub fn attachments(&self) -> impl Iterator<Item = Attachment> + '_ {
        self.0.attachments()
    }
}

/// An immutable element view. Unknown extensions remain available as raw bytes.
#[derive(Clone, Copy, Debug)]
pub struct Element<'a> {
    doc: &'a Document,
    index: u16,
}

/// The direct children of an [`Element`] in source order.
#[derive(Debug)]
pub struct Children<'a> {
    doc: &'a Document,
    next: u16,
    end: u16,
}
impl<'a> Iterator for Children<'a> {
    type Item = Element<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let element = Element {
            doc: self.doc,
            index: self.next,
        };
        self.next = self.doc.nodes[self.next as usize].next;
        Some(element)
    }
}

impl<'a> Element<'a> {
    /// Index of this element in the document's preorder arena; the root is 0.
    pub fn ordinal(self) -> usize {
        self.index as usize
    }
    /// The ordinal one past this element's last descendant.
    pub fn subtree_end(self) -> usize {
        self.doc.nodes[self.index as usize].next as usize
    }
    /// The EBML element ID, marker bits included.
    pub fn id(self) -> u64 {
        u64::from(self.doc.nodes[self.index as usize].id)
    }
    /// Byte range of the payload within [`Document::raw`]; all of it for the root.
    pub fn range(self) -> Range<usize> {
        self.range_in(self.doc.raw.as_ref())
    }
    fn range_in(self, raw: &[u8]) -> Range<usize> {
        if self.index == 0 {
            return 0..raw.len();
        }
        // Headers were checked before insertion. A complete immutable preorder
        // arena gives the end from the first node after this subtree, keeping
        // nodes at twelve bytes. During construction, LEN remains authoritative:
        // a completed prefix tail need not be the final raw-source child yet.
        let n = self.doc.nodes[self.index as usize];
        let at = n.header as usize + ebml::id_len(n.id);
        let width = ebml::vint_width(raw[at]);
        let start = at + width;
        if n.next > self.index && self.doc.nodes[0].next as usize == self.doc.nodes.len() {
            let end = self
                .doc
                .nodes
                .get(n.next as usize)
                .map_or(raw.len(), |next| next.header as usize);
            return start..end;
        }
        start..start + ebml::size_at(raw, at, width)
    }
    /// The payload bytes.
    pub fn raw(self) -> &'a [u8] {
        let raw = self.doc.raw.as_ref();
        &raw[self.range_in(raw)]
    }
    /// The direct children in source order.
    pub fn children(self) -> Children<'a> {
        Children {
            doc: self.doc,
            next: self.index + 1,
            end: self.doc.nodes[self.index as usize].next,
        }
    }
    /// The first direct child with element ID `id`.
    pub fn field(self, id: u64) -> Option<Self> {
        self.children().find(|e| e.id() == id)
    }
    /// The payload as an unsigned integer. An empty payload reads as the schema
    /// default: 1000000 for TimestampScale, 1 for ChapterFlagEnabled and TagDefault,
    /// 50 for TargetTypeValue, otherwise 0. Wider than 8 bytes is [`Error::Invalid`].
    pub fn uint(self) -> Result<u64> {
        let raw = self.raw();
        if raw.is_empty() {
            return Ok(default_uint(self.id()));
        }
        ebml::uint(raw)
    }
    /// The payload as a signed integer; empty reads as 0, wider than 8 bytes is
    /// [`Error::Invalid`].
    pub fn sint(self) -> Result<i64> {
        ebml::sint(self.raw())
    }
    /// The payload as a big-endian IEEE 754 float of 4 or 8 bytes; empty reads as 0.
    /// Any other width is [`Error::Invalid`].
    pub fn float(self) -> Result<f64> {
        let raw = self.raw();
        match raw.len() {
            0 => Ok(0.),
            4 => Ok(f64::from(f32::from_be_bytes(
                raw.try_into()
                    .map_err(|_| Error::Invalid("metadata float width"))?,
            ))),
            8 => Ok(f64::from_be_bytes(
                raw.try_into()
                    .map_err(|_| Error::Invalid("metadata float width"))?,
            )),
            _ => Err(Error::Invalid("metadata float width")),
        }
    }
    /// The payload as a string, without its zero padding. An empty ChapLanguage
    /// reads as `eng` and an empty TagLanguage as `und`. Fails with
    /// [`Error::Invalid`] for invalid UTF-8, nonzero bytes after the first zero, or a
    /// byte outside printable ASCII in an ASCII field.
    pub fn text(self) -> Result<&'a str> {
        let raw = self.raw();
        if raw.is_empty() {
            match self.id() {
                0x437c => return Ok("eng"),
                0x447a => return Ok("und"),
                _ => (),
            }
        }
        text(raw, ascii(self.id()))
    }
    /// [`uint`](Self::uint) of the first child `id`; `None` when absent.
    pub fn number(self, id: u64) -> Result<Option<u64>> {
        self.field(id).map(Self::uint).transpose()
    }
    /// [`text`](Self::text) of the first child `id`; `None` when absent.
    pub fn string(self, id: u64) -> Result<Option<&'a str>> {
        self.field(id).map(Self::text).transpose()
    }
    /// Whether the document reads this field as an unsigned integer.
    pub fn is_unsigned(self) -> bool {
        unsigned(self.id())
    }
    /// Whether the document reads this field as a UTF-8 or ASCII string.
    pub fn is_text(self) -> bool {
        utf8(self.id()) || ascii(self.id())
    }
    /// The enclosing element; `None` for the root.
    pub fn parent(self) -> Option<Self> {
        let p = self.doc.nodes[self.index as usize].parent;
        (p != u16::MAX).then_some(Self {
            doc: self.doc,
            index: p,
        })
    }
}

/// An attachment handle retains exactly one shared document after reader teardown.
#[derive(Clone, Debug)]
pub struct Attachment {
    document: Arc<Document>,
    index: u16,
}
impl Attachment {
    /// The AttachedFile element.
    pub fn element(&self) -> Element<'_> {
        Element {
            doc: &self.document,
            index: self.index,
        }
    }
    /// FileUID. Fails with [`Error::Invalid`] when absent.
    pub fn uid(&self) -> Result<u64> {
        self.element()
            .number(0x46ae)?
            .ok_or(Error::Invalid("attachment UID"))
    }
    /// FileName. Fails with [`Error::Invalid`] when absent.
    pub fn filename(&self) -> Result<&str> {
        self.element()
            .string(0x466e)?
            .ok_or(Error::Invalid("attachment filename"))
    }
    /// FileMediaType, the attachment's media type. Fails with [`Error::Invalid`]
    /// when absent.
    pub fn media_type(&self) -> Result<&str> {
        self.element()
            .string(0x4660)?
            .ok_or(Error::Invalid("attachment media type"))
    }
    /// FileDescription; `None` when absent.
    pub fn description(&self) -> Result<Option<&str>> {
        self.element().string(0x467e)
    }
    /// The FileData payload, borrowed from the document. Fails with
    /// [`Error::Invalid`] when absent.
    pub fn data(&self) -> Result<&[u8]> {
        self.element()
            .field(0x465c)
            .map(Element::raw)
            .ok_or(Error::Invalid("attachment data"))
    }
    /// Whether both attachments are held by the same document.
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.document, &other.document)
    }
}

impl Document {
    /// The element ID of the document: [`INFO`], [`CHAPTERS`], [`TAGS`] or [`ATTACHMENTS`].
    pub fn kind(&self) -> u64 {
        u64::from(self.nodes[0].id)
    }
    /// The element payload as read.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    /// The level-one element itself, ordinal 0.
    pub fn root(&self) -> Element<'_> {
        Element {
            doc: self,
            index: 0,
        }
    }
    /// Absolute source payload offset when loaded by a container reader.
    pub fn source_offset(&self) -> Option<u64> {
        self.source_offset
    }
    /// Elements in the document, the root included.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    /// An upper bound of the bytes this document keeps charged: source capacity,
    /// node arena and owner overhead.
    pub fn retained_bytes(&self) -> usize {
        self.raw.retained_capacity()
            + self.nodes.capacity() * size_of::<Node>()
            + size_of::<Self>()
            + 2 * size_of::<usize>()
            + self.raw.owner_overhead_bound()
    }
    fn attachments(self: &Arc<Self>) -> impl Iterator<Item = Attachment> + '_ {
        self.root()
            .children()
            .filter(|e| self.kind() == ATTACHMENTS && e.id() == 0x61a7)
            .map(|e| Attachment {
                document: Arc::clone(self),
                index: e.index,
            })
    }
    /// Every element in preorder, the root first.
    pub fn elements(&self) -> impl Iterator<Item = Element<'_>> {
        (0..self.nodes.len()).map(|i| Element {
            doc: self,
            index: i as u16,
        })
    }
    /// Test oracle entry: build one document through the typed document job.
    #[cfg(test)]
    pub(crate) fn parse(
        kind: u64,
        raw: Vec<u8>,
        node_limit: usize,
        depth_limit: usize,
        storage_limit: usize,
        attachment_limit: usize,
    ) -> Result<Self> {
        use crate::step::ParseStep;
        let capacity = storage_limit
            .checked_mul(3)
            .and_then(|n| n.checked_add(4096))
            .ok_or(Error::Overflow)?;
        let pool = Budget::new(capacity)?;
        let raw = SharedBytes::try_from_vec(&pool, raw)?;
        let limits = build::DocumentLimits {
            node_limit,
            depth_limit,
            storage_limit,
            attachment_limit,
        };
        let mut job = build::DocumentJob::new(kind, raw, limits, None, &pool)?;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let document = loop {
            match job.step(&mut triergon_core::WorkBudget::new(u64::MAX), &cancel)? {
                ParseStep::Event(document) => break document,
                ParseStep::Yield => continue,
                other => panic!("unexpected document step {other:?}"),
            }
        };
        drop(job);
        Arc::try_unwrap(document.0).map_err(|_| Error::Invalid("test document is shared"))
    }
}

fn master(id: u64) -> bool {
    matches!(
        id,
        INFO | ATTACHMENTS
            | CHAPTERS
            | TAGS
            | 0x61a7
            | 0x45b9
            | 0xb6
            | 0x80
            | 0x8f
            | 0x6944
            | 0x6911
            | 0x7373
            | 0x63c0
            | 0x67c8
    )
}
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
fn boolean(id: u64) -> bool {
    matches!(id, 0x45bd | 0x45db | 0x45dd | 0x98 | 0x4598 | 0x4484)
}
fn default_uint(id: u64) -> u64 {
    match id {
        0x2ad7b1 => 1_000_000,
        0x4598 | 0x4484 => 1,
        0x68ca => 50,
        _ => 0,
    }
}
fn utf8(id: u64) -> bool {
    matches!(
        id,
        0x7ba9 | 0x4d80 | 0x5741 | 0x467e | 0x466e | 0x5654 | 0x85 | 0x45a3 | 0x4487
    )
}
fn ascii(id: u64) -> bool {
    matches!(
        id,
        0x4660 | 0x437c | 0x437d | 0x437e | 0x63ca | 0x447a | 0x447b
    )
}
fn text(data: &[u8], ascii_only: bool) -> Result<&str> {
    let end = data.iter().position(|v| *v == 0).unwrap_or(data.len());
    if data[end..].iter().any(|v| *v != 0) {
        return Err(Error::Invalid("metadata string terminator"));
    }
    let text = std::str::from_utf8(&data[..end]).map_err(|_| Error::Invalid("UTF-8 metadata"))?;
    if ascii_only && !text.bytes().all(|v| (0x20..=0x7e).contains(&v)) {
        return Err(Error::Invalid("ASCII metadata"));
    }
    Ok(text)
}

/// A reader's cumulative metadata arena. Clones retain shared documents, not copies.
#[derive(Debug)]
pub(crate) struct Catalogue {
    pool: Budget,
    documents: StorageVec<SharedDocument>,
    retained: usize,
    nodes: usize,
    attachments: usize,
}
impl Catalogue {
    pub(crate) fn new(pool: &Budget) -> Self {
        Self {
            pool: pool.clone(),
            documents: StorageVec::new(pool),
            retained: 0,
            nodes: 0,
            attachments: 0,
        }
    }
    pub(crate) fn documents(&self) -> &[SharedDocument] {
        &self.documents
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained + self.documents.capacity() * size_of::<SharedDocument>()
    }
    pub(crate) fn attachment_bytes(&self) -> usize {
        self.attachments
    }
    /// Test entry: commit one document through the typed catalogue job.
    #[cfg(test)]
    pub(crate) fn add(
        &mut self,
        kind: u64,
        raw: Vec<u8>,
        structural_limit: usize,
        source_offset: Option<u64>,
        limits: crate::Limits,
    ) -> Result<usize> {
        use crate::step::ParseStep;
        let raw = SharedBytes::try_from_vec(&self.pool, raw)?;
        let pool = self.pool.clone();
        let mut job =
            build::CatalogueJob::new(kind, raw, structural_limit, source_offset, limits, &pool)?;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        loop {
            match job.step(self, &mut triergon_core::WorkBudget::new(u64::MAX), &cancel)? {
                ParseStep::Event(commit) => return Ok(commit.structural_bytes),
                ParseStep::Yield => continue,
                other => panic!("unexpected catalogue step {other:?}"),
            }
        }
    }
}

mod views;
pub use views::{Chapter, ChapterDisplay, Edition, SegmentInfo, SimpleTag, Tag, Targets};

#[cfg(test)]
mod tests;

pub(crate) mod build;
