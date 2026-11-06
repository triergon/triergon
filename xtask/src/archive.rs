//! Reading `.tar.gz` source archives: DEFLATE (RFC 1951), gzip (RFC 1952) and
//! POSIX/pax tar entries, enough to verify an extracted tree against its archive.

/// Decompress a gzip file (all members), bounded by `limit` output bytes.
pub fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let header = data.get(at..at + 10).ok_or("truncated gzip header")?;
        if header[0] != 0x1f || header[1] != 0x8b || header[2] != 8 {
            return Err("not a gzip deflate member".into());
        }
        let flags = header[3];
        let mut pos = at + 10;
        if flags & 4 != 0 {
            let extra = data.get(pos..pos + 2).ok_or("truncated gzip extra field")?;
            pos += 2 + usize::from(u16::from_le_bytes([extra[0], extra[1]]));
        }
        for bit in [8, 16] {
            if flags & bit != 0 {
                let end = data
                    .get(pos..)
                    .and_then(|rest| rest.iter().position(|&b| b == 0))
                    .ok_or("truncated gzip name")?;
                pos += end + 1;
            }
        }
        if flags & 2 != 0 {
            pos += 2;
        }
        let start = out.len();
        let consumed = inflate(
            data.get(pos..).ok_or("truncated gzip member")?,
            &mut out,
            limit,
        )?;
        pos += consumed;
        let trailer = data.get(pos..pos + 8).ok_or("truncated gzip trailer")?;
        let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
        if crate::zlib::crc32(&out[start..]) != crc || (out.len() - start) as u32 != size {
            return Err("gzip member checksum mismatch".into());
        }
        at = pos + 8;
    }
    Ok(out)
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl BitReader<'_> {
    fn bits(&mut self, count: u32) -> Result<u32, String> {
        let mut value = 0;
        for i in 0..count {
            let byte = *self.data.get(self.pos).ok_or("truncated deflate stream")?;
            value |= u32::from((byte >> self.bit) & 1) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(value)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
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

    fn decode(&self, input: &mut BitReader<'_>) -> Result<usize, String> {
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

/// Inflate one raw DEFLATE stream into `out`; returns the input bytes consumed.
pub fn inflate(data: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize, String> {
    let mut input = BitReader {
        data,
        pos: 0,
        bit: 0,
    };
    loop {
        let last = input.bits(1)? == 1;
        match input.bits(2)? {
            0 => {
                input.align();
                let header = data
                    .get(input.pos..input.pos + 4)
                    .ok_or("truncated stored block")?;
                let len = usize::from(u16::from_le_bytes([header[0], header[1]]));
                if len as u16 != !u16::from_le_bytes([header[2], header[3]]) {
                    return Err("stored block length mismatch".into());
                }
                let start = input.pos + 4;
                let block = data
                    .get(start..start + len)
                    .ok_or("truncated stored block")?;
                push(out, block, limit)?;
                input.pos = start + len;
            }
            1 => {
                let mut lengths = [8u8; 288];
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                codes(
                    &mut input,
                    out,
                    &Huffman::new(&lengths)?,
                    &Huffman::new(&[5; 30])?,
                    limit,
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
                    let symbol = code.decode(&mut input)?;
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
                codes(&mut input, out, &lit, &dist, limit)?;
            }
            _ => return Err("invalid deflate block type".into()),
        }
        if last {
            input.align();
            return Ok(input.pos);
        }
    }
}

fn push(out: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), String> {
    if out.len() + bytes.len() > limit {
        return Err("decompressed size limit".into());
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn codes(
    input: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    limit: usize,
) -> Result<(), String> {
    loop {
        let symbol = lit.decode(input)?;
        match symbol {
            0..=255 => push(out, &[symbol as u8], limit)?,
            256 => return Ok(()),
            _ => {
                let index = symbol - 257;
                let base = *LENGTH_BASE.get(index).ok_or("invalid length code")?;
                let len = usize::from(base) + input.bits(u32::from(LENGTH_EXTRA[index]))? as usize;
                let d = dist.decode(input)?;
                let base = *DIST_BASE.get(d).ok_or("invalid distance code")?;
                let distance = usize::from(base) + input.bits(u32::from(DIST_EXTRA[d]))? as usize;
                if distance > out.len() {
                    return Err("distance before stream start".into());
                }
                if out.len() + len > limit {
                    return Err("decompressed size limit".into());
                }
                let start = out.len() - distance;
                for k in 0..len {
                    out.push(out[start + k]);
                }
            }
        }
    }
}

/// A regular file in a tar archive.
pub struct Entry<'a> {
    pub path: String,
    pub data: &'a [u8],
}

/// The regular files of a tar archive, with pax and GNU long names applied.
pub fn tar_files(data: &[u8]) -> Result<Vec<Entry<'_>>, String> {
    let mut files = Vec::new();
    let mut at = 0;
    let mut long_name: Option<String> = None;
    while at + 512 <= data.len() {
        let header = &data[at..at + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let field = |range: std::ops::Range<usize>| {
            let raw = &header[range];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let size =
            usize::from_str_radix(field(124..136).trim(), 8).map_err(|_| "invalid tar size")?;
        let body = data
            .get(at + 512..at + 512 + size)
            .ok_or("truncated tar entry")?;
        let kind = header[156];
        let mut name = field(0..100);
        if &header[257..262] == b"ustar" {
            let prefix = field(345..500);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        match kind {
            b'x' => long_name = pax_path(body)?.or(long_name),
            b'L' => {
                let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                long_name = Some(String::from_utf8_lossy(&body[..end]).into_owned());
            }
            b'g' => {}
            _ => {
                let path = long_name.take().unwrap_or(name);
                if kind == b'0' || kind == 0 || kind == b'7' {
                    files.push(Entry { path, data: body });
                }
            }
        }
        at += 512 + size.div_ceil(512) * 512;
    }
    Ok(files)
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
        let listed: Vec<_> = files.iter().map(|f| (f.path.as_str(), f.data)).collect();
        assert_eq!(
            listed,
            [
                ("dir/a.txt", &b"abc"[..]),
                ("long/name/of/file.txt", &b"xy"[..])
            ]
        );
    }
}
