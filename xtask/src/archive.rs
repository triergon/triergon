//! Reading `.tar.gz` archives: DEFLATE (RFC 1951), gzip (RFC 1952) and POSIX/pax
//! tar entries. Both decoders stream, so an archive larger than memory can be
//! verified and unpacked; the in-memory helpers are built on them.

use std::io::{self, Read, Write};

/// Decompress a gzip file (all members), bounded by `limit` output bytes.
pub fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    gunzip_to(data, &mut out, limit as u64)?;
    Ok(out)
}

/// Decompress every gzip member read from `input` into `out`, checking each
/// member's CRC-32 and length, and refusing more than `limit` output bytes.
/// Returns the bytes written. `input` is read one byte at a time outside stored
/// blocks, so give it a buffered reader.
pub fn gunzip_to(mut input: impl Read, out: &mut impl Write, limit: u64) -> Result<u64, String> {
    let mut written = 0u64;
    loop {
        let mut magic = [0u8; 1];
        if read_some(&mut input, &mut magic)? == 0 {
            return Ok(written);
        }
        let mut header = [0u8; 10];
        header[0] = magic[0];
        read_all(&mut input, &mut header[1..], "truncated gzip header")?;
        if header[0] != 0x1f || header[1] != 0x8b || header[2] != 8 {
            return Err("not a gzip deflate member".into());
        }
        let flags = header[3];
        if flags & 4 != 0 {
            let mut extra = [0u8; 2];
            read_all(&mut input, &mut extra, "truncated gzip extra field")?;
            skip(
                &mut input,
                u64::from(u16::from_le_bytes(extra)),
                "truncated gzip extra field",
            )?;
        }
        for bit in [8, 16] {
            if flags & bit != 0 {
                let mut byte = [1u8];
                while byte[0] != 0 {
                    read_all(&mut input, &mut byte, "truncated gzip name")?;
                }
            }
        }
        if flags & 2 != 0 {
            skip(&mut input, 2, "truncated gzip header checksum")?;
        }
        let mut member = Window::new(&mut *out, limit - written);
        let mut bits = BitReader::new(&mut input);
        inflate_into(&mut bits, &mut member)?;
        let (crc, size) = member.finish()?;
        let mut trailer = [0u8; 8];
        read_all(&mut input, &mut trailer, "truncated gzip trailer")?;
        let expected_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let expected_size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
        // ISIZE is the length modulo 2^32.
        if crc != expected_crc || size as u32 != expected_size {
            return Err("gzip member checksum mismatch".into());
        }
        written += size;
    }
}

fn read_some(input: &mut impl Read, buf: &mut [u8]) -> Result<usize, String> {
    loop {
        match input.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            result => return result.map_err(|e| e.to_string()),
        }
    }
}

fn read_all(input: &mut impl Read, buf: &mut [u8], truncated: &str) -> Result<(), String> {
    input.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => truncated.to_owned(),
        _ => e.to_string(),
    })
}

fn skip(input: &mut impl Read, count: u64, truncated: &str) -> Result<(), String> {
    let skipped = io::copy(&mut input.take(count), &mut io::sink()).map_err(|e| e.to_string())?;
    if skipped == count {
        Ok(())
    } else {
        Err(truncated.to_owned())
    }
}

/// Bits read least significant first. A byte is fetched only when its first bit
/// is needed, so after the final block the input holds exactly what follows the
/// stream.
struct BitReader<R> {
    input: R,
    byte: u8,
    /// Bits of `byte` already used; 8 when the next bit needs a new byte.
    used: u32,
}

impl<R: Read> BitReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            byte: 0,
            used: 8,
        }
    }

    fn bits(&mut self, count: u32) -> Result<u32, String> {
        let mut value = 0;
        for i in 0..count {
            if self.used == 8 {
                let mut byte = [0u8];
                read_all(&mut self.input, &mut byte, "truncated deflate stream")?;
                (self.byte, self.used) = (byte[0], 0);
            }
            value |= u32::from((self.byte >> self.used) & 1) << i;
            self.used += 1;
        }
        Ok(value)
    }

    fn align(&mut self) {
        self.used = 8;
    }
}

/// The largest distance a DEFLATE match reaches back.
const WINDOW: usize = 32 * 1024;

/// Inflated output on its way to `out`: the last [`WINDOW`] bytes stay for
/// matches, the rest is written as it accumulates.
struct Window<'a, W: Write> {
    out: &'a mut W,
    held: Vec<u8>,
    /// Bytes of this stream already passed to `out`.
    flushed: u64,
    limit: u64,
    crc: u32,
}

impl<'a, W: Write> Window<'a, W> {
    fn new(out: &'a mut W, limit: u64) -> Self {
        Self {
            out,
            held: Vec::with_capacity(4 * WINDOW),
            flushed: 0,
            limit,
            crc: 0,
        }
    }

    fn len(&self) -> u64 {
        self.flushed + self.held.len() as u64
    }

    fn reserve(&mut self, count: usize) -> Result<(), String> {
        if self.len() + count as u64 > self.limit {
            return Err("decompressed size limit".into());
        }
        if self.held.len() + count > 4 * WINDOW && self.held.len() > WINDOW {
            let pass = self.held.len() - WINDOW;
            self.write_out(pass)?;
        }
        Ok(())
    }

    fn write_out(&mut self, count: usize) -> Result<(), String> {
        let passed = &self.held[..count];
        self.crc = crate::zlib::crc32_extend(self.crc, passed);
        self.out.write_all(passed).map_err(|e| e.to_string())?;
        self.held.drain(..count);
        self.flushed += count as u64;
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.reserve(bytes.len())?;
        self.held.extend_from_slice(bytes);
        Ok(())
    }

    /// Append `len` bytes read from `input`.
    fn push_from(&mut self, input: &mut impl Read, len: usize) -> Result<(), String> {
        self.reserve(len)?;
        let start = self.held.len();
        self.held.resize(start + len, 0);
        read_all(input, &mut self.held[start..], "truncated stored block")
    }

    /// Copy `len` bytes from `distance` back, overlapping as DEFLATE allows.
    fn repeat(&mut self, distance: usize, len: usize) -> Result<(), String> {
        if distance as u64 > self.len() {
            return Err("distance before stream start".into());
        }
        self.reserve(len)?;
        let start = self.held.len() - distance;
        for k in 0..len {
            self.held.push(self.held[start + k]);
        }
        Ok(())
    }

    /// Write what is held and return the stream's CRC-32 and length.
    fn finish(mut self) -> Result<(u32, u64), String> {
        let held = self.held.len();
        self.write_out(held)?;
        Ok((self.crc, self.flushed))
    }
}

/// Canonical Huffman decoding table: symbols ordered by (length, value).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &len in lengths {
            counts[usize::from(len)] += 1;
        }
        counts[0] = 0;
        let mut left = 1i32;
        for &count in &counts[1..] {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return Err("over-subscribed Huffman code".into());
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0; lengths.len()];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[usize::from(offsets[usize::from(len)])] = symbol as u16;
                offsets[usize::from(len)] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, input: &mut BitReader<impl Read>) -> Result<usize, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= input.bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                return Ok(usize::from(self.symbols[(index + code - first) as usize]));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("invalid Huffman code".into())
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Inflate one raw DEFLATE stream from `input` into `out`.
fn inflate_into<R: Read, W: Write>(
    input: &mut BitReader<R>,
    out: &mut Window<'_, W>,
) -> Result<(), String> {
    loop {
        let last = input.bits(1)? == 1;
        match input.bits(2)? {
            0 => {
                input.align();
                let mut header = [0u8; 4];
                read_all(&mut input.input, &mut header, "truncated stored block")?;
                let len = u16::from_le_bytes([header[0], header[1]]);
                if len != !u16::from_le_bytes([header[2], header[3]]) {
                    return Err("stored block length mismatch".into());
                }
                out.push_from(&mut input.input, usize::from(len))?;
            }
            1 => {
                let mut lengths = [8u8; 288];
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                codes(
                    input,
                    out,
                    &Huffman::new(&lengths)?,
                    &Huffman::new(&[5; 30])?,
                )?;
            }
            2 => {
                let nlen = input.bits(5)? as usize + 257;
                let ndist = input.bits(5)? as usize + 1;
                let ncode = input.bits(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut code_lengths = [0u8; 19];
                for &index in &ORDER[..ncode] {
                    code_lengths[index] = input.bits(3)? as u8;
                }
                let code = Huffman::new(&code_lengths)?;
                let mut lengths = vec![0u8; nlen + ndist];
                let mut i = 0;
                while i < nlen + ndist {
                    let symbol = code.decode(input)?;
                    let (value, repeat) = match symbol {
                        0..=15 => (symbol as u8, 1),
                        16 => (
                            *lengths
                                .get(i.wrapping_sub(1))
                                .ok_or("repeat without a length")?,
                            3 + input.bits(2)?,
                        ),
                        17 => (0, 3 + input.bits(3)?),
                        _ => (0, 11 + input.bits(7)?),
                    };
                    for _ in 0..repeat {
                        *lengths.get_mut(i).ok_or("code lengths overflow")? = value;
                        i += 1;
                    }
                }
                let lit = Huffman::new(&lengths[..nlen])?;
                let dist = Huffman::new(&lengths[nlen..])?;
                codes(input, out, &lit, &dist)?;
            }
            _ => return Err("invalid deflate block type".into()),
        }
        if last {
            input.align();
            return Ok(());
        }
    }
}

fn codes<R: Read, W: Write>(
    input: &mut BitReader<R>,
    out: &mut Window<'_, W>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), String> {
    loop {
        let symbol = lit.decode(input)?;
        match symbol {
            0..=255 => out.push(&[symbol as u8])?,
            256 => return Ok(()),
            _ => {
                let index = symbol - 257;
                let base = *LENGTH_BASE.get(index).ok_or("invalid length code")?;
                let len = usize::from(base) + input.bits(u32::from(LENGTH_EXTRA[index]))? as usize;
                let d = dist.decode(input)?;
                let base = *DIST_BASE.get(d).ok_or("invalid distance code")?;
                let distance = usize::from(base) + input.bits(u32::from(DIST_EXTRA[d]))? as usize;
                out.repeat(distance, len)?;
            }
        }
    }
}

/// A regular file in a tar archive.
pub struct Entry {
    pub path: String,
    pub data: Vec<u8>,
}

/// The regular files of a tar archive, with pax and GNU long names applied.
pub fn tar_files(data: &[u8]) -> Result<Vec<Entry>, String> {
    #[derive(Default)]
    struct Collect(Vec<Entry>);
    impl TarFiles for Collect {
        fn begin(&mut self, path: &str, size: u64) -> Result<(), String> {
            usize::try_from(size).map_err(|_| "tar entry size")?;
            self.0.push(Entry {
                path: path.to_owned(),
                data: Vec::new(),
            });
            Ok(())
        }
        fn data(&mut self, chunk: &[u8]) -> Result<(), String> {
            self.0
                .last_mut()
                .expect("begun")
                .data
                .extend_from_slice(chunk);
            Ok(())
        }
        fn end(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
    let mut tar = TarReader::new(Collect::default());
    tar.write_all(data).map_err(|e| e.to_string())?;
    Ok(tar.finish()?.0)
}

/// Receives the regular files of a tar stream as [`TarReader`] parses it.
pub trait TarFiles {
    /// A regular file starts.
    fn begin(&mut self, path: &str, size: u64) -> Result<(), String>;
    /// The next bytes of the current file.
    fn data(&mut self, chunk: &[u8]) -> Result<(), String>;
    /// The current file is complete.
    fn end(&mut self) -> Result<(), String>;
}

/// The largest pax or GNU long-name record kept.
const NAME_RECORD_LIMIT: u64 = 1 << 20;

/// What the bytes after a tar header are.
enum Body {
    File,
    Name(Vec<u8>, NameRecord),
    Skip,
}

#[derive(Clone, Copy)]
enum NameRecord {
    Pax,
    Gnu,
}

enum TarState {
    Header,
    Body { body: Body, left: u64, pad: u64 },
    End,
}

/// A tar parser fed by `write`: regular files go to `F` as their bytes arrive,
/// with pax and GNU long names applied. Parsing stops at the first zero header.
pub struct TarReader<F> {
    files: F,
    header: Vec<u8>,
    state: TarState,
    long_name: Option<String>,
}

impl<F: TarFiles> TarReader<F> {
    pub fn new(files: F) -> Self {
        Self {
            files,
            header: Vec::with_capacity(512),
            state: TarState::Header,
            long_name: None,
        }
    }

    /// Check the stream ended at an entry boundary and return the receiver.
    pub fn finish(self) -> Result<F, String> {
        match self.state {
            TarState::Header if self.header.is_empty() => Ok(self.files),
            TarState::End => Ok(self.files),
            _ => Err("truncated tar entry".into()),
        }
    }

    fn start(&mut self) -> Result<(), String> {
        let header = std::mem::take(&mut self.header);
        if header.iter().all(|&b| b == 0) {
            self.state = TarState::End;
            return Ok(());
        }
        let field = |range: std::ops::Range<usize>| {
            let raw = &header[range];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let size =
            u64::from_str_radix(field(124..136).trim(), 8).map_err(|_| "invalid tar size")?;
        let mut name = field(0..100);
        if &header[257..262] == b"ustar" {
            let prefix = field(345..500);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let record = |kind| {
            if size > NAME_RECORD_LIMIT {
                return Err("tar name record too long".to_owned());
            }
            Ok(Body::Name(Vec::new(), kind))
        };
        let body = match header[156] {
            b'x' => record(NameRecord::Pax)?,
            b'L' => record(NameRecord::Gnu)?,
            b'g' => Body::Skip,
            kind => {
                let path = self.long_name.take().unwrap_or(name);
                if kind == b'0' || kind == 0 || kind == b'7' {
                    self.files.begin(&path, size)?;
                    Body::File
                } else {
                    Body::Skip
                }
            }
        };
        self.header = header;
        self.header.clear();
        self.state = TarState::Body {
            body,
            left: size,
            pad: size.next_multiple_of(512) - size,
        };
        self.complete_empty()
    }

    /// End a body that has no bytes left.
    fn complete_empty(&mut self) -> Result<(), String> {
        let TarState::Body { body, left: 0, pad } = &mut self.state else {
            return Ok(());
        };
        match std::mem::replace(body, Body::Skip) {
            Body::File => self.files.end()?,
            Body::Name(record, NameRecord::Pax) => {
                self.long_name = pax_path(&record)?.or(self.long_name.take());
            }
            Body::Name(record, NameRecord::Gnu) => {
                let end = record.iter().position(|&b| b == 0).unwrap_or(record.len());
                self.long_name = Some(String::from_utf8_lossy(&record[..end]).into_owned());
            }
            Body::Skip => {}
        }
        if *pad == 0 {
            self.state = TarState::Header;
        }
        Ok(())
    }

    fn feed(&mut self, mut data: &[u8]) -> Result<(), String> {
        while !data.is_empty() {
            match &mut self.state {
                TarState::End => return Ok(()),
                TarState::Header => {
                    let take = (512 - self.header.len()).min(data.len());
                    self.header.extend_from_slice(&data[..take]);
                    data = &data[take..];
                    if self.header.len() == 512 {
                        self.start()?;
                    }
                }
                TarState::Body { left: 0, pad, .. } => {
                    let take = (*pad).min(data.len() as u64);
                    *pad -= take;
                    data = &data[take as usize..];
                    if *pad == 0 {
                        self.state = TarState::Header;
                    }
                }
                TarState::Body { body, left, .. } => {
                    let take = (*left).min(data.len() as u64) as usize;
                    match body {
                        Body::File => self.files.data(&data[..take])?,
                        Body::Name(record, _) => record.extend_from_slice(&data[..take]),
                        Body::Skip => {}
                    }
                    *left -= take as u64;
                    data = &data[take..];
                    self.complete_empty()?;
                }
            }
        }
        Ok(())
    }
}

impl<F: TarFiles> Write for TarReader<F> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.feed(data).map_err(io::Error::other)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn pax_path(body: &[u8]) -> Result<Option<String>, String> {
    let mut path = None;
    let mut rest = body;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or("invalid pax record")?;
        let len: usize = std::str::from_utf8(&rest[..space])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or("invalid pax length")?;
        let record = rest.get(space + 1..len).ok_or("truncated pax record")?;
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(value) = record.strip_prefix(b"path=") {
            path = Some(String::from_utf8_lossy(value).into_owned());
        }
        rest = &rest[len..];
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zlib::{Strategy, compress};

    /// Inflate one raw DEFLATE stream into `out`; returns the input bytes consumed.
    fn inflate(data: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, String> {
        let mut input = data;
        let mut window = Window::new(&mut *out, limit as u64);
        inflate_into(&mut BitReader::new(&mut input), &mut window)?;
        window.finish()?;
        Ok(data.len() - input.len())
    }

    fn gzip(raw_deflate: &[u8], original: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 8, 0, 0, 0, 0, 0, 3];
        out.extend(b"name\0");
        out.extend_from_slice(raw_deflate);
        out.extend(crate::zlib::crc32(original).to_le_bytes());
        out.extend((original.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn inflates_what_the_zlib_port_writes() {
        let mut text =
            b"bounded zlib track, signed timestamps, cues and decoder preroll. ".repeat(300);
        text.extend((0..=255u8).cycle().take(5000));
        for (level, strategy) in [
            (0, Strategy::Default),
            (6, Strategy::Fixed),
            (6, Strategy::Default),
        ] {
            let stream = compress(&text, level, strategy).unwrap();
            let raw = &stream[2..stream.len() - 4];
            let mut out = Vec::new();
            assert_eq!(inflate(raw, &mut out, usize::MAX).unwrap(), raw.len());
            assert_eq!(out, text);
            assert_eq!(gunzip(&gzip(raw, &text), usize::MAX).unwrap(), text);
            assert!(gunzip(&gzip(raw, &text), text.len() - 1).is_err());
        }
    }

    #[test]
    fn rejects_corrupt_streams() {
        let stream = compress(b"hello hello hello", 6, Strategy::Default).unwrap();
        let mut bad = gzip(&stream[2..stream.len() - 4], b"hello hello hello");
        let n = bad.len();
        bad[n - 8] ^= 1;
        assert!(gunzip(&bad, usize::MAX).is_err());
        assert!(inflate(&[0x07], &mut Vec::new(), usize::MAX).is_err());
    }

    fn tar_header(name: &str, kind: u8, size: usize) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h
    }

    fn padded(body: &[u8]) -> Vec<u8> {
        let mut out = body.to_vec();
        out.resize(body.len().div_ceil(512) * 512, 0);
        out
    }

    #[test]
    fn lists_regular_files_with_pax_names() {
        let pax = b"30 path=long/name/of/file.txt\n";
        let archive = [
            tar_header("pax_global_header", b'g', 0),
            tar_header("dir/", b'5', 0),
            tar_header("dir/a.txt", b'0', 3),
            padded(b"abc"),
            tar_header("PaxHeader", b'x', pax.len()),
            padded(pax),
            tar_header("short", b'0', 2),
            padded(b"xy"),
            tar_header("link", b'2', 0),
            vec![0; 1024],
        ]
        .concat();
        let files = tar_files(&archive).unwrap();
        let listed: Vec<_> = files
            .iter()
            .map(|f| (f.path.as_str(), f.data.as_slice()))
            .collect();
        assert_eq!(
            listed,
            [
                ("dir/a.txt", &b"abc"[..]),
                ("long/name/of/file.txt", &b"xy"[..])
            ]
        );

        // Fed one byte at a time, the reader sees the same files.
        let mut one_by_one = TarReader::new(Recorded::default());
        for byte in &archive {
            one_by_one.write_all(std::slice::from_ref(byte)).unwrap();
        }
        let recorded = one_by_one.finish().unwrap();
        assert_eq!(
            recorded.0,
            [
                ("dir/a.txt".to_owned(), b"abc".to_vec()),
                ("long/name/of/file.txt".to_owned(), b"xy".to_vec())
            ]
        );
        assert!(tar_files(&archive[..512 + 512 + 512 + 100]).is_err());
    }

    #[derive(Default)]
    struct Recorded(Vec<(String, Vec<u8>)>);

    impl TarFiles for Recorded {
        fn begin(&mut self, path: &str, _size: u64) -> Result<(), String> {
            self.0.push((path.to_owned(), Vec::new()));
            Ok(())
        }
        fn data(&mut self, chunk: &[u8]) -> Result<(), String> {
            self.0.last_mut().unwrap().1.extend_from_slice(chunk);
            Ok(())
        }
        fn end(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn streams_gzip_members_through_the_window() {
        let text: Vec<u8> = (0..60_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8 % 7 + b'a')
            .collect();
        let stream = compress(&text, 6, Strategy::Default).unwrap();
        let member = gzip(&stream[2..stream.len() - 4], &text);
        let two = [member.clone(), member].concat();
        let mut out = Vec::new();
        let written = gunzip_to(io::BufReader::new(&two[..]), &mut out, u64::MAX).unwrap();
        assert_eq!(written, 2 * text.len() as u64);
        assert_eq!(out, [text.clone(), text.clone()].concat());
        assert!(gunzip_to(&two[..two.len() - 1], &mut Vec::new(), u64::MAX).is_err());
        assert!(gunzip_to(&two[..], &mut Vec::new(), 2 * text.len() as u64 - 1).is_err());
    }

    #[test]
    fn matches_reach_a_full_window_back_after_output_is_written() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        let mut window = Window::new(&mut out, u64::MAX);
        for piece in data.chunks(65_535) {
            window.push(piece).unwrap();
        }
        window.repeat(WINDOW, 300).unwrap();
        let (crc, len) = window.finish().unwrap();
        let mut expected = data.clone();
        for k in 0..300 {
            expected.push(expected[data.len() - WINDOW + k]);
        }
        assert_eq!(len, expected.len() as u64);
        assert_eq!(crc, crate::zlib::crc32(&expected));
        assert_eq!(out[..expected.len()], expected[..]);
    }
}
