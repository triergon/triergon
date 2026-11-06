//! Intended child of metadata. Builds the existing twelve-byte Node arena and
//! opaque SharedDocument; every source/node/validation pass really resumes.
use super::{
    ATTACHMENTS, CHAPTERS, Catalogue, Document, INFO, Node, SharedDocument, TAGS, ascii, boolean,
    default_uint, master, unsigned, utf8,
};
use crate::matroska::typed::syntax::Required;
use crate::matroska::typed::syntax::{ChildCursor, check};
use crate::{
    Error, Result, ebml,
    incremental::ParseStep,
    storage::{SharedBytes, StorageVec, text_job::TextJob},
};
use std::{
    mem::size_of,
    ops::Range,
    sync::{Arc, atomic::AtomicBool},
};
use triergon_core::{Budget, BudgetVec, Reservation, WorkBudget};

#[derive(Debug)]
enum Frame {
    Parse {
        node: u16,
        cursor: ChildCursor,
        depth: u8,
    },
    // Parsing has finished before these contexts reuse the same fixed slots.
    // The bounds summarize chapter ancestors only up to the nearest Edition.
    Validate {
        end: u16,
        lower: u64,
        upper: u64,
        ordered: bool,
    },
}
#[derive(Debug, Clone, Copy, Default)]
struct Fields {
    presence: u64,
    singles: u64,
    start: Option<u64>,
    end: Option<u64>,
    ordered: Option<u64>,
}
fn bit(id: u64) -> u64 {
    let index = match id {
        0x61a7 => 0,
        0x45b9 => 1,
        0x7373 => 2,
        0x46ae => 3,
        0x466e => 4,
        0x4660 => 5,
        0x465c => 6,
        0xb6 => 7,
        0x73c4 => 8,
        0x91 => 9,
        0x92 => 10,
        0x6ebc => 11,
        0x6e67 => 12,
        0x85 => 13,
        0x67c8 => 14,
        0x45a3 => 15,
        0x4487 => 16,
        0x4485 => 17,
        0x6922 => 18,
        0x6933 => 19,
        _ => return 0,
    };
    1 << index
}
fn singles(id: u64) -> &'static [u64] {
    match id {
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
    }
}
fn require(fields: Fields, ids: &[u64]) -> Result<()> {
    if ids.iter().any(|id| fields.presence & bit(*id) == 0) {
        Err(Error::Invalid("missing metadata field"))
    } else {
        Ok(())
    }
}
#[derive(Debug)]
enum Phase {
    Owner,
    NodeReserve,
    NodeCopy,
    NodeAppend,
    NodeKind,
    Text,
    Parse,
    ValidateStart,
    ValidateChildren,
    ValidateRules,
    ValidateContext,
    UidReserve,
    UidFill,
    UidSort,
    UidCheck,
    Finish,
    Done,
}
#[derive(Debug)]
pub(crate) struct DocumentJob {
    raw: SharedBytes,
    pool: Budget,
    node_limit: usize,
    depth_limit: usize,
    storage_limit: usize,
    attachment_limit: usize,
    source_offset: Option<u64>,
    nodes: BudgetVec<Node>,
    owner: Option<Reservation>,
    growth: Option<(BudgetVec<Node>, usize)>,
    pending: Option<Node>,
    pending_range: Range<usize>,
    pending_index: u16,
    stack: [Option<Frame>; 65],
    stack_len: usize,
    attachment_bytes: usize,
    text: Option<TextJob>,
    phase: Phase,
    validate: usize,
    child: usize,
    child_end: usize,
    fields: Fields,
    uid_count: usize,
    uid_kinds: u8,
    uids: BudgetVec<u64>,
    uid_kind: usize,
    uid_at: usize,
    heap: Option<Heap>,
    timestamp_scale: Option<u64>,
    result: Option<SharedDocument>,
    failed: bool,
}
/// The four existing document resource policies travel as one explicit record.
/// Source identity/location and retained storage still use the caller's pool.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DocumentLimits {
    pub node_limit: usize,
    pub depth_limit: usize,
    pub storage_limit: usize,
    pub attachment_limit: usize,
}
impl DocumentJob {
    pub fn new(
        kind: u64,
        raw: SharedBytes,
        limits: DocumentLimits,
        source_offset: Option<u64>,
        pool: &Budget,
    ) -> Result<Self> {
        let DocumentLimits {
            node_limit,
            depth_limit,
            storage_limit,
            attachment_limit,
        } = limits;
        if !matches!(kind, INFO | ATTACHMENTS | CHAPTERS | TAGS) {
            return Err(Error::Invalid("metadata document type"));
        }
        if !raw.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool);
        }
        u32::try_from(raw.len()).map_err(|_| Error::Policy("metadata offset space"))?;
        let end = raw.len();
        Ok(Self {
            raw,
            pool: pool.clone(),
            node_limit: node_limit.min(u16::MAX as usize),
            depth_limit: depth_limit.min(64),
            storage_limit,
            attachment_limit,
            source_offset,
            nodes: BudgetVec::new(pool),
            owner: None,
            growth: None,
            pending: Some(Node {
                id: kind as u32,
                header: 0,
                next: 1,
                parent: u16::MAX,
            }),
            pending_range: 0..end,
            pending_index: 0,
            stack: std::array::from_fn(|_| None),
            stack_len: 0,
            attachment_bytes: 0,
            text: None,
            phase: Phase::Owner,
            validate: 0,
            child: 0,
            child_end: 0,
            fields: Fields::default(),
            uid_count: 0,
            uid_kinds: 0,
            uids: BudgetVec::new(pool),
            uid_kind: 0,
            uid_at: 0,
            heap: None,
            timestamp_scale: None,
            result: None,
            failed: false,
        })
    }
    pub fn timestamp_scale(&self) -> Option<u64> {
        self.timestamp_scale
    }
    fn retained_with_capacity(&self, capacity: usize) -> Result<usize> {
        capacity
            .checked_mul(size_of::<Node>())
            .and_then(|n| n.checked_add(self.raw.retained_capacity()))
            .and_then(|n| n.checked_add(size_of::<Document>() + 2 * size_of::<usize>()))
            .and_then(|n| n.checked_add(self.raw.owner_overhead_bound()))
            .ok_or(Error::Overflow)
    }
    fn range(&self, index: usize) -> Result<Range<usize>> {
        if index == 0 {
            return Ok(0..self.raw.len());
        }
        let node = self
            .nodes
            .as_slice()
            .get(index)
            .required("missing metadata node")?;
        let at = node.header as usize;
        let (_, a, _) = ebml::vint(&self.raw[at..], true)?;
        let (size, b, unknown) = ebml::vint(&self.raw[at + a..], false)?;
        if unknown {
            return Err(Error::Invalid("unknown metadata child size"));
        }
        let start = at.checked_add(a + b).ok_or(Error::Overflow)?;
        let end = start
            .checked_add(usize::try_from(size).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        if end > self.raw.len() {
            return Err(Error::Truncated);
        }
        Ok(start..end)
    }
    fn uint(&self, index: usize) -> Result<u64> {
        let raw = &self.raw[self.range(index)?];
        if raw.is_empty() {
            Ok(default_uint(u64::from(self.nodes.as_slice()[index].id)))
        } else {
            ebml::uint(raw)
        }
    }
    fn collect(&self, parent: usize, child: usize, fields: &mut Fields) -> Result<()> {
        let id = u64::from(self.nodes.as_slice()[child].id);
        let unique = singles(u64::from(self.nodes.as_slice()[parent].id));
        if let Some(bit) = unique.iter().position(|value| *value == id) {
            let mask = 1 << bit;
            if fields.singles & mask != 0 {
                return Err(Error::Invalid("duplicate metadata field"));
            }
            fields.singles |= mask;
        }
        fields.presence |= bit(id);
        match id {
            0x91 => fields.start = Some(self.uint(child)?),
            0x92 => fields.end = Some(self.uint(child)?),
            0x45dd => fields.ordered = Some(self.uint(child)?),
            _ => (),
        }
        Ok(())
    }
    fn next_validation(&mut self) {
        self.validate += 1;
        self.phase = Phase::ValidateStart;
    }
    fn validation_context(&self) -> Result<(u64, u64, bool)> {
        if self.stack_len == 0 {
            return Ok((0, u64::MAX, false));
        }
        let Some(Frame::Validate {
            lower,
            upper,
            ordered,
            ..
        }) = self.stack[self.stack_len - 1].as_ref()
        else {
            return Err(Error::Invalid("missing metadata validation context"));
        };
        Ok((*lower, *upper, *ordered))
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedDocument>> {
        if self.failed {
            return Err(Error::Invalid(
                "metadata job requires replacement after error",
            ));
        }
        let result = self.advance(work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedDocument>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Owner => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.owner = Some(
                        self.pool
                            .reserve(size_of::<Document>() + 2 * size_of::<usize>())?,
                    );
                    self.phase = Phase::NodeReserve;
                }
                Phase::NodeReserve => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.nodes.len() >= self.node_limit {
                        return Err(Error::Policy("metadata nodes"));
                    }
                    if self.nodes.len() < self.nodes.capacity() {
                        self.phase = Phase::NodeAppend;
                        continue;
                    }
                    let initial = (self.raw.len() / 2 + 1).min(8);
                    let target = self
                        .nodes
                        .len()
                        .saturating_mul(2)
                        .max(initial)
                        .min(self.node_limit);
                    let lower = target
                        .checked_mul(size_of::<Node>())
                        .and_then(|n| n.checked_add(self.raw.retained_capacity()))
                        .and_then(|n| n.checked_add(size_of::<Document>() + 2 * size_of::<usize>()))
                        .ok_or(Error::Overflow)?;
                    if lower > self.storage_limit {
                        return Err(Error::Policy("metadata retained storage"));
                    }
                    let mut replacement = BudgetVec::new(&self.pool);
                    replacement.try_reserve_exact(target)?;
                    if self.retained_with_capacity(replacement.capacity())? > self.storage_limit {
                        return Err(Error::Policy("metadata retained storage"));
                    }
                    self.growth = Some((replacement, 0));
                    self.phase = Phase::NodeCopy;
                }
                Phase::NodeCopy => {
                    let growth = self
                        .growth
                        .as_mut()
                        .required("missing metadata arena growth")?;
                    if growth.1 == self.nodes.len() {
                        let (replacement, _) = self
                            .growth
                            .take()
                            .required("missing metadata arena growth")?;
                        self.nodes = replacement;
                        self.phase = Phase::NodeAppend;
                        continue;
                    }
                    let count = work.take((self.nodes.len() - growth.1).min(64) as u64) as usize;
                    if count == 0 {
                        return Ok(ParseStep::Yield);
                    }
                    growth.0.try_extend_from_slice(
                        &self.nodes.as_slice()[growth.1..growth.1 + count],
                    )?;
                    growth.1 += count;
                }
                Phase::NodeAppend => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let node = self
                        .pending
                        .take()
                        .required("missing metadata node admission")?;
                    self.pending_index = u16::try_from(self.nodes.len())
                        .map_err(|_| Error::Policy("metadata index space"))?;
                    if let Err(error) = self.nodes.try_push(node) {
                        self.pending = Some(error.value);
                        return Err(error.error);
                    }
                    self.phase = Phase::NodeKind;
                }
                Phase::NodeKind => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let index = usize::from(self.pending_index);
                    let id = u64::from(self.nodes.as_slice()[index].id);
                    if master(id) {
                        let depth = if self.stack_len == 0 {
                            0
                        } else {
                            let Some(Frame::Parse { depth, .. }) =
                                self.stack[self.stack_len - 1].as_ref()
                            else {
                                return Err(Error::Invalid("missing metadata parent"));
                            };
                            usize::from(*depth) + 1
                        };
                        if depth > self.depth_limit {
                            return Err(Error::Policy("metadata nesting"));
                        }
                        self.stack[self.stack_len] = Some(Frame::Parse {
                            node: self.pending_index,
                            cursor: ChildCursor::new(self.pending_range.clone()),
                            depth: u8::try_from(depth).map_err(|_| Error::Overflow)?,
                        });
                        self.stack_len += 1;
                        self.phase = Phase::Parse;
                        continue;
                    }
                    let raw = &self.raw[self.pending_range.clone()];
                    if unsigned(id) {
                        let value = if raw.is_empty() {
                            default_uint(id)
                        } else {
                            ebml::uint(raw)?
                        };
                        if id == 0x2ad7b1
                            && self.nodes.as_slice()[index].parent == 0
                            && u64::from(self.nodes.as_slice()[0].id) == INFO
                        {
                            self.timestamp_scale = Some(value);
                        }
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
                    // Every scalar is visited here once. Count UID namespaces
                    // now, while retaining the original post-validation
                    // workspace admission and complete duplicate checks.
                    let uid_kind = match id {
                        0x46ae => 1,
                        0x73c4 => 2,
                        0x45bc => 4,
                        _ => 0,
                    };
                    if uid_kind != 0 {
                        self.uid_count = self.uid_count.checked_add(1).ok_or(Error::Overflow)?;
                        self.uid_kinds |= uid_kind;
                    }
                    if id == 0x4489 {
                        let value = match raw.len() {
                            0 => 0.,
                            4 => f64::from(f32::from_be_bytes(
                                raw.try_into()
                                    .map_err(|_| Error::Invalid("metadata float width"))?,
                            )),
                            8 => f64::from_be_bytes(
                                raw.try_into()
                                    .map_err(|_| Error::Invalid("metadata float width"))?,
                            ),
                            _ => return Err(Error::Invalid("metadata float width")),
                        };
                        if !value.is_finite() || value <= 0. {
                            return Err(Error::Invalid("Segment duration range"));
                        }
                    }
                    if id == 0x4461 && !matches!(raw.len(), 0 | 8) {
                        return Err(Error::Invalid("DateUTC width"));
                    }
                    if id == 0x6e67 && raw.len() != 16 {
                        return Err(Error::Invalid("chapter SegmentUUID length"));
                    }
                    if id == 0x465c {
                        self.attachment_bytes = self
                            .attachment_bytes
                            .checked_add(raw.len())
                            .ok_or(Error::Overflow)?;
                        if self.attachment_bytes > self.attachment_limit {
                            return Err(Error::Policy("attachment payload bytes"));
                        }
                    }
                    if utf8(id) || ascii(id) {
                        self.text = Some(TextJob::new(
                            self.raw.clone(),
                            self.pending_range.clone(),
                            ascii(id),
                        )?);
                        self.phase = Phase::Text;
                    } else {
                        let next = u16::try_from(self.nodes.len()).map_err(|_| Error::Overflow)?;
                        self.nodes.as_mut_slice()[index].next = next;
                        self.phase = Phase::Parse;
                    }
                }
                Phase::Text => match self
                    .text
                    .as_mut()
                    .required("missing metadata text job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(_) => {
                        self.text = None;
                        let next = u16::try_from(self.nodes.len()).map_err(|_| Error::Overflow)?;
                        self.nodes.as_mut_slice()[usize::from(self.pending_index)].next = next;
                        self.phase = Phase::Parse;
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                Phase::Parse => {
                    let Some(Frame::Parse { node, cursor, .. }) =
                        self.stack[self.stack_len - 1].as_mut()
                    else {
                        return Err(Error::Invalid("missing metadata parse frame"));
                    };
                    match cursor.step(&self.raw, work, cancel) {
                        Err(Error::Invalid("unknown size inside metadata")) => {
                            return Err(Error::Invalid("unknown metadata child size"));
                        }
                        Err(error) => return Err(error),
                        Ok(ParseStep::Yield) => return Ok(ParseStep::Yield),
                        Ok(ParseStep::Event(None)) => {
                            if !work.try_charge(1) {
                                return Ok(ParseStep::Yield);
                            }
                            let node = *node;
                            let next =
                                u16::try_from(self.nodes.len()).map_err(|_| Error::Overflow)?;
                            self.nodes.as_mut_slice()[usize::from(node)].next = next;
                            self.stack_len -= 1;
                            self.stack[self.stack_len] = None;
                            if self.stack_len == 0 {
                                self.phase = Phase::ValidateStart;
                            }
                        }
                        Ok(ParseStep::Event(Some(child))) => {
                            self.pending = Some(Node {
                                id: child.id as u32,
                                header: child.header as u32,
                                next: 0,
                                parent: *node,
                            });
                            self.pending_range = child.payload;
                            self.phase = Phase::NodeReserve;
                        }
                        _ => return Err(Error::Invalid("invalid metadata parse transition")),
                    }
                }
                Phase::ValidateStart => {
                    if self.validate == self.nodes.len() {
                        self.uid_at = 0;
                        self.phase = if self.uid_count == 0 {
                            Phase::Finish
                        } else {
                            Phase::UidReserve
                        };
                        continue;
                    }
                    if self.stack_len != 0 {
                        let Some(Frame::Validate { end, .. }) =
                            self.stack[self.stack_len - 1].as_ref()
                        else {
                            return Err(Error::Invalid("missing metadata validation context"));
                        };
                        if usize::from(*end) <= self.validate {
                            if !work.try_charge(1) {
                                return Ok(ParseStep::Yield);
                            }
                            self.stack_len -= 1;
                            self.stack[self.stack_len] = None;
                            continue;
                        }
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    // Scalar and opaque nodes have no parsed children or
                    // parent-field rules. NodeKind has already checked every
                    // scalar value. Keep one charged visit, the context-pop
                    // order above and the later complete UID validation.
                    if !master(u64::from(self.nodes.as_slice()[self.validate].id)) {
                        self.validate += 1;
                        continue;
                    }
                    self.fields = Fields::default();
                    self.child = self.validate + 1;
                    self.child_end = usize::from(self.nodes.as_slice()[self.validate].next);
                    self.phase = Phase::ValidateChildren;
                }
                Phase::ValidateChildren => {
                    // One charged visit and one cancellation check per child, as
                    // one outer iteration per child would do, without re-dispatch.
                    let mut fields = self.fields;
                    let mut first = true;
                    while self.child < self.child_end {
                        if !first {
                            check(cancel)?;
                        }
                        first = false;
                        if !work.try_charge(1) {
                            self.fields = fields;
                            return Ok(ParseStep::Yield);
                        }
                        let result = self.collect(self.validate, self.child, &mut fields);
                        self.fields = fields;
                        result?;
                        self.child = usize::from(self.nodes.as_slice()[self.child].next);
                    }
                    if !first {
                        check(cancel)?;
                    }
                    self.phase = Phase::ValidateRules;
                }
                Phase::ValidateRules => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let id = u64::from(self.nodes.as_slice()[self.validate].id);
                    let fields = self.fields;
                    match id {
                        ATTACHMENTS => require(fields, &[0x61a7])?,
                        CHAPTERS => require(fields, &[0x45b9])?,
                        TAGS => require(fields, &[0x7373])?,
                        0x61a7 => require(fields, &[0x46ae, 0x466e, 0x4660, 0x465c])?,
                        0x45b9 => require(fields, &[0xb6])?,
                        0xb6 => {
                            require(fields, &[0x73c4, 0x91])?;
                            let start = fields.start.required("chapter start")?;
                            if fields.end.is_some_and(|end| end < start) {
                                return Err(Error::Invalid("chapter time range"));
                            }
                            if fields.presence & bit(0x6ebc) != 0 {
                                require(fields, &[0x6e67])?;
                            }
                            let (lower, upper, ordered) = self.validation_context()?;
                            if start < lower || start > upper {
                                return Err(Error::Invalid("nested chapter time range"));
                            }
                            if ordered && fields.presence & bit(0xb6) == 0 {
                                require(fields, &[0x92])?;
                            }
                        }
                        0x80 => require(fields, &[0x85])?,
                        0x7373 => require(fields, &[0x67c8])?,
                        0x67c8 => {
                            require(fields, &[0x45a3])?;
                            if fields.presence & bit(0x4487) != 0
                                && fields.presence & bit(0x4485) != 0
                            {
                                return Err(Error::Invalid("text and binary tag"));
                            }
                        }
                        0x6911 => require(fields, &[0x6922, 0x6933])?,
                        _ => (),
                    }
                    if matches!(id, 0x45b9 | 0xb6)
                        && usize::from(self.nodes.as_slice()[self.validate].next)
                            > self.validate + 1
                    {
                        self.phase = Phase::ValidateContext;
                    } else {
                        self.next_validation();
                    }
                }
                Phase::ValidateContext => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let node = self.nodes.as_slice()[self.validate];
                    let (lower, upper, ordered) = if u64::from(node.id) == 0x45b9 {
                        (0, u64::MAX, self.fields.ordered == Some(1))
                    } else {
                        let (lower, upper, ordered) = self.validation_context()?;
                        (
                            lower.max(self.fields.start.required("chapter start")?),
                            upper.min(self.fields.end.unwrap_or(u64::MAX)),
                            ordered,
                        )
                    };
                    if self.stack_len == self.stack.len() {
                        return Err(Error::Policy("metadata nesting"));
                    }
                    self.stack[self.stack_len] = Some(Frame::Validate {
                        end: node.next,
                        lower,
                        upper,
                        ordered,
                    });
                    self.stack_len += 1;
                    self.next_validation();
                }
                Phase::UidReserve => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let workspace = self
                        .uid_count
                        .checked_mul(size_of::<u64>())
                        .ok_or(Error::Overflow)?;
                    if self
                        .retained_with_capacity(self.nodes.capacity())?
                        .checked_add(workspace)
                        .ok_or(Error::Overflow)?
                        > self.storage_limit
                    {
                        return Err(Error::Policy("metadata validation storage"));
                    }
                    self.uids.try_reserve_exact(self.uid_count)?;
                    // The completed count proves which independent UID namespaces
                    // are populated. Keep the original full-u64 arena and kind
                    // order; absent kinds need no second walk of every node.
                    self.uid_kind = self.uid_kinds.trailing_zeros() as usize;
                    self.uid_at = 0;
                    self.phase = Phase::UidFill;
                }
                Phase::UidFill => {
                    // One charged visit and one cancellation check per node, as
                    // one outer iteration per node would do, without re-dispatch.
                    let kind = [0x46ae, 0x73c4, 0x45bc][self.uid_kind];
                    let mut first = true;
                    while self.uid_at < self.nodes.len() {
                        if !first {
                            check(cancel)?;
                        }
                        first = false;
                        if !work.try_charge(1) {
                            return Ok(ParseStep::Yield);
                        }
                        if u64::from(self.nodes.as_slice()[self.uid_at].id) == kind {
                            let value = self.uint(self.uid_at)?;
                            self.uids.try_push(value).map_err(|e| e.error)?;
                        }
                        self.uid_at += 1;
                    }
                    if !first {
                        check(cancel)?;
                    }
                    self.heap = Some(Heap::new(self.uids.len()));
                    self.phase = Phase::UidSort;
                }
                Phase::UidSort => {
                    if !self.heap.as_mut().required("missing UID sort job")?.step(
                        self.uids.as_mut_slice(),
                        work,
                        cancel,
                    )? {
                        return Ok(ParseStep::Yield);
                    }
                    self.uid_at = 0;
                    self.phase = Phase::UidCheck;
                }
                Phase::UidCheck => {
                    if self.uid_at + 1 >= self.uids.len() {
                        self.uid_kinds &= !(1_u8 << self.uid_kind);
                        if self.uid_kinds == 0 {
                            self.uids = BudgetVec::new(&self.pool);
                            self.heap = None;
                            self.phase = Phase::Finish;
                        } else {
                            self.uid_kind = self.uid_kinds.trailing_zeros() as usize;
                            self.uid_at = 0;
                            self.uids.clear();
                            self.heap = None;
                            self.phase = Phase::UidFill;
                        }
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.uids.as_slice()[self.uid_at] == self.uids.as_slice()[self.uid_at + 1] {
                        return Err(Error::Invalid("duplicate metadata UID"));
                    }
                    self.uid_at += 1;
                }
                Phase::Finish => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let nodes = std::mem::replace(&mut self.nodes, BudgetVec::new(&self.pool));
                    let charge = self
                        .owner
                        .take()
                        .required("missing metadata owner reservation")?;
                    let document = Document {
                        raw: self.raw.clone(),
                        nodes: StorageVec(nodes),
                        attachment_bytes: self.attachment_bytes,
                        source_offset: self.source_offset,
                        _pool: self.pool.clone(),
                        _owner_charge: charge,
                    };
                    self.result = Some(SharedDocument(Arc::new(document)));
                    self.phase = Phase::Done;
                }
                Phase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing completed metadata document")?
                            .clone(),
                    ));
                }
            }
        }
    }
}

/// Own specification-level heapsort of scalar UID workspace. No sorted order is
/// exported: it proves duplicate rejection only. At most one bounded heap move
/// and two comparisons occur per work unit; no std whole-vector sort is hidden.
#[derive(Debug, Clone, Copy)]
enum HeapPhase {
    Build,
    Sift {
        root: usize,
        end: usize,
        building: bool,
    },
    Extract,
    Done,
}
#[derive(Debug)]
pub(crate) struct Heap {
    build: usize,
    end: usize,
    phase: HeapPhase,
}
impl Heap {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            build: len / 2,
            end: len,
            phase: HeapPhase::Build,
        }
    }
    pub(crate) fn step(
        &mut self,
        values: &mut [u64],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            check(cancel)?;
            match self.phase {
                HeapPhase::Build => {
                    if self.build == 0 {
                        self.phase = HeapPhase::Extract;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(false);
                    }
                    self.build -= 1;
                    self.phase = HeapPhase::Sift {
                        root: self.build,
                        end: values.len(),
                        building: true,
                    };
                }
                HeapPhase::Sift {
                    root,
                    end,
                    building,
                } => {
                    let left = root
                        .checked_mul(2)
                        .and_then(|n| n.checked_add(1))
                        .ok_or(Error::Overflow)?;
                    if left >= end {
                        self.phase = if building {
                            HeapPhase::Build
                        } else {
                            HeapPhase::Extract
                        };
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(false);
                    }
                    let child = if left + 1 < end && values[left + 1] > values[left] {
                        left + 1
                    } else {
                        left
                    };
                    if values[child] > values[root] {
                        values.swap(root, child);
                        self.phase = HeapPhase::Sift {
                            root: child,
                            end,
                            building,
                        };
                    } else {
                        self.phase = if building {
                            HeapPhase::Build
                        } else {
                            HeapPhase::Extract
                        };
                    }
                }
                HeapPhase::Extract => {
                    if self.end <= 1 {
                        self.phase = HeapPhase::Done;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(false);
                    }
                    self.end -= 1;
                    values.swap(0, self.end);
                    self.phase = HeapPhase::Sift {
                        root: 0,
                        end: self.end,
                        building: false,
                    };
                }
                HeapPhase::Done => return Ok(true),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CatalogueCommit {
    pub document: SharedDocument,
    pub structural_bytes: usize,
    pub timestamp_scale: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct CataloguePrepareJob {
    replacement: Option<BudgetVec<SharedDocument>>,
    at: usize,
    ready: bool,
}
impl CataloguePrepareJob {
    pub fn new() -> Self {
        Self {
            replacement: None,
            at: 0,
            ready: false,
        }
    }
    pub fn step(
        &mut self,
        catalogue: &mut Catalogue,
        limits: crate::Limits,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<usize>> {
        loop {
            check(cancel)?;
            if self.ready {
                return Ok(ParseStep::Event(
                    limits
                        .metadata_storage_bytes
                        .checked_sub(catalogue.retained_bytes())
                        .ok_or(Error::Policy("metadata retained storage"))?,
                ));
            }
            if let Some(replacement) = &mut self.replacement {
                if self.at == catalogue.documents.len() {
                    catalogue.documents = StorageVec(
                        self.replacement
                            .take()
                            .required("missing prepared catalogue growth")?,
                    );
                    self.ready = true;
                    continue;
                }
                if !work.try_charge(1) {
                    return Ok(ParseStep::Yield);
                }
                replacement
                    .try_push(catalogue.documents[self.at].clone())
                    .map_err(|e| e.error)?;
                self.at += 1;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            if catalogue.documents.len() >= 256 {
                return Err(Error::Policy("metadata documents"));
            }
            if catalogue.documents.len() < catalogue.documents.capacity() {
                self.ready = true;
                continue;
            }
            let target = catalogue
                .documents
                .len()
                .checked_add(1)
                .ok_or(Error::Overflow)?;
            let pointers = target
                .checked_mul(size_of::<SharedDocument>())
                .ok_or(Error::Overflow)?;
            if catalogue
                .retained
                .checked_add(pointers)
                .ok_or(Error::Overflow)?
                > limits.metadata_storage_bytes
            {
                return Err(Error::Policy("metadata retained storage"));
            }
            let mut replacement = BudgetVec::new(&catalogue.pool);
            replacement.try_reserve_exact(target)?;
            self.replacement = Some(replacement);
        }
    }
}
#[derive(Debug)]
enum CommitPhase {
    Duplicate,
    Prepare,
    Copy,
    Parse,
    Commit,
    Done,
}
#[derive(Debug)]
pub(crate) struct CatalogueJob {
    kind: u64,
    raw: SharedBytes,
    structural_limit: usize,
    source_offset: Option<u64>,
    limits: crate::Limits,
    pool: Budget,
    phase: CommitPhase,
    at: usize,
    replacement: Option<BudgetVec<SharedDocument>>,
    job: Option<DocumentJob>,
    ready: Option<SharedDocument>,
    timestamp_scale: Option<u64>,
    result: Option<CatalogueCommit>,
    failed: bool,
}
impl CatalogueJob {
    pub fn new(
        kind: u64,
        raw: SharedBytes,
        structural_limit: usize,
        source_offset: Option<u64>,
        limits: crate::Limits,
        pool: &Budget,
    ) -> Result<Self> {
        if !raw.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool);
        }
        Ok(Self {
            kind,
            raw,
            structural_limit,
            source_offset,
            limits,
            pool: pool.clone(),
            phase: CommitPhase::Duplicate,
            at: 0,
            replacement: None,
            job: None,
            ready: None,
            timestamp_scale: None,
            result: None,
            failed: false,
        })
    }
    pub fn step(
        &mut self,
        catalogue: &mut Catalogue,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<CatalogueCommit>> {
        if self.failed {
            return Err(Error::Invalid(
                "catalogue job requires replacement after error",
            ));
        }
        if !catalogue.pool.same_pool(&self.pool) {
            return Err(triergon_core::Error::ForeignPool);
        }
        let result = self.advance(catalogue, work, cancel);
        match result {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        catalogue: &mut Catalogue,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<CatalogueCommit>> {
        loop {
            check(cancel)?;
            match self.phase {
                CommitPhase::Duplicate => {
                    if self.at == catalogue.documents.len() {
                        self.phase = CommitPhase::Prepare;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.kind == ATTACHMENTS
                        && catalogue.documents[self.at].kind() == ATTACHMENTS
                    {
                        return Err(Error::Invalid("duplicate Attachments"));
                    }
                    self.at += 1;
                }
                CommitPhase::Prepare => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if catalogue.documents.len() >= 256 {
                        return Err(Error::Policy("metadata documents"));
                    }
                    if catalogue.documents.len() == catalogue.documents.capacity() {
                        let target = catalogue
                            .documents
                            .len()
                            .checked_add(1)
                            .ok_or(Error::Overflow)?;
                        let pointers = target
                            .checked_mul(size_of::<SharedDocument>())
                            .ok_or(Error::Overflow)?;
                        if catalogue
                            .retained
                            .checked_add(pointers)
                            .ok_or(Error::Overflow)?
                            > self.limits.metadata_storage_bytes
                        {
                            return Err(Error::Policy("metadata retained storage"));
                        }
                        let mut replacement = BudgetVec::new(&self.pool);
                        replacement.try_reserve_exact(target)?;
                        self.replacement = Some(replacement);
                        self.at = 0;
                        self.phase = CommitPhase::Copy;
                        continue;
                    }
                    self.start_document(catalogue)?;
                }
                CommitPhase::Copy => {
                    if self.at == catalogue.documents.len() {
                        let replacement = self
                            .replacement
                            .take()
                            .required("missing catalogue growth")?;
                        // Old shared handles have fixed-size drop; document/data
                        // owners stay retained by the copied handles. At most the
                        // explicit 256-document policy can be present here.
                        catalogue.documents = StorageVec(replacement);
                        self.start_document(catalogue)?;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.replacement
                        .as_mut()
                        .required("missing catalogue growth")?
                        .try_push(catalogue.documents[self.at].clone())
                        .map_err(|e| e.error)?;
                    self.at += 1;
                }
                CommitPhase::Parse => match self
                    .job
                    .as_mut()
                    .required("missing catalogue document job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.timestamp_scale = self
                            .job
                            .as_ref()
                            .required("missing document scale state")?
                            .timestamp_scale();
                        self.ready = Some(value);
                        self.job = None;
                        self.phase = CommitPhase::Commit;
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                CommitPhase::Commit => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let document = self
                        .ready
                        .as_ref()
                        .required("missing validated catalogue document")?;
                    let structural_bytes = document
                        .raw
                        .len()
                        .checked_sub(document.attachment_bytes)
                        .ok_or(Error::Overflow)?;
                    if structural_bytes > self.structural_limit {
                        return Err(Error::Policy("metadata"));
                    }
                    let nodes = catalogue
                        .nodes
                        .checked_add(document.node_count())
                        .ok_or(Error::Overflow)?;
                    let attachments = catalogue
                        .attachments
                        .checked_add(document.attachment_bytes)
                        .ok_or(Error::Overflow)?;
                    let retained = catalogue
                        .retained
                        .checked_add(document.retained_bytes())
                        .ok_or(Error::Overflow)?;
                    catalogue.documents.push(document.clone())?;
                    catalogue.nodes = nodes;
                    catalogue.attachments = attachments;
                    catalogue.retained = retained;
                    let result = CatalogueCommit {
                        document: document.clone(),
                        structural_bytes,
                        timestamp_scale: self.timestamp_scale,
                    };
                    self.result = Some(result.clone());
                    self.ready = None;
                    self.phase = CommitPhase::Done;
                    return Ok(ParseStep::Event(result));
                }
                CommitPhase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing catalogue commit")?
                            .clone(),
                    ));
                }
            }
        }
    }
    fn start_document(&mut self, catalogue: &Catalogue) -> Result<()> {
        let storage = self
            .limits
            .metadata_storage_bytes
            .checked_sub(catalogue.retained_bytes())
            .ok_or(Error::Policy("metadata retained storage"))?;
        let nodes = self
            .limits
            .metadata_nodes
            .checked_sub(catalogue.nodes)
            .ok_or(Error::Policy("metadata nodes"))?;
        let attachments = self
            .limits
            .attachment_bytes
            .checked_sub(catalogue.attachments)
            .ok_or(Error::Policy("attachment payload bytes"))?;
        self.job = Some(DocumentJob::new(
            self.kind,
            self.raw.clone(),
            crate::metadata::typed_document::DocumentLimits {
                node_limit: nodes,
                depth_limit: self.limits.metadata_depth,
                storage_limit: storage,
                attachment_limit: attachments,
            },
            self.source_offset,
            &self.pool,
        )?);
        self.phase = CommitPhase::Parse;
        Ok(())
    }
}

impl SharedDocument {
    pub(crate) fn shares_owner_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn structure_storage_bytes(&self) -> Result<usize> {
        self.nodes
            .capacity()
            .checked_mul(size_of::<Node>())
            .and_then(|bytes| bytes.checked_add(self._owner_charge.bytes()))
            .ok_or(Error::Overflow)
    }
}
impl crate::matroska::typed::usage::Storage for SharedDocument {
    fn storage_slots(&self) -> Result<usize> {
        Ok(2)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        match index {
            0 => Ok(Some(crate::matroska::typed::usage::Owner::Document(
                self.clone(),
            ))),
            1 => crate::matroska::typed::usage::Storage::storage_owner(&self.raw, 0),
            _ => Ok(None),
        }
    }
}
crate::matroska::typed::usage::storage!(Catalogue, |s| [
    s.documents.0,
    crate::matroska::typed::usage::Documents(&s.documents)
]);
crate::matroska::typed::usage::storage!(DocumentJob, |s| [
    s.raw,
    s.nodes,
    s.owner,
    s.growth.as_ref().map(|(buffer, _)| buffer),
    s.uids,
    s.text,
    s.result
]);
crate::matroska::typed::usage::storage!(CataloguePrepareJob, |s| [
    s.replacement,
    crate::matroska::typed::usage::Documents(
        s.replacement.as_ref().map_or(&[][..], BudgetVec::as_slice)
    )
]);
crate::matroska::typed::usage::storage!(CatalogueCommit, |s| [s.document]);
crate::matroska::typed::usage::storage!(CatalogueJob, |s| [
    s.raw,
    s.replacement,
    crate::matroska::typed::usage::Documents(
        s.replacement.as_ref().map_or(&[][..], BudgetVec::as_slice)
    ),
    s.job,
    s.ready,
    s.result
]);
