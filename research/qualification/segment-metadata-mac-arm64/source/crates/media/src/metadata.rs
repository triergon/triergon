//! Bounded source chapters, tags and attachments from RFC 9559 §§5.1.6–8.
//!
//! A document owns its input once. Compact nodes index that shared storage;
//! neither binary attachment data nor strings are copied into each view.
use crate::{Error, Result, ebml};
use std::{mem::size_of, ops::Range, sync::Arc};

pub const INFO: u64 = 0x1549a966;
pub const ATTACHMENTS: u64 = 0x1941a469;
pub const CHAPTERS: u64 = 0x1043a770;
pub const TAGS: u64 = 0x1254c367;

#[derive(Clone, Copy, Debug)]
struct Node {
    id: u32,
    header: u32,
    next: u16,
    parent: u16,
}

#[derive(Debug)]
pub struct Document {
    raw: Vec<u8>,
    nodes: Vec<Node>,
    attachment_bytes: usize,
    source_offset: Option<u64>,
}

/// An immutable element view. Unknown extensions remain available as raw bytes.
#[derive(Clone, Copy, Debug)]
pub struct Element<'a> {
    doc: &'a Document,
    index: u16,
}

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
    pub fn ordinal(self) -> usize {
        self.index as usize
    }
    pub fn subtree_end(self) -> usize {
        self.doc.nodes[self.index as usize].next as usize
    }
    pub fn id(self) -> u64 {
        u64::from(self.doc.nodes[self.index as usize].id)
    }
    pub fn range(self) -> Range<usize> {
        if self.index == 0 {
            return 0..self.doc.raw.len();
        }
        // Headers were checked before insertion and the document is immutable.
        // Recovering a bounded VINT keeps each arena node at twelve bytes.
        let n = self.doc.nodes[self.index as usize];
        let at = n.header as usize + (32 - n.id.leading_zeros()).div_ceil(8) as usize;
        let width = self.doc.raw[at].leading_zeros() as usize + 1;
        let mut length = usize::from(self.doc.raw[at] & (0xff_u64 >> width) as u8);
        for byte in &self.doc.raw[at + 1..at + width] {
            length = (length << 8) | usize::from(*byte);
        }
        let start = at + width;
        start..start + length
    }
    pub fn raw(self) -> &'a [u8] {
        &self.doc.raw[self.range()]
    }
    pub fn children(self) -> Children<'a> {
        Children {
            doc: self.doc,
            next: self.index + 1,
            end: self.doc.nodes[self.index as usize].next,
        }
    }
    pub fn field(self, id: u64) -> Option<Self> {
        self.children().find(|e| e.id() == id)
    }
    pub fn uint(self) -> Result<u64> {
        if self.raw().is_empty() {
            return Ok(default_uint(self.id()));
        }
        ebml::uint(self.raw())
    }
    pub fn sint(self) -> Result<i64> {
        ebml::sint(self.raw())
    }
    pub fn float(self) -> Result<f64> {
        match self.raw().len() {
            0 => Ok(0.),
            4 => Ok(f64::from(f32::from_be_bytes(
                self.raw()
                    .try_into()
                    .map_err(|_| Error::Invalid("metadata float width"))?,
            ))),
            8 => Ok(f64::from_be_bytes(
                self.raw()
                    .try_into()
                    .map_err(|_| Error::Invalid("metadata float width"))?,
            )),
            _ => Err(Error::Invalid("metadata float width")),
        }
    }
    pub fn text(self) -> Result<&'a str> {
        if self.raw().is_empty() {
            match self.id() {
                0x437c => return Ok("eng"),
                0x447a => return Ok("und"),
                _ => (),
            }
        }
        text(self.raw(), ascii(self.id()))
    }
    pub fn number(self, id: u64) -> Result<Option<u64>> {
        self.field(id).map(Self::uint).transpose()
    }
    pub fn string(self, id: u64) -> Result<Option<&'a str>> {
        self.field(id).map(Self::text).transpose()
    }
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
    pub fn element(&self) -> Element<'_> {
        Element {
            doc: &self.document,
            index: self.index,
        }
    }
    pub fn uid(&self) -> Result<u64> {
        self.element()
            .number(0x46ae)?
            .ok_or(Error::Invalid("attachment UID"))
    }
    pub fn filename(&self) -> Result<&str> {
        self.element()
            .string(0x466e)?
            .ok_or(Error::Invalid("attachment filename"))
    }
    pub fn media_type(&self) -> Result<&str> {
        self.element()
            .string(0x4660)?
            .ok_or(Error::Invalid("attachment media type"))
    }
    pub fn description(&self) -> Result<Option<&str>> {
        self.element().string(0x467e)
    }
    pub fn data(&self) -> Result<&[u8]> {
        self.element()
            .field(0x465c)
            .map(Element::raw)
            .ok_or(Error::Invalid("attachment data"))
    }
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.document, &other.document)
    }
}

impl Document {
    pub fn kind(&self) -> u64 {
        u64::from(self.nodes[0].id)
    }
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
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
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn attachment_bytes(&self) -> usize {
        self.attachment_bytes
    }
    pub fn retained_bytes(&self) -> usize {
        self.raw.capacity()
            + self.nodes.capacity() * size_of::<Node>()
            + size_of::<Self>()
            + 2 * size_of::<usize>()
    }
    pub fn attachments(self: &Arc<Self>) -> impl Iterator<Item = Attachment> + '_ {
        self.root()
            .children()
            .filter(|e| self.kind() == ATTACHMENTS && e.id() == 0x61a7)
            .map(|e| Attachment {
                document: Arc::clone(self),
                index: e.index,
            })
    }
    pub fn elements(&self) -> impl Iterator<Item = Element<'_>> {
        (0..self.nodes.len()).map(|i| Element {
            doc: self,
            index: i as u16,
        })
    }
    pub(crate) fn parse(
        kind: u64,
        raw: Vec<u8>,
        node_limit: usize,
        depth_limit: usize,
        storage_limit: usize,
        attachment_limit: usize,
    ) -> Result<Self> {
        if !matches!(kind, INFO | ATTACHMENTS | CHAPTERS | TAGS) {
            return Err(Error::Invalid("metadata document type"));
        }
        u32::try_from(raw.len()).map_err(|_| Error::Limit("metadata offset space"))?;
        let mut doc = Self {
            raw,
            nodes: Vec::new(),
            attachment_bytes: 0,
            source_offset: None,
        };
        doc.push(
            Node {
                id: kind as u32,
                header: 0,
                next: 1,
                parent: u16::MAX,
            },
            node_limit,
            storage_limit,
        )?;
        doc.parse_children(
            0,
            0,
            node_limit,
            depth_limit,
            storage_limit,
            attachment_limit,
        )?;
        doc.validate(storage_limit)?;
        Ok(doc)
    }
    fn push(&mut self, node: Node, limit: usize, storage: usize) -> Result<u16> {
        let limit = limit.min(u16::MAX as usize);
        if self.nodes.len() >= limit {
            return Err(Error::Limit("metadata nodes"));
        }
        if self.nodes.len() == self.nodes.capacity() {
            let initial = (self.raw.len() / 2 + 1).min(8);
            let target = self.nodes.len().saturating_mul(2).max(initial).min(limit);
            let bytes = target
                .checked_mul(size_of::<Node>())
                .and_then(|n| n.checked_add(self.raw.capacity()))
                .and_then(|n| n.checked_add(size_of::<Self>() + 2 * size_of::<usize>()))
                .ok_or(Error::Overflow)?;
            if bytes > storage {
                return Err(Error::Limit("metadata retained storage"));
            }
            self.nodes
                .try_reserve_exact(target - self.nodes.len())
                .map_err(|_| Error::Limit("metadata node allocation"))?;
            if self.retained_bytes() > storage {
                return Err(Error::Limit("metadata retained storage"));
            }
        }
        let index =
            u16::try_from(self.nodes.len()).map_err(|_| Error::Limit("metadata index space"))?;
        self.nodes.push(node);
        Ok(index)
    }
    fn parse_children(
        &mut self,
        parent: u16,
        depth: usize,
        nodes: usize,
        max_depth: usize,
        storage: usize,
        attachments: usize,
    ) -> Result<()> {
        if depth > max_depth || depth > 64 {
            return Err(Error::Limit("metadata nesting"));
        }
        let p = Element {
            doc: self,
            index: parent,
        }
        .range();
        let mut at = p.start;
        while at < p.end {
            let (id, a, _) = ebml::vint(&self.raw[at..p.end], true)?;
            let (length, b, unknown) = ebml::vint(&self.raw[at + a..p.end], false)?;
            if unknown {
                return Err(Error::Invalid("unknown metadata child size"));
            }
            let start = at.checked_add(a + b).ok_or(Error::Overflow)?;
            let end = start
                .checked_add(usize::try_from(length).map_err(|_| Error::Overflow)?)
                .ok_or(Error::Overflow)?;
            if end > p.end {
                return Err(Error::Truncated);
            }
            let index = self.push(
                Node {
                    id: id as u32,
                    header: at as u32,
                    next: 0,
                    parent,
                },
                nodes,
                storage,
            )?;
            if master(id) {
                self.parse_children(index, depth + 1, nodes, max_depth, storage, attachments)?;
            } else {
                let element = Element { doc: self, index };
                if unsigned(id) {
                    let value = element.uint()?;
                    if boolean(id) && value > 1 {
                        return Err(Error::Invalid("metadata boolean"));
                    }
                    if matches!(id, 0x46ae | 0x73c4 | 0x45bc | 0x6ebc | 0x68ca) && value == 0 {
                        return Err(Error::Invalid("zero metadata UID/type"));
                    }
                    if id == 0x6922 && value > 2 {
                        return Err(Error::Invalid("chapter process time"));
                    }
                }
                if id == 0x4489 {
                    let value = element.float()?;
                    if !value.is_finite() || value <= 0. {
                        return Err(Error::Invalid("Segment duration range"));
                    }
                }
                if id == 0x4461 && !matches!(element.raw().len(), 0 | 8) {
                    return Err(Error::Invalid("DateUTC width"));
                }
                if utf8(id) || ascii(id) {
                    element.text()?;
                }
                if id == 0x6e67 && element.raw().len() != 16 {
                    return Err(Error::Invalid("chapter SegmentUUID length"));
                }
                if id == 0x465c {
                    self.attachment_bytes = self
                        .attachment_bytes
                        .checked_add(end - start)
                        .ok_or(Error::Overflow)?;
                    if self.attachment_bytes > attachments {
                        return Err(Error::Limit("attachment payload bytes"));
                    }
                }
            }
            self.nodes[index as usize].next =
                u16::try_from(self.nodes.len()).map_err(|_| Error::Overflow)?;
            at = end;
        }
        self.nodes[parent as usize].next =
            u16::try_from(self.nodes.len()).map_err(|_| Error::Overflow)?;
        Ok(())
    }
    fn validate(&self, storage: usize) -> Result<()> {
        for e in self.elements() {
            let singles: &[u64] = match e.id() {
                INFO => &[
                    0x2ad7b1, 0x4489, 0x4461, 0x7ba9, 0x4d80, 0x5741, 0x73a4, 0x3cb923, 0x3eb923,
                ],
                0x61a7 => &[0x46ae, 0x466e, 0x4660, 0x467e, 0x465c],
                0x45b9 => &[0x45bc, 0x45bd, 0x45db, 0x45dd],
                0xb6 => &[
                    0x73c4, 0x5654, 0x91, 0x92, 0x98, 0x4598, 0x6e67, 0x6ebc, 0x63c3, 0x8f,
                ],
                0x80 => &[0x85],
                0x7373 => &[0x63c0],
                0x63c0 => &[0x68ca, 0x63ca],
                0x67c8 => &[0x45a3, 0x447a, 0x447b, 0x4484, 0x4487, 0x4485],
                0x6944 => &[0x6955, 0x450d],
                0x6911 => &[0x6922, 0x6933],
                _ => &[],
            };
            let mut seen = 0_u64;
            for child in e.children() {
                if let Some(bit) = singles.iter().position(|id| *id == child.id()) {
                    if seen & (1 << bit) != 0 {
                        return Err(Error::Invalid("duplicate metadata field"));
                    }
                    seen |= 1 << bit;
                }
            }
            let require = |ids: &[u64]| -> Result<()> {
                if ids.iter().any(|id| e.field(*id).is_none()) {
                    Err(Error::Invalid("missing metadata field"))
                } else {
                    Ok(())
                }
            };
            match e.id() {
                ATTACHMENTS => require(&[0x61a7])?,
                CHAPTERS => require(&[0x45b9])?,
                TAGS => require(&[0x7373])?,
                0x61a7 => require(&[0x46ae, 0x466e, 0x4660, 0x465c])?,
                0x45b9 => require(&[0xb6])?,
                0xb6 => {
                    require(&[0x73c4, 0x91])?;
                    let start = e.number(0x91)?.ok_or(Error::Invalid("chapter start"))?;
                    if e.number(0x92)?.is_some_and(|end| end < start) {
                        return Err(Error::Invalid("chapter time range"));
                    }
                    if e.field(0x6ebc).is_some() {
                        require(&[0x6e67])?;
                    }
                    let mut ancestor = e.parent();
                    let mut edition = None;
                    while let Some(p) = ancestor {
                        if p.id() == 0xb6
                            && (p.number(0x91)?.is_some_and(|v| start < v)
                                || p.number(0x92)?.is_some_and(|v| start > v))
                        {
                            return Err(Error::Invalid("nested chapter time range"));
                        }
                        if p.id() == 0x45b9 {
                            edition = Some(p);
                            break;
                        }
                        ancestor = p.parent();
                    }
                    if edition.is_some_and(|p| p.number(0x45dd).ok().flatten() == Some(1))
                        && e.field(0xb6).is_none()
                    {
                        require(&[0x92])?;
                    }
                }
                0x80 => require(&[0x85])?,
                0x7373 => require(&[0x67c8])?,
                0x67c8 => {
                    require(&[0x45a3])?;
                    if e.field(0x4487).is_some() && e.field(0x4485).is_some() {
                        return Err(Error::Invalid("text and binary tag"));
                    }
                }
                0x6911 => require(&[0x6922, 0x6933])?,
                _ => (),
            }
        }
        // Bounded temporary UID storage; documents without UIDs need none.
        let uid_count = self
            .elements()
            .filter(|e| matches!(e.id(), 0x46ae | 0x73c4 | 0x45bc))
            .count();
        if uid_count == 0 {
            return Ok(());
        }
        let workspace = uid_count
            .checked_mul(size_of::<u64>())
            .ok_or(Error::Overflow)?;
        if self
            .retained_bytes()
            .checked_add(workspace)
            .ok_or(Error::Overflow)?
            > storage
        {
            return Err(Error::Limit("metadata validation storage"));
        }
        let mut uids = Vec::new();
        uids.try_reserve_exact(uid_count)
            .map_err(|_| Error::Limit("metadata UID allocation"))?;
        for kind in [0x46ae, 0x73c4, 0x45bc] {
            uids.clear();
            for e in self.elements().filter(|e| e.id() == kind) {
                uids.push(e.uint()?);
            }
            uids.sort_unstable();
            if uids.windows(2).any(|p| p[0] == p[1]) {
                return Err(Error::Invalid("duplicate metadata UID"));
            }
        }
        Ok(())
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
#[derive(Default)]
pub(crate) struct Catalogue {
    documents: Vec<Arc<Document>>,
    retained: usize,
    nodes: usize,
    attachments: usize,
}
impl Catalogue {
    pub fn documents(&self) -> &[Arc<Document>] {
        &self.documents
    }
    pub fn retained_bytes(&self) -> usize {
        self.retained + self.documents.capacity() * size_of::<Arc<Document>>()
    }
    pub fn attachment_bytes(&self) -> usize {
        self.attachments
    }
    pub fn prepare(&mut self, limits: crate::Limits) -> Result<usize> {
        if self.documents.len() >= 256 {
            return Err(Error::Limit("metadata documents"));
        }
        if self.documents.len() == self.documents.capacity() {
            let pointers = (self.documents.len() + 1)
                .checked_mul(size_of::<Arc<Document>>())
                .ok_or(Error::Overflow)?;
            if self.retained.checked_add(pointers).ok_or(Error::Overflow)?
                > limits.metadata_storage_bytes
            {
                return Err(Error::Limit("metadata retained storage"));
            }
            self.documents
                .try_reserve_exact(1)
                .map_err(|_| Error::Limit("metadata document allocation"))?;
        }
        limits
            .metadata_storage_bytes
            .checked_sub(self.retained_bytes())
            .ok_or(Error::Limit("metadata retained storage"))
    }
    pub fn add(
        &mut self,
        kind: u64,
        raw: Vec<u8>,
        structural_limit: usize,
        source_offset: Option<u64>,
        limits: crate::Limits,
    ) -> Result<usize> {
        if kind == ATTACHMENTS && self.documents.iter().any(|d| d.kind() == ATTACHMENTS) {
            return Err(Error::Invalid("duplicate Attachments"));
        }
        let available = self.prepare(limits)?;
        let mut doc = Document::parse(
            kind,
            raw,
            limits
                .metadata_nodes
                .checked_sub(self.nodes)
                .ok_or(Error::Limit("metadata nodes"))?,
            limits.metadata_depth,
            available,
            limits
                .attachment_bytes
                .checked_sub(self.attachments)
                .ok_or(Error::Limit("attachment payload bytes"))?,
        )?;
        doc.source_offset = source_offset;
        let structural = doc.raw.len() - doc.attachment_bytes;
        if structural > structural_limit {
            return Err(Error::Limit("metadata"));
        }
        self.nodes = self
            .nodes
            .checked_add(doc.node_count())
            .ok_or(Error::Overflow)?;
        self.attachments = self
            .attachments
            .checked_add(doc.attachment_bytes)
            .ok_or(Error::Overflow)?;
        self.retained = self
            .retained
            .checked_add(doc.retained_bytes())
            .ok_or(Error::Overflow)?;
        self.documents.push(Arc::new(doc));
        Ok(structural)
    }
}

#[path = "metadata_views.rs"]
mod views;
pub use views::{Chapter, ChapterDisplay, Edition, SegmentInfo, SimpleTag, Tag, Targets};

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
