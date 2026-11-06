//! zlib streams (RFC 1950/1951) byte-identical to the zlib library's output.
//!
//! The authored compression fixtures are pinned by hash to what zlib 1.2.12 produces
//! with `deflateInit2(level, Z_DEFLATED, 15, 8, strategy)` followed by one
//! `deflate(Z_NO_FLUSH)` and one `deflate(Z_FINISH)` over the whole input. This
//! module reproduces that output by following zlib's own algorithms: the level-6
//! lazy matcher (hash chains, lazy evaluation, `TOO_FAR`), the block coder
//! (heap-built Huffman trees with zlib's tie-breaking and length limiting,
//! run-length coded tree descriptions, the stored/fixed/dynamic choice) and the
//! level-0 stored-block layout. Only level 0 and level 6 are implemented, and the
//! window is never slid, so inputs are limited to [`MAX_INPUT`] bytes.
//!
//! This is tooling only and is never linked into a library crate.
//!
//! Altered source: a Rust rendering of the deflate algorithms of zlib's
//! `deflate.c` and `trees.c`, reduced to the paths above. It is not the original
//! software. zlib's notice follows.

// Copyright (C) 1995-2022 Jean-loup Gailly and Mark Adler
//
// This software is provided 'as-is', without any express or implied
// warranty.  In no event will the authors be held liable for any damages
// arising from the use of this software.
//
// Permission is granted to anyone to use this software for any purpose,
// including commercial applications, and to alter it and redistribute it
// freely, subject to the following restrictions:
//
// 1. The origin of this software must not be misrepresented; you must not
//    claim that you wrote the original software. If you use this software
//    in a product, an acknowledgment in the product documentation would be
//    appreciated but is not required.
// 2. Altered source versions must be plainly marked as such, and must not be
//    misrepresented as being the original software.
// 3. This notice may not be removed or altered from any source distribution.
//
// Jean-loup Gailly        Mark Adler
// jloup@gzip.org          madler@alumni.caltech.edu

/// Largest input accepted: the matcher never slides its 32 KiB window pair.
pub const MAX_INPUT: usize = 65_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    Default,
    Fixed,
}

/// Compress `data` at `level` (0 or 6), as zlib does in one finishing call.
pub fn compress(data: &[u8], level: u32, strategy: Strategy) -> Result<Vec<u8>, String> {
    if data.len() > MAX_INPUT {
        return Err(format!(
            "zlib input of {} bytes exceeds {MAX_INPUT}",
            data.len()
        ));
    }
    let mut out = Bits::default();
    let level_flags = match (level, strategy) {
        (0, _) | (_, Strategy::Fixed) => 0,
        (6, Strategy::Default) => 2,
        _ => return Err(format!("zlib level {level} is not implemented")),
    };
    let mut header = (0x78u16 << 8) | (level_flags << 6);
    header += 31 - header % 31;
    out.bytes.extend_from_slice(&header.to_be_bytes());
    if level == 0 {
        stored(data, &mut out);
    } else {
        Deflater::new(data, strategy).run(&mut out);
    }
    out.bytes.extend_from_slice(&adler32(data).to_be_bytes());
    Ok(out.bytes)
}

/// zlib's level-0 layout: the first call emits non-final stored blocks while at
/// least a 32 KiB block is available; the finishing call writes the rest as one
/// final block (possibly empty).
fn stored(data: &[u8], out: &mut Bits) {
    const MIN_BLOCK: usize = 32_768;
    let mut rest = data;
    while rest.len() >= MIN_BLOCK {
        let take = rest.len().min(65_535);
        stored_block(out, &rest[..take], false);
        rest = &rest[take..];
    }
    stored_block(out, rest, true);
}

fn stored_block(out: &mut Bits, data: &[u8], last: bool) {
    out.put(u32::from(last), 3);
    out.align();
    let len = data.len() as u16;
    out.bytes.extend_from_slice(&len.to_le_bytes());
    out.bytes.extend_from_slice(&(!len).to_le_bytes());
    out.bytes.extend_from_slice(data);
}

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// LSB-first bit writer.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    acc: u64,
    count: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        self.acc |= u64::from(value) << self.count;
        self.count += len;
        while self.count >= 8 {
            self.bytes.push(self.acc as u8);
            self.acc >>= 8;
            self.count -= 8;
        }
    }

    fn align(&mut self) {
        if self.count > 0 {
            self.bytes.push(self.acc as u8);
        }
        self.acc = 0;
        self.count = 0;
    }
}

const WSIZE: usize = 32_768;
const HASH_MASK: usize = WSIZE - 1;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const MIN_LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;
const MAX_DIST: usize = WSIZE - MIN_LOOKAHEAD;
const TOO_FAR: usize = 4096;
const GOOD_MATCH: usize = 8;
const MAX_LAZY: usize = 16;
const NICE_MATCH: usize = 128;
const MAX_CHAIN: usize = 128;
/// Symbols per block before a forced flush: `lit_bufsize - 1` with memLevel 8.
const MAX_SYMBOLS: usize = (1 << 14) - 1;

const LITERALS: usize = 256;
const END_BLOCK: usize = 256;
const L_CODES: usize = LITERALS + 1 + 29;
const D_CODES: usize = 30;
const BL_CODES: usize = 19;
const HEAP_SIZE: usize = 2 * L_CODES + 1;
const MAX_BITS: usize = 15;
const EXTRA_LBITS: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const EXTRA_DBITS: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const EXTRA_BLBITS: [u32; 19] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];
const BL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

enum Symbol {
    Literal(u8),
    /// Length minus 3, distance minus 1.
    Match(u8, u16),
}

struct Deflater<'a> {
    data: &'a [u8],
    /// The input followed by zeros, as zlib's window reads past the input end.
    window: Vec<u8>,
    head: Vec<u16>,
    prev: Vec<u16>,
    strategy: Strategy,
    tables: Tables,
}

impl<'a> Deflater<'a> {
    fn new(data: &'a [u8], strategy: Strategy) -> Self {
        let mut window = vec![0; data.len() + 2 * MAX_MATCH + 8];
        window[..data.len()].copy_from_slice(data);
        Deflater {
            data,
            window,
            head: vec![0; WSIZE],
            prev: vec![0; WSIZE],
            strategy,
            tables: Tables::new(),
        }
    }

    fn hash(&self, at: usize) -> usize {
        let w = &self.window;
        ((usize::from(w[at]) << 10) ^ (usize::from(w[at + 1]) << 5) ^ usize::from(w[at + 2]))
            & HASH_MASK
    }

    /// Insert the string at `at`; returns the previous head of its chain.
    fn insert(&mut self, at: usize) -> usize {
        let h = self.hash(at);
        let previous = self.head[h];
        self.prev[at & (WSIZE - 1)] = previous;
        self.head[h] = at as u16;
        usize::from(previous)
    }

    fn longest_match(
        &self,
        start: usize,
        mut current: usize,
        prev_length: usize,
        lookahead: usize,
    ) -> (usize, Option<usize>) {
        let w = &self.window;
        let mut chain = if prev_length >= GOOD_MATCH {
            MAX_CHAIN >> 2
        } else {
            MAX_CHAIN
        };
        let mut best = prev_length;
        let mut best_start = None;
        let nice = NICE_MATCH.min(lookahead);
        let limit = start.saturating_sub(MAX_DIST);
        loop {
            let m = current;
            if w[m + best] == w[start + best]
                && w[m + best - 1] == w[start + best - 1]
                && w[m] == w[start]
                && w[m + 1] == w[start + 1]
            {
                // Byte 2 is equal whenever bytes 0 and 1 and the hash are.
                let len = (3..=MAX_MATCH)
                    .find(|&i| w[start + i] != w[m + i])
                    .unwrap_or(MAX_MATCH);
                if len > best {
                    best_start = Some(m);
                    best = len;
                    if len >= nice {
                        break;
                    }
                }
            }
            current = usize::from(self.prev[current & (WSIZE - 1)]);
            chain -= 1;
            if current <= limit || chain == 0 {
                break;
            }
        }
        (best.min(lookahead), best_start)
    }

    fn run(mut self, out: &mut Bits) {
        let mut block = Block::new();
        let mut block_start = 0;
        let mut strstart = 0;
        let mut lookahead = self.data.len();
        let mut match_length = MIN_MATCH - 1;
        let mut match_start = 0;
        let mut match_available = false;
        while lookahead > 0 {
            let mut hash_head = 0;
            if lookahead >= MIN_MATCH {
                hash_head = self.insert(strstart);
            }
            let prev_length = match_length;
            let prev_match = match_start;
            match_length = MIN_MATCH - 1;
            if hash_head != 0 && prev_length < MAX_LAZY && strstart - hash_head <= MAX_DIST {
                let (len, found) = self.longest_match(strstart, hash_head, prev_length, lookahead);
                match_length = len;
                if let Some(found) = found {
                    match_start = found;
                }
                if match_length == MIN_MATCH && strstart - match_start > TOO_FAR {
                    match_length = MIN_MATCH - 1;
                }
            }
            if prev_length >= MIN_MATCH && match_length <= prev_length {
                let max_insert = strstart + lookahead - MIN_MATCH;
                block.symbols.push(Symbol::Match(
                    (prev_length - MIN_MATCH) as u8,
                    (strstart - 1 - prev_match - 1) as u16,
                ));
                lookahead -= prev_length - 1;
                for _ in 0..prev_length - 2 {
                    strstart += 1;
                    if strstart <= max_insert {
                        self.insert(strstart);
                    }
                }
                match_available = false;
                match_length = MIN_MATCH - 1;
                strstart += 1;
                if block.symbols.len() == MAX_SYMBOLS {
                    self.flush(out, &mut block, block_start, strstart, false);
                    block_start = strstart;
                }
            } else if match_available {
                block
                    .symbols
                    .push(Symbol::Literal(self.window[strstart - 1]));
                if block.symbols.len() == MAX_SYMBOLS {
                    self.flush(out, &mut block, block_start, strstart, false);
                    block_start = strstart;
                }
                strstart += 1;
                lookahead -= 1;
            } else {
                match_available = true;
                strstart += 1;
                lookahead -= 1;
            }
        }
        if match_available {
            block
                .symbols
                .push(Symbol::Literal(self.window[strstart - 1]));
        }
        self.flush(out, &mut block, block_start, strstart, true);
        out.align();
    }

    /// zlib's `_tr_flush_block`: the cheapest of stored, fixed and dynamic.
    fn flush(&self, out: &mut Bits, block: &mut Block, start: usize, end: usize, last: bool) {
        let stored_len = end - start;
        let t = &self.tables;
        for symbol in &block.symbols {
            match *symbol {
                Symbol::Literal(c) => block.ltree.freq[usize::from(c)] += 1,
                Symbol::Match(lc, dist) => {
                    block.ltree.freq[t.length_code[usize::from(lc)] + LITERALS + 1] += 1;
                    block.dtree.freq[t.d_code(usize::from(dist))] += 1;
                }
            }
        }
        block.ltree.freq[END_BLOCK] = 1;
        let mut lens = Lengths::default();
        let l_max = build_tree(
            &mut block.ltree,
            Some(&t.static_l),
            &EXTRA_LBITS,
            LITERALS + 1,
            L_CODES,
            MAX_BITS,
            &mut lens,
        );
        let d_max = build_tree(
            &mut block.dtree,
            Some(&t.static_d),
            &EXTRA_DBITS,
            0,
            D_CODES,
            MAX_BITS,
            &mut lens,
        );
        let mut bl = Tree::new(2 * BL_CODES + 1);
        scan_tree(&mut block.ltree, l_max, &mut bl);
        scan_tree(&mut block.dtree, d_max, &mut bl);
        build_tree(&mut bl, None, &EXTRA_BLBITS, 0, BL_CODES, 7, &mut lens);
        let mut max_blindex = BL_CODES - 1;
        while max_blindex >= 3 && bl.len[BL_ORDER[max_blindex]] == 0 {
            max_blindex -= 1;
        }
        lens.opt = lens
            .opt
            .wrapping_add(3 * (max_blindex as u64 + 1) + 5 + 5 + 4);
        let mut opt_lenb = lens.opt.wrapping_add(3 + 7) >> 3;
        let static_lenb = lens.stat.wrapping_add(3 + 7) >> 3;
        if static_lenb <= opt_lenb {
            opt_lenb = static_lenb;
        }
        if stored_len as u64 + 4 <= opt_lenb {
            stored_block(out, &self.window[start..end], last);
        } else if self.strategy == Strategy::Fixed || static_lenb == opt_lenb {
            out.put(2 + u32::from(last), 3);
            self.compress_block(out, &block.symbols, &t.static_l, &t.static_d);
        } else {
            out.put(4 + u32::from(last), 3);
            out.put((l_max + 1 - 257) as u32, 5);
            out.put(d_max as u32, 5);
            out.put((max_blindex + 1 - 4) as u32, 4);
            for &index in &BL_ORDER[..=max_blindex] {
                out.put(u32::from(bl.len[index]), 3);
            }
            send_tree(out, &block.ltree, l_max, &bl);
            send_tree(out, &block.dtree, d_max, &bl);
            self.compress_block(out, &block.symbols, &block.ltree, &block.dtree);
        }
        *block = Block::new();
    }

    fn compress_block(&self, out: &mut Bits, symbols: &[Symbol], ltree: &Tree, dtree: &Tree) {
        let t = &self.tables;
        for symbol in symbols {
            match *symbol {
                Symbol::Literal(c) => ltree.send(out, usize::from(c)),
                Symbol::Match(lc, dist) => {
                    let (lc, dist) = (usize::from(lc), usize::from(dist));
                    let code = t.length_code[lc];
                    ltree.send(out, code + LITERALS + 1);
                    if EXTRA_LBITS[code] != 0 {
                        out.put((lc - t.base_length[code]) as u32, EXTRA_LBITS[code]);
                    }
                    let code = t.d_code(dist);
                    dtree.send(out, code);
                    if EXTRA_DBITS[code] != 0 {
                        out.put((dist - t.base_dist[code]) as u32, EXTRA_DBITS[code]);
                    }
                }
            }
        }
        ltree.send(out, END_BLOCK);
    }
}

struct Block {
    symbols: Vec<Symbol>,
    ltree: Tree,
    dtree: Tree,
}

impl Block {
    fn new() -> Self {
        Block {
            symbols: Vec::new(),
            ltree: Tree::new(HEAP_SIZE),
            dtree: Tree::new(2 * D_CODES + 1),
        }
    }
}

/// One Huffman tree in zlib's layout: leaves first, then internal nodes.
struct Tree {
    freq: Vec<u32>,
    dad: Vec<u16>,
    len: Vec<u16>,
    code: Vec<u16>,
}

impl Tree {
    fn new(size: usize) -> Self {
        Tree {
            freq: vec![0; size],
            dad: vec![0; size],
            len: vec![0; size],
            code: vec![0; size],
        }
    }

    fn send(&self, out: &mut Bits, symbol: usize) {
        out.put(u32::from(self.code[symbol]), u32::from(self.len[symbol]));
    }
}

/// zlib's `opt_len` and `static_len` in bits.
#[derive(Default)]
struct Lengths {
    opt: u64,
    stat: u64,
}

fn smaller(tree: &Tree, depth: &[u8], n: usize, m: usize) -> bool {
    tree.freq[n] < tree.freq[m] || (tree.freq[n] == tree.freq[m] && depth[n] <= depth[m])
}

fn pqdownheap(tree: &Tree, depth: &[u8], heap: &mut [usize], heap_len: usize, mut k: usize) {
    let v = heap[k];
    let mut j = k << 1;
    while j <= heap_len {
        if j < heap_len && smaller(tree, depth, heap[j + 1], heap[j]) {
            j += 1;
        }
        if smaller(tree, depth, v, heap[j]) {
            break;
        }
        heap[k] = heap[j];
        k = j;
        j <<= 1;
    }
    heap[k] = v;
}

/// zlib's `build_tree` with `gen_bitlen` and `gen_codes`; returns `max_code`.
fn build_tree(
    tree: &mut Tree,
    stree: Option<&Tree>,
    extra: &[u32],
    extra_base: usize,
    elems: usize,
    max_length: usize,
    lens: &mut Lengths,
) -> usize {
    let mut heap = [0usize; HEAP_SIZE];
    let mut depth = [0u8; HEAP_SIZE];
    let mut heap_len = 0;
    let mut heap_max = HEAP_SIZE;
    let mut max_code: isize = -1;
    for n in 0..elems {
        if tree.freq[n] != 0 {
            heap_len += 1;
            heap[heap_len] = n;
            max_code = n as isize;
        } else {
            tree.len[n] = 0;
        }
    }
    while heap_len < 2 {
        let node = if max_code < 2 {
            max_code += 1;
            max_code as usize
        } else {
            0
        };
        heap_len += 1;
        heap[heap_len] = node;
        tree.freq[node] = 1;
        depth[node] = 0;
        lens.opt = lens.opt.wrapping_sub(1);
        if let Some(stree) = stree {
            lens.stat = lens.stat.wrapping_sub(u64::from(stree.len[node]));
        }
    }
    let max_code = max_code as usize;
    for n in (1..=heap_len / 2).rev() {
        pqdownheap(tree, &depth, &mut heap, heap_len, n);
    }
    let mut node = elems;
    while heap_len >= 2 {
        let n = heap[1];
        heap[1] = heap[heap_len];
        heap_len -= 1;
        pqdownheap(tree, &depth, &mut heap, heap_len, 1);
        let m = heap[1];
        heap_max -= 1;
        heap[heap_max] = n;
        heap_max -= 1;
        heap[heap_max] = m;
        tree.freq[node] = tree.freq[n] + tree.freq[m];
        depth[node] = depth[n].max(depth[m]) + 1;
        tree.dad[n] = node as u16;
        tree.dad[m] = node as u16;
        heap[1] = node;
        node += 1;
        pqdownheap(tree, &depth, &mut heap, heap_len, 1);
    }
    heap_max -= 1;
    heap[heap_max] = heap[1];

    // gen_bitlen
    let mut bl_count = [0u16; MAX_BITS + 1];
    tree.len[heap[heap_max]] = 0;
    let mut overflow = 0;
    let mut h = heap_max + 1;
    while h < HEAP_SIZE {
        let n = heap[h];
        let mut bits = usize::from(tree.len[usize::from(tree.dad[n])]) + 1;
        if bits > max_length {
            bits = max_length;
            overflow += 1;
        }
        tree.len[n] = bits as u16;
        h += 1;
        if n > max_code {
            continue;
        }
        bl_count[bits] += 1;
        let xbits = if n >= extra_base {
            extra[n - extra_base]
        } else {
            0
        };
        let f = u64::from(tree.freq[n]);
        lens.opt = lens.opt.wrapping_add(f * (bits as u64 + u64::from(xbits)));
        if let Some(stree) = stree {
            lens.stat = lens
                .stat
                .wrapping_add(f * (u64::from(stree.len[n]) + u64::from(xbits)));
        }
    }
    if overflow > 0 {
        while overflow > 0 {
            let mut bits = max_length - 1;
            while bl_count[bits] == 0 {
                bits -= 1;
            }
            bl_count[bits] -= 1;
            bl_count[bits + 1] += 2;
            bl_count[max_length] -= 1;
            overflow -= 2;
        }
        let mut h = HEAP_SIZE;
        for bits in (1..=max_length).rev() {
            let mut n = bl_count[bits];
            while n != 0 {
                h -= 1;
                let m = heap[h];
                if m > max_code {
                    continue;
                }
                if usize::from(tree.len[m]) != bits {
                    let delta = (bits as u64)
                        .wrapping_sub(u64::from(tree.len[m]))
                        .wrapping_mul(u64::from(tree.freq[m]));
                    lens.opt = lens.opt.wrapping_add(delta);
                    tree.len[m] = bits as u16;
                }
                n -= 1;
            }
        }
    }
    gen_codes(tree, max_code, &bl_count);
    max_code
}

fn gen_codes(tree: &mut Tree, max_code: usize, bl_count: &[u16; MAX_BITS + 1]) {
    let mut next = [0u16; MAX_BITS + 1];
    let mut code = 0u16;
    for bits in 1..=MAX_BITS {
        code = (code + bl_count[bits - 1]) << 1;
        next[bits] = code;
    }
    for n in 0..=max_code {
        let len = usize::from(tree.len[n]);
        if len != 0 {
            tree.code[n] = reverse(next[len], len);
            next[len] += 1;
        }
    }
}

fn reverse(code: u16, len: usize) -> u16 {
    code.reverse_bits() >> (16 - len)
}

/// Tally run-length codes for `tree`'s code lengths into `bl` (zlib's `scan_tree`).
fn scan_tree(tree: &mut Tree, max_code: usize, bl: &mut Tree) {
    tree.len[max_code + 1] = 0xffff;
    runs(tree, max_code, |kind, count| match kind {
        Run::Single(len) => bl.freq[len] += count,
        Run::Repeat(len, new) => {
            if new {
                bl.freq[len] += 1;
            }
            bl.freq[16] += 1;
        }
        Run::Zeros => bl.freq[if count <= 10 { 17 } else { 18 }] += 1,
    });
}

/// Send `tree`'s code lengths with the `bl` code (zlib's `send_tree`).
fn send_tree(out: &mut Bits, tree: &Tree, max_code: usize, bl: &Tree) {
    runs(tree, max_code, |kind, count| match kind {
        Run::Single(len) => (0..count).for_each(|_| bl.send(out, len)),
        Run::Repeat(len, new) => {
            let mut count = count;
            if new {
                bl.send(out, len);
                count -= 1;
            }
            bl.send(out, 16);
            out.put(count - 3, 2);
        }
        Run::Zeros if count <= 10 => {
            bl.send(out, 17);
            out.put(count - 3, 3);
        }
        Run::Zeros => {
            bl.send(out, 18);
            out.put(count - 11, 7);
        }
    });
}

enum Run {
    /// `count` literal copies of this length.
    Single(usize),
    /// A repeat of a non-zero length; `true` when it differs from the previous run.
    Repeat(usize, bool),
    Zeros,
}

/// zlib's run splitting shared by `scan_tree` and `send_tree`. `tree.len[max_code + 1]`
/// must hold the 0xffff guard.
fn runs(tree: &Tree, max_code: usize, mut emit: impl FnMut(Run, u32)) {
    let mut prevlen = usize::MAX;
    let mut nextlen = usize::from(tree.len[0]);
    let mut count = 0;
    let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
    for n in 0..=max_code {
        let curlen = nextlen;
        nextlen = usize::from(tree.len[n + 1]);
        count += 1;
        if count < max_count && curlen == nextlen {
            continue;
        } else if count < min_count {
            emit(Run::Single(curlen), count);
        } else if curlen != 0 {
            emit(Run::Repeat(curlen, curlen != prevlen), count);
        } else {
            emit(Run::Zeros, count);
        }
        count = 0;
        prevlen = curlen;
        (max_count, min_count) = if nextlen == 0 {
            (138, 3)
        } else if curlen == nextlen {
            (6, 3)
        } else {
            (7, 4)
        };
    }
}

/// zlib's static code tables.
struct Tables {
    length_code: [usize; 256],
    base_length: [usize; 29],
    dist_code: [usize; 512],
    base_dist: [usize; 30],
    static_l: Tree,
    static_d: Tree,
}

impl Tables {
    fn new() -> Self {
        let mut t = Tables {
            length_code: [0; 256],
            base_length: [0; 29],
            dist_code: [0; 512],
            base_dist: [0; 30],
            static_l: Tree::new(L_CODES + 2),
            static_d: Tree::new(D_CODES),
        };
        let mut length = 0;
        for code in 0..28 {
            t.base_length[code] = length;
            for _ in 0..1 << EXTRA_LBITS[code] {
                t.length_code[length] = code;
                length += 1;
            }
        }
        t.length_code[255] = 28;
        let mut dist = 0;
        for code in 0..16 {
            t.base_dist[code] = dist;
            for _ in 0..1 << EXTRA_DBITS[code] {
                t.dist_code[dist] = code;
                dist += 1;
            }
        }
        dist >>= 7;
        for code in 16..D_CODES {
            t.base_dist[code] = dist << 7;
            for _ in 0..1 << (EXTRA_DBITS[code] - 7) {
                t.dist_code[256 + dist] = code;
                dist += 1;
            }
        }
        let mut bl_count = [0u16; MAX_BITS + 1];
        for n in 0..L_CODES + 2 {
            let len = match n {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
            t.static_l.len[n] = len;
            bl_count[usize::from(len)] += 1;
        }
        gen_codes(&mut t.static_l, L_CODES + 1, &bl_count);
        for n in 0..D_CODES {
            t.static_d.len[n] = 5;
            t.static_d.code[n] = reverse(n as u16, 5);
        }
        t
    }

    fn d_code(&self, dist: usize) -> usize {
        if dist < 256 {
            self.dist_code[dist]
        } else {
            self.dist_code[256 + (dist >> 7)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn small_streams_match_zlib() {
        // Outputs of zlib 1.2.12 for the same calls.
        assert_eq!(
            hex(&compress(b"", 6, Strategy::Default).unwrap()),
            "789c030000000001"
        );
        assert_eq!(
            hex(&compress(b"", 0, Strategy::Default).unwrap()),
            "7801010000ffff00000001"
        );
        assert_eq!(
            hex(&compress(b"abcd", 0, Strategy::Default).unwrap()),
            "7801010400fbff6162636403d8018b"
        );
        assert_eq!(
            hex(&compress(b"\0\xff\0\0\xff", 6, Strategy::Default).unwrap()),
            "789c63f8cfc0f01f00050001ff"
        );
    }

    #[test]
    fn large_stored_input_splits_like_zlib() {
        let data = vec![7u8; 32_768];
        let out = compress(&data, 0, Strategy::Default).unwrap();
        assert_eq!(out.len(), 2 + 5 + 32_768 + 5 + 4);
        assert_eq!(&out[2..7], &[0, 0, 0x80, 0xff, 0x7f]);
        assert_eq!(&out[32_775..32_780], &[1, 0, 0, 0xff, 0xff]);
    }

    #[test]
    fn rejects_unimplemented_parameters() {
        assert!(compress(b"x", 9, Strategy::Default).is_err());
        assert!(compress(&vec![0; MAX_INPUT + 1], 6, Strategy::Default).is_err());
    }
}
