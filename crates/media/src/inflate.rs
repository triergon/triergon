//! Resumable DEFLATE over one bounded immutable compressed packet. This source
//! uses the sibling RFC-authored bit reader; no native/reference source is used.
use super::{Bits, InflateLimits, adler32, header};
use crate::{Error, Result, incremental::ParseStep, storage::SharedBytes};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, WorkBudget};

#[derive(Clone, Copy, Debug)]
struct Position {
    at: usize,
    reservoir: u64,
    available: usize,
}
impl Position {
    fn new() -> Self {
        Self {
            at: 0,
            reservoir: 0,
            available: 0,
        }
    }
    fn reader<'a>(&self, data: &'a [u8]) -> Bits<'a> {
        Bits {
            data,
            at: self.at,
            reservoir: self.reservoir,
            available: self.available,
        }
    }
    fn save(&mut self, bits: &Bits<'_>) {
        *self = Self {
            at: bits.at,
            reservoir: bits.reservoir,
            available: bits.available,
        };
    }
}
#[derive(Debug)]
struct HuffmanTree<const SYMBOLS: usize, const QUICK: usize> {
    quick: [u16; QUICK],
    counts: [u16; 16],
    first: [u16; 16],
    offsets: [u16; 16],
    symbols: [u16; SYMBOLS],
    max: usize,
}
impl<const SYMBOLS: usize, const QUICK: usize> HuffmanTree<SYMBOLS, QUICK> {
    const QUICK_BITS: usize = QUICK.trailing_zeros() as usize;
    fn empty() -> Self {
        Self {
            quick: [0; QUICK],
            counts: [0; 16],
            first: [0; 16],
            offsets: [0; 16],
            symbols: [0; SYMBOLS],
            max: 0,
        }
    }
    #[inline(always)]
    fn symbol(&self, bits: &mut Bits<'_>) -> Result<usize> {
        // Short alphabets need only their actual prefix space. Longer codes
        // use this alphabet's bounded quick width plus canonical fallback.
        let entry = self.quick[bits.peek(self.quick_width()) as usize];
        if entry != 0 {
            // The table index may be padded; consuming the real width is
            // always checked. A short source never invents encoded bits.
            bits.take(usize::from(entry & 15))?;
            return Ok(usize::from(entry >> 4));
        }
        self.long_symbol(bits)
    }
    #[inline(always)]
    fn quick_width(&self) -> usize {
        self.max.min(Self::QUICK_BITS)
    }
    #[cold]
    #[inline(never)]
    fn long_symbol(&self, bits: &mut Bits<'_>) -> Result<usize> {
        let mut code = 0_u16;
        for width in 1..=self.max {
            code = (code << 1) | bits.take(1)? as u16;
            if let Some(index) = code.checked_sub(self.first[width]) {
                if index < self.counts[width] {
                    return Ok(usize::from(
                        self.symbols[usize::from(self.offsets[width] + index)],
                    ));
                }
            }
        }
        Err(Error::Invalid("invalid Huffman code"))
    }
}
// RFC 1951 alphabets: 288 fixed literal/length entries, 19 CL entries,
// 32 distance entries. Five quick bits for distances retain all longer
// codes through the canonical fallback, without a 512-row distance array.
type Tree = HuffmanTree<288, 512>;
type CodeTree = HuffmanTree<19, 128>;
type DistanceTree = HuffmanTree<32, 32>;

// RFC 1951 §3.2.6 fixed alphabets. Immutable tables require no per-frame
// construction, heap allocation or hidden runtime work. Include reserved
// literal 286/287 and distance 30/31 entries: the parser rejects them explicitly.
const fn reverse_width(value: u16, width: usize) -> usize {
    let mut reversed = 0_usize;
    let mut at = 0;
    while at < width {
        reversed = (reversed << 1) | (((value >> at) & 1) as usize);
        at += 1;
    }
    reversed
}
const fn fixed_tree<const SYMBOLS: usize, const QUICK: usize>(
    distances: bool,
) -> HuffmanTree<SYMBOLS, QUICK> {
    let count = if distances { 32 } else { 288 };
    let quick_limit = QUICK;
    let mut lengths = [0_u8; SYMBOLS];
    let mut tree = HuffmanTree {
        quick: [0; QUICK],
        counts: [0; 16],
        first: [0; 16],
        offsets: [0; 16],
        symbols: [0; SYMBOLS],
        max: if distances { 5 } else { 9 },
    };
    let mut at = 0;
    while at < count {
        let width = if distances {
            5
        } else if at < 144 {
            8
        } else if at < 256 {
            9
        } else if at < 280 {
            7
        } else {
            8
        };
        lengths[at] = width;
        tree.counts[width as usize] += 1;
        at += 1;
    }
    let mut code = 0_u16;
    let mut offset = 0_u16;
    at = 1;
    while at <= 15 {
        code = (code + tree.counts[at - 1]) << 1;
        tree.first[at] = code;
        tree.offsets[at] = offset;
        offset += tree.counts[at];
        at += 1;
    }
    let mut next = tree.first;
    let mut positions = tree.offsets;
    at = 0;
    while at < count {
        let width = lengths[at] as usize;
        tree.symbols[positions[width] as usize] = at as u16;
        positions[width] += 1;
        let mut key = reverse_width(next[width], width);
        let step = 1_usize << width;
        let entry = ((at as u16) << 4) | width as u16;
        while key < quick_limit {
            tree.quick[key] = entry;
            key += step;
        }
        next[width] += 1;
        at += 1;
    }
    tree
}
static FIXED_LITERAL: Tree = fixed_tree(false);
static FIXED_DISTANCE: DistanceTree = fixed_tree(true);

/// Small construction cursor for a freshly cleared destination. Its table
/// stays in that destination and becomes readable only after advance completes.
#[derive(Debug)]
struct Build {
    stage: u8,
    at: usize,
    available: i32,
    code: u16,
    offset: u16,
    positions: [u16; 16],
    next_codes: [u16; 16],
    pending_key: usize,
    pending_stride: usize,
    pending_entry: u16,
    code_lengths: bool,
}
impl Build {
    fn new(code_lengths: bool) -> Self {
        Self {
            stage: 0,
            at: 0,
            available: 1,
            code: 0,
            offset: 0,
            positions: [0; 16],
            next_codes: [0; 16],
            pending_key: 0,
            pending_stride: 0,
            pending_entry: 0,
            code_lengths,
        }
    }
    /// One alphabet entry/table row per unit. Even a one-unit caller progresses.
    fn advance<const SYMBOLS: usize, const QUICK: usize>(
        &mut self,
        tree: &mut HuffmanTree<SYMBOLS, QUICK>,
        lengths: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            match self.stage {
                0 if self.at < lengths.len() => {
                    let count = (lengths.len() - self.at)
                        .min(work.remaining().saturating_add(1).min(32) as usize);
                    if !work.try_charge((count - 1) as u64) {
                        return Err(Error::Invalid("table work accounting"));
                    }
                    for &length in &lengths[self.at..self.at + count] {
                        let width = usize::from(length);
                        if width > 15 {
                            return Err(Error::Invalid("Huffman length"));
                        }
                        if width != 0 {
                            tree.counts[width] += 1;
                            tree.max = tree.max.max(width);
                        }
                    }
                    self.at += count;
                }
                0 => {
                    self.stage = 1;
                    self.at = 1;
                }
                1 if self.at <= 15 => {
                    self.available = self.available * 2 - i32::from(tree.counts[self.at]);
                    if self.available < 0 {
                        return Err(Error::Invalid("oversubscribed Huffman tree"));
                    }
                    self.at += 1;
                }
                1 => {
                    if self.available != 0 && (self.code_lengths || tree.max != 1) && tree.max != 0
                    {
                        return Err(Error::Invalid("incomplete Huffman tree"));
                    }
                    if self.code_lengths && tree.max == 0 {
                        return Err(Error::Invalid("empty code-length tree"));
                    }
                    self.stage = 2;
                    self.at = 1;
                }
                2 if self.at <= 15 => {
                    self.code = (self.code + tree.counts[self.at - 1]) << 1;
                    tree.first[self.at] = self.code;
                    tree.offsets[self.at] = self.offset;
                    self.offset += tree.counts[self.at];
                    self.at += 1;
                }
                2 => {
                    self.positions = tree.offsets;
                    self.next_codes = tree.first;
                    self.stage = 3;
                    self.at = 0;
                }
                3 if self.at < lengths.len() => {
                    // Scan at most 32 symbols between cancellation checks.
                    // Charge before each read/assignment, stopping at a quick
                    // code so its row fill remains separately resumable.
                    let count = (lengths.len() - self.at)
                        .min(work.remaining().saturating_add(1).min(32) as usize);
                    for offset in 0..count {
                        if offset != 0 && !work.try_charge(1) {
                            return Err(Error::Invalid("table work accounting"));
                        }
                        let width = usize::from(lengths[self.at]);
                        let symbol = self.at;
                        self.at += 1;
                        if width == 0 {
                            continue;
                        }
                        tree.symbols[usize::from(self.positions[width])] = symbol as u16;
                        self.positions[width] += 1;
                        if width <= HuffmanTree::<SYMBOLS, QUICK>::QUICK_BITS {
                            self.pending_key = reverse_width(self.next_codes[width], width);
                            self.pending_stride = 1_usize << width;
                            self.pending_entry = ((symbol as u16) << 4) | width as u16;
                            self.stage = 4;
                        }
                        self.next_codes[width] += 1;
                        if self.stage == 4 {
                            break;
                        }
                    }
                }
                4 if self.pending_key < 1 << tree.quick_width() => {
                    let last = (1 << tree.quick_width()) - 1;
                    let remaining = (last - self.pending_key) / self.pending_stride + 1;
                    let count = remaining.min(work.remaining().saturating_add(1).min(64) as usize);
                    if !work.try_charge((count - 1) as u64) {
                        return Err(Error::Invalid("table work accounting"));
                    }
                    for _ in 0..count {
                        tree.quick[self.pending_key] = self.pending_entry;
                        self.pending_key += self.pending_stride;
                    }
                    if self.pending_key > last {
                        self.stage = 3;
                    }
                }
                4 => {
                    self.stage = 3;
                }
                _ => return Ok(true),
            }
        }
    }
}
const LENGTH: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const EXTRA: [usize; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

#[derive(Clone, Copy, Debug)]
enum Phase {
    Header,
    Block,
    StoredHeader,
    StoredReserve { remaining: usize },
    Stored { remaining: usize },
    DynamicHeader,
    CodeLengths { at: usize },
    BuildCodes,
    Lengths,
    Repeat { value: u8, remaining: usize },
    BuildLiterals,
    BuildDistances,
    Symbol,
    Literal(u8),
    Distance { length: usize },
    Match { distance: usize, remaining: usize },
    Finish,
    Checksum { at: usize },
    Freeze,
    Grow,
    Done,
}
/// Output and bit/block/tree/copy/checksum state survive both work yields and
/// retained-output backpressure. No partial decoded packet is returned.
#[derive(Debug)]
pub struct Inflater {
    source: SharedBytes,
    state: State,
}
#[derive(Debug)]
struct Growth {
    buffer: BudgetVec<u8>,
    at: usize,
    resume: Phase,
}
#[derive(Debug)]
struct State {
    phase: Phase,
    position: Position,
    output: Option<BudgetVec<u8>>,
    growth: Option<Growth>,
    pool: Budget,
    result: Option<SharedBytes>,
    limit: usize,
    initial: usize,
    policy_left: u64,
    last: bool,
    fixed: bool,
    window: usize,
    lengths: [u8; 320],
    code_lengths: [u8; 19],
    literals: usize,
    distances: usize,
    codes: usize,
    at: usize,
    builder: Option<Build>,
    code_tree: Option<CodeTree>,
    literal_tree: Option<Tree>,
    distance_tree: Option<DistanceTree>,
    a: u32,
    b: u32,
    failed: bool,
}
impl Inflater {
    #[inline(always)]
    pub fn new(source: SharedBytes, limits: InflateLimits, pool: &Budget) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        let limit = limits
            .output_bytes
            .min(source.len().saturating_mul(limits.expansion_ratio));
        let initial = source.len().saturating_mul(8).clamp(256, 8192).min(limit);
        Ok(Self {
            source,
            state: State {
                phase: Phase::Header,
                position: Position::new(),
                output: Some(BudgetVec::new(pool)),
                growth: None,
                pool: pool.clone(),
                result: None,
                limit,
                initial,
                policy_left: limits.work,
                last: false,
                fixed: false,
                window: 0,
                lengths: [0; 320],
                code_lengths: [0; 19],
                literals: 0,
                distances: 0,
                codes: 0,
                at: 0,
                builder: None,
                code_tree: None,
                literal_tree: None,
                distance_tree: None,
                a: 1,
                b: 0,
                failed: false,
            },
        })
    }
    pub fn output_len(&self) -> usize {
        self.state.output_len()
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        self.state.step(self.source.as_ref(), work, cancel)
    }
}
impl State {
    #[inline(always)]
    fn output_len(&self) -> usize {
        self.output.as_ref().map_or_else(
            || self.result.as_ref().map_or(0, |r| r.len()),
            BudgetVec::len,
        )
    }
    #[inline(always)]
    fn policy(&mut self, units: u64) -> Result<()> {
        let Some(left) = self.policy_left.checked_sub(units) else {
            return Err(Error::Limit("inflate work"));
        };
        self.policy_left = left;
        Ok(())
    }
    #[inline(always)]
    fn reserve_output(&mut self, extra: usize) -> Result<bool> {
        let Some(output) = self.output.as_ref() else {
            return Err(Error::Invalid("missing inflate output"));
        };
        let Some(needed) = output.len().checked_add(extra) else {
            return Err(Error::Overflow);
        };
        if needed > self.limit {
            return Err(Error::Limit("inflated output bytes or expansion ratio"));
        }
        if needed > output.capacity() {
            return self.grow_output(needed);
        }
        Ok(true)
    }
    #[cold]
    #[inline(never)]
    fn grow_output(&mut self, needed: usize) -> Result<bool> {
        let output = self
            .output
            .as_ref()
            .ok_or(Error::Invalid("missing inflate output"))?;
        let initial = if output.capacity() == 0 {
            self.initial
        } else {
            0
        };
        let target = needed
            .max(output.capacity().saturating_mul(2))
            .max(initial)
            .min(self.limit);
        // The replacement starts empty. Core reserves the same geometric target
        // and minimal fallback as before, without hiding a copy of the live prefix.
        let mut buffer = BudgetVec::new(&self.pool);
        buffer.try_reserve_exact(target).or_else(|error| {
            if target > needed
                && matches!(
                    error,
                    triergon_core::Error::Backpressure { .. }
                        | triergon_core::Error::Limit { .. }
                        | triergon_core::Error::Overflow
                )
            {
                buffer.try_reserve_exact(needed)
            } else {
                Err(error)
            }
        })?;
        if output.is_empty() {
            self.output = Some(buffer);
            return Ok(true);
        }
        self.growth = Some(Growth {
            buffer,
            at: 0,
            resume: self.phase,
        });
        self.phase = Phase::Grow;
        Ok(false)
    }
    fn copy_growth(&mut self) -> Result<()> {
        let growth = self
            .growth
            .as_mut()
            .ok_or(Error::Invalid("missing inflate growth"))?;
        let output = self
            .output
            .as_ref()
            .ok_or(Error::Invalid("missing inflate output"))?;
        // A growth unit is at most 2 KiB of already validated bytes, separate
        // from semantic DEFLATE/output work. Both allocations remain charged and
        // cancellation is checked before every unit. No decoded byte is appended.
        let count = (output.len() - growth.at).min(2048);
        growth
            .buffer
            .try_extend_from_slice(&output.as_slice()[growth.at..growth.at + count])?;
        growth.at += count;
        if growth.at == output.len() {
            let growth = self
                .growth
                .take()
                .ok_or(Error::Invalid("missing inflate growth"))?;
            self.output = Some(growth.buffer);
            self.phase = growth.resume;
        }
        Ok(())
    }
    #[inline(always)]
    fn append(&mut self, value: u8) -> Result<bool> {
        if !self.reserve_output(1)? {
            return Ok(false);
        }
        let Some(output) = self.output.as_mut() else {
            return Err(Error::Invalid("missing inflate output"));
        };
        output
            .try_push(value)
            .map_err(|error| Error::from(error.error))?;
        Ok(true)
    }
    // A fused successor consumes exactly the unit its dispatched phase would
    // consume, with the same cancellation boundary. Its phase is installed
    // before any operation which can report output backpressure.
    #[inline(always)]
    fn next_unit(work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(work.try_charge(1))
    }
    #[inline(always)]
    fn copy_match(
        &mut self,
        distance: usize,
        remaining: usize,
        work: &mut WorkBudget,
    ) -> Result<()> {
        let count = Self::batch_count(remaining, 8192, work);
        if !self.reserve_output(count)? {
            return Ok(());
        }
        Self::charge_batch(count, work)?;
        let Some(output) = self.output.as_mut() else {
            return Err(Error::Invalid("missing inflate output"));
        };
        output.try_extend_from_history(distance, count)?;
        self.phase = if count == remaining {
            Phase::Symbol
        } else {
            Phase::Match {
                distance,
                remaining: remaining - count,
            }
        };
        Ok(())
    }
    #[inline(always)]
    fn repeat_lengths(&mut self, value: u8, remaining: usize, work: &mut WorkBudget) -> Result<()> {
        let count = Self::batch_count(remaining, 64, work);
        Self::charge_batch(count, work)?;
        self.lengths[self.at..self.at + count].fill(value);
        self.at += count;
        self.phase = if count == remaining {
            Phase::Lengths
        } else {
            Phase::Repeat {
                value,
                remaining: remaining - count,
            }
        };
        Ok(())
    }
    #[inline(always)]
    fn distance(
        &mut self,
        length: usize,
        bits: &mut Bits<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let tree = if self.fixed {
            &FIXED_DISTANCE
        } else {
            let Some(tree) = self.distance_tree.as_ref() else {
                return Err(Error::Invalid("missing distance tree"));
            };
            tree
        };
        let code = tree.symbol(bits)?;
        if code >= 30 {
            return Err(Error::Invalid("reserved distance symbol"));
        }
        let extra = if code < 4 { 0 } else { code / 2 - 1 };
        let distance = DIST[code] + bits.take(extra)? as usize;
        if distance > self.output_len() || distance > self.window {
            return Err(Error::Invalid("DEFLATE distance outside window"));
        }
        self.policy(length as u64)?;
        self.phase = Phase::Match {
            distance,
            remaining: length,
        };
        if Self::next_unit(work, cancel)? {
            self.copy_match(distance, length, work)?;
        }
        Ok(())
    }
    #[inline(always)]
    fn symbol(
        &mut self,
        bits: &mut Bits<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.policy(1)?;
        let tree = if self.fixed {
            &FIXED_LITERAL
        } else {
            let Some(tree) = self.literal_tree.as_ref() else {
                return Err(Error::Invalid("missing literal tree"));
            };
            tree
        };
        let symbol = tree.symbol(bits)?;
        match symbol {
            0..=255 => {
                self.phase = Phase::Literal(symbol as u8);
                if Self::next_unit(work, cancel)? && self.append(symbol as u8)? {
                    self.phase = Phase::Symbol;
                }
            }
            256 => {
                self.phase = if self.last {
                    Phase::Finish
                } else {
                    Phase::Block
                };
            }
            257..=285 => {
                let length = LENGTH[symbol - 257] + bits.take(EXTRA[symbol - 257])? as usize;
                self.phase = Phase::Distance { length };
                if Self::next_unit(work, cancel)? {
                    self.distance(length, bits, work, cancel)?;
                }
            }
            _ => return Err(Error::Invalid("reserved length symbol")),
        }
        Ok(())
    }
    // The phase dispatch already charged one unit. Each further byte in a
    // bounded copy/checksum batch consumes another unit before the work starts.
    #[inline(always)]
    fn batch_count(remaining: usize, ceiling: usize, work: &WorkBudget) -> usize {
        let available = work.remaining().saturating_add(1).min(ceiling as u64) as usize;
        remaining.min(available)
    }
    #[inline(always)]
    fn charge_batch(count: usize, work: &mut WorkBudget) -> Result<()> {
        if count == 0 || !work.try_charge((count - 1) as u64) {
            return Err(Error::Invalid("inflate batch work accounting"));
        }
        Ok(())
    }
    fn step(
        &mut self,
        source: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        if cancel.load(Ordering::Relaxed) {
            self.failed = true;
            return Err(Error::Cancelled);
        }
        if self.failed {
            return Err(Error::Invalid(
                "inflater requires explicit replacement after error",
            ));
        }
        // Borrow the immutable source once per step, and keep the bit reader
        // local across phase dispatch. Every return saves the exact cursor;
        // allocation retries therefore resume the same pending operation.
        // Header validation still runs only in its charged phase. A short
        // unvalidated input has an empty provisional payload, never a panic.
        let payload = source.get(2..source.len().saturating_sub(4)).unwrap_or(&[]);
        let mut bits = self.position.reader(payload);
        let result = self.advance(source, &mut bits, work, cancel);
        self.position.save(&bits);
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
            value => value,
        }
    }
    fn advance(
        &mut self,
        source: &[u8],
        bits: &mut Bits<'_>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<SharedBytes>> {
        const ORDER: [usize; 19] = [
            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
        ];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if matches!(self.phase, Phase::Done) {
                return Ok(ParseStep::Event(
                    self.result
                        .as_ref()
                        .ok_or(Error::Invalid("missing inflated result"))?
                        .clone(),
                ));
            }
            // Builder itself charges each alphabet entry and table row.
            if matches!(
                self.phase,
                Phase::BuildCodes | Phase::BuildLiterals | Phase::BuildDistances
            ) {
                let lengths = match self.phase {
                    Phase::BuildCodes => &self.code_lengths[..],
                    Phase::BuildLiterals => &self.lengths[..self.literals],
                    _ => &self.lengths[self.literals..self.literals + self.distances],
                };
                let builder = self
                    .builder
                    .as_mut()
                    .ok_or(Error::Invalid("missing Huffman builder"))?;
                let ready = match self.phase {
                    Phase::BuildCodes => builder.advance(
                        self.code_tree
                            .as_mut()
                            .ok_or(Error::Invalid("missing code-length destination"))?,
                        lengths,
                        work,
                        cancel,
                    )?,
                    Phase::BuildLiterals => builder.advance(
                        self.literal_tree
                            .as_mut()
                            .ok_or(Error::Invalid("missing literal destination"))?,
                        lengths,
                        work,
                        cancel,
                    )?,
                    _ => builder.advance(
                        self.distance_tree
                            .as_mut()
                            .ok_or(Error::Invalid("missing distance destination"))?,
                        lengths,
                        work,
                        cancel,
                    )?,
                };
                if !ready {
                    return Ok(ParseStep::Yield);
                }
                self.builder = None;
                match self.phase {
                    Phase::BuildCodes => {
                        self.at = 0;
                        self.phase = Phase::Lengths;
                    }
                    Phase::BuildLiterals => {
                        self.distance_tree = Some(DistanceTree::empty());
                        self.builder = Some(Build::new(false));
                        self.phase = Phase::BuildDistances;
                    }
                    _ => self.phase = Phase::Symbol,
                }
                continue;
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            match self.phase {
                Phase::Header => {
                    self.window = header(source)?;
                    self.phase = Phase::Block;
                }
                Phase::Block => {
                    self.policy(1)?;
                    self.last = bits.take(1)? != 0;
                    let kind = bits.take(2)?;
                    self.phase = match kind {
                        0 => Phase::StoredHeader,
                        1 => {
                            self.fixed = true;
                            Phase::Symbol
                        }
                        2 => {
                            self.fixed = false;
                            Phase::DynamicHeader
                        }
                        _ => return Err(Error::Invalid("reserved DEFLATE block type")),
                    };
                }
                Phase::StoredHeader => {
                    bits.align()?;
                    let length = bits.take(16)? as usize;
                    if bits.take(16)? as u16 != !(length as u16) {
                        return Err(Error::Invalid("stored block complement"));
                    }
                    self.policy(length as u64)?;
                    self.phase = Phase::StoredReserve { remaining: length };
                }
                Phase::StoredReserve { remaining } => {
                    // LEN is known. Reserve once for its complete destination,
                    // while copying and checksum work still resume in batches.
                    if !self.reserve_output(remaining)? {
                        continue;
                    }
                    self.phase = if remaining != 0 {
                        Phase::Stored { remaining }
                    } else if self.last {
                        Phase::Finish
                    } else {
                        Phase::Block
                    };
                }
                Phase::Stored { remaining } => {
                    let count = Self::batch_count(remaining, 8192, work);
                    // Validate before reserving. Backpressure must leave both
                    // the bit reader and un-emitted remainder unchanged.
                    let start = bits.position();
                    let end = start.checked_add(count).ok_or(Error::Overflow)?;
                    if end > bits.data.len() {
                        return Err(Error::Truncated);
                    }
                    if !self.reserve_output(count)? {
                        continue;
                    }
                    Self::charge_batch(count, work)?;
                    self.output
                        .as_mut()
                        .ok_or(Error::Invalid("missing inflate output"))?
                        .try_extend_from_slice(&bits.data[start..end])?;
                    bits.skip_bytes(count)?;
                    self.phase = if count != remaining {
                        Phase::Stored {
                            remaining: remaining - count,
                        }
                    } else if self.last {
                        Phase::Finish
                    } else {
                        Phase::Block
                    };
                }
                Phase::DynamicHeader => {
                    let literals = bits.take(5)? as usize + 257;
                    let distances = bits.take(5)? as usize + 1;
                    let codes = bits.take(4)? as usize + 4;
                    if literals > 286 {
                        return Err(Error::Invalid("literal alphabet size"));
                    }
                    self.literals = literals;
                    self.distances = distances;
                    self.codes = codes;
                    self.code_lengths = [0; 19];
                    self.phase = Phase::CodeLengths { at: 0 };
                }
                Phase::CodeLengths { at } => {
                    if at < self.codes {
                        let value = bits.take(3)? as u8;
                        self.code_lengths[ORDER[at]] = value;
                        self.phase = Phase::CodeLengths { at: at + 1 };
                    } else {
                        self.code_tree = Some(CodeTree::empty());
                        self.builder = Some(Build::new(true));
                        self.phase = Phase::BuildCodes;
                    }
                }
                Phase::Lengths => {
                    if self.at == self.literals + self.distances {
                        if self.lengths[256] == 0 {
                            return Err(Error::Invalid("missing end-of-block code"));
                        }
                        self.literal_tree = Some(Tree::empty());
                        self.builder = Some(Build::new(false));
                        self.phase = Phase::BuildLiterals;
                        continue;
                    }
                    let Some(tree) = self.code_tree.as_ref() else {
                        return Err(Error::Invalid("missing code-length tree"));
                    };
                    let code = tree.symbol(bits)?;
                    let (value, count) = match code {
                        0..=15 => (code as u8, 1),
                        16 if self.at != 0 => {
                            (self.lengths[self.at - 1], bits.take(2)? as usize + 3)
                        }
                        17 => (0, bits.take(3)? as usize + 3),
                        18 => (0, bits.take(7)? as usize + 11),
                        _ => return Err(Error::Invalid("code-length repeat without predecessor")),
                    };
                    if self.at + count > self.literals + self.distances {
                        return Err(Error::Invalid("code-length repeat overrun"));
                    }
                    self.phase = Phase::Repeat {
                        value,
                        remaining: count,
                    };
                    if Self::next_unit(work, cancel)? {
                        self.repeat_lengths(value, count, work)?;
                    }
                }
                Phase::Repeat { value, remaining } => {
                    self.repeat_lengths(value, remaining, work)?
                }
                Phase::Symbol => self.symbol(bits, work, cancel)?,
                Phase::Literal(value) => {
                    if self.append(value)? {
                        self.phase = Phase::Symbol;
                    }
                }
                Phase::Distance { length } => self.distance(length, bits, work, cancel)?,
                Phase::Match {
                    distance,
                    remaining,
                } => self.copy_match(distance, remaining, work)?,
                Phase::Grow => self.copy_growth()?,
                Phase::Finish => {
                    bits.align()?;
                    if bits.position() != bits.data.len() {
                        return Err(Error::Invalid("trailing zlib bytes"));
                    }
                    self.phase = Phase::Checksum { at: 0 };
                }
                Phase::Checksum { at } => {
                    let output = self
                        .output
                        .as_ref()
                        .ok_or(Error::Invalid("missing inflate output"))?;
                    if at == output.len() {
                        let expected = u32::from_be_bytes(
                            source[source.len() - 4..]
                                .try_into()
                                .map_err(|_| Error::Truncated)?,
                        );
                        if ((self.b << 16) | self.a) != expected {
                            return Err(Error::Invalid("Adler-32 mismatch"));
                        }
                        self.phase = Phase::Freeze;
                        continue;
                    }
                    let count = Self::batch_count(output.len() - at, 4096, work);
                    Self::charge_batch(count, work)?;
                    let chunk = adler32(&output.as_slice()[at..at + count], cancel)?;
                    let old_a = self.a;
                    self.a = (self.a + (chunk & 0xffff) + 65520) % 65521;
                    self.b = ((u64::from(self.b)
                        + u64::from(chunk >> 16)
                        + count as u64 * ((u64::from(old_a) + 65520) % 65521))
                        % 65521) as u32;
                    self.phase = Phase::Checksum { at: at + count };
                }
                Phase::Freeze => {
                    let output = self
                        .output
                        .take()
                        .ok_or(Error::Invalid("missing inflate output"))?;
                    if output.is_empty() {
                        self.result = Some(SharedBytes::default());
                        self.phase = Phase::Done;
                        continue;
                    }
                    match output.freeze() {
                        Ok(bytes) => {
                            self.result = Some(SharedBytes::from_core(bytes));
                            self.phase = Phase::Done;
                        }
                        Err(error) => {
                            self.output = Some(error.buffer);
                            return Err(error.error.into());
                        }
                    }
                }
                _ => return Err(Error::Invalid("invalid inflate state")),
            }
        }
    }
}

#[cfg(test)]
#[path = "inflate_prefix_tests.rs"]
mod tests;

crate::matroska::typed::usage::storage!(Inflater, |s| [
    s.source,
    s.state.output,
    s.state.result,
    s.state.growth.as_ref().map(|growth| &growth.buffer)
]);
