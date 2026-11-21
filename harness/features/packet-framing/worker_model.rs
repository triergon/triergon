//! Caller-owned, preloaded immutable input. Codec parsing remains in each operation.
use triergon_core::{Budget, Bytes, Error, Result};
use triergon_framing::nal::{Codec, NalPacketFields};

pub(super) const CAP: usize = 16 * 1024 * 1024;

pub enum Event {
    Packet {
        fields: NalPacketFields,
        bytes: Bytes,
        extra: Option<Bytes>,
        offset: u64,
        extra_offset: u64,
    },
    Flush,
    Drain,
}

pub struct Case {
    pub pool: Budget,
    pub codec: Codec,
    pub configuration: Bytes,
    pub config_offset: u64,
    pub events: Vec<Event>,
    pub baseline: usize,
}

struct Reader<'a> {
    raw: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Overflow)?;
        let part = self.raw.get(self.at..end).ok_or(Error::Truncated)?;
        self.at = end;
        Ok(part)
    }
    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error::Truncated)
    }
    fn len32(&mut self) -> Result<usize> {
        usize::try_from(u32::from_be_bytes(self.fixed()?)).map_err(|_| Error::Overflow)
    }
    fn blob(&mut self, pool: &Budget, n: usize) -> Result<Bytes> {
        if n > CAP {
            return Err(Error::Limit {
                requested: n,
                maximum: CAP,
            });
        }
        let raw = self.take(n)?;
        let padded_len = n + 64;
        let mut padded = Vec::new();
        padded
            .try_reserve_exact(padded_len)
            .map_err(|_| Error::Allocation)?;
        padded.extend_from_slice(raw);
        padded.resize(padded_len, 0);
        let storage = Bytes::try_from_vec(pool, padded).map_err(|e| e.error())?;
        storage.slice(0..n)
    }
}

impl Case {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() > CAP {
            return Err(Error::Limit {
                requested: raw.len(),
                maximum: CAP,
            });
        }
        let pool = Budget::new(64 * 1024 * 1024)?;
        let mut r = Reader { raw, at: 0 };
        if r.take(8)? != b"SMBSF001" {
            return Err(Error::Invalid("case magic"));
        }
        let codec = match u8::from_be_bytes(r.fixed()?) {
            1 => Codec::Avc,
            2 => Codec::Hevc,
            _ => return Err(Error::Invalid("case codec")),
        };
        let config_len = r.len32()?;
        let config_offset = r.at as u64;
        let configuration = r.blob(&pool, config_len)?;
        let count = r.len32()?;
        if count > 64 {
            return Err(Error::Limit {
                requested: count,
                maximum: 64,
            });
        }
        let mut events = Vec::new();
        events
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        for _ in 0..count {
            let event = match u8::from_be_bytes(r.fixed()?) {
                1 => Event::Flush,
                2 => Event::Drain,
                0 => {
                    let fields = NalPacketFields {
                        pts: i64::from_be_bytes(r.fixed()?),
                        dts: i64::from_be_bytes(r.fixed()?),
                        duration: i64::from_be_bytes(r.fixed()?),
                        position: i64::from_be_bytes(r.fixed()?),
                        flags: u32::from_be_bytes(r.fixed()?),
                        stream_index: i32::from_be_bytes(r.fixed()?),
                    };
                    let size = r.len32()?;
                    let extra_size = u32::from_be_bytes(r.fixed()?);
                    let offset = r.at as u64;
                    let bytes = r.blob(&pool, size)?;
                    let extra_offset = r.at as u64;
                    let extra = if extra_size == u32::MAX {
                        None
                    } else {
                        let length = usize::try_from(extra_size).map_err(|_| Error::Overflow)?;
                        Some(r.blob(&pool, length)?)
                    };
                    Event::Packet {
                        fields,
                        bytes,
                        extra,
                        offset,
                        extra_offset,
                    }
                }
                _ => return Err(Error::Invalid("case event")),
            };
            events.push(event);
        }
        if r.at != raw.len() {
            return Err(Error::Invalid("trailing case bytes"));
        }
        let baseline = pool.used();
        Ok(Self {
            pool,
            codec,
            configuration,
            config_offset,
            events,
            baseline,
        })
    }
}

#[inline(always)]
fn group_four<const BASE: u32>(group: [u8; 4]) -> u32 {
    let [a, b, c, d] = group.map(u32::from);
    (a * BASE.pow(3)) + (b * BASE.pow(2)) + (c * BASE) + d
}

#[inline(always)]
fn group_eight<const BASE: u32>(first: [u8; 4], last: [u8; 4]) -> u64 {
    // Four bytes fit in u32 for each of the two bases. Widen only when
    // combining the halves so 32-bit targets can use a widening multiply.
    u64::from(group_four::<BASE>(first)) * u64::from(BASE.pow(4))
        + u64::from(group_four::<BASE>(last))
}

// Fold each polynomial in its own call on 32-bit ARM. Folded together, the
// compiler pairs the two sums in vector lanes there, and every group then
// moves between vector and general registers: the fold took 1.5 times the
// cycles of the reference's identical C fold. On AArch64 the paired form is
// the faster one.
const SEPARATE_FOLDS: bool = cfg!(target_arch = "arm");

// One polynomial over words, each one eight-byte group of its little-endian
// bytes, and then over bytes, in eight-byte groups and then single bytes.
#[inline(never)]
fn fold<const BASE: u32>(mut sum: u64, words: &[u64], bytes: &[u8]) -> u64 {
    let group = |sum: u64, [c0, c1, c2, c3, c4, c5, c6, c7]: [u8; 8]| {
        sum.wrapping_mul(u64::from(BASE).pow(8))
            .wrapping_add(group_eight::<BASE>([c0, c1, c2, c3], [c4, c5, c6, c7]))
    };
    for word in words {
        sum = group(sum, word.to_le_bytes());
    }
    let (chunks, rest) = bytes.as_chunks::<8>();
    for &chunk in chunks {
        sum = group(sum, chunk);
    }
    for &byte in rest {
        sum = sum
            .wrapping_mul(u64::from(BASE))
            .wrapping_add(u64::from(byte));
    }
    sum
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digest(pub u64, pub u64);
impl Digest {
    #[inline(always)]
    fn group(&mut self, chunk: &[u8; 8]) {
        let &[a, b, c, d, e, f, g, h] = chunk;
        let first = [a, b, c, d];
        let last = [e, f, g, h];
        self.0 = self
            .0
            .wrapping_mul(31_u64.pow(8))
            .wrapping_add(group_eight::<31>(first, last));
        self.1 = self
            .1
            .wrapping_mul(131_u64.pow(8))
            .wrapping_add(group_eight::<131>(first, last));
    }
    /// A word is exactly one eight-byte group of its little-endian bytes.
    // Initialization, packet metadata, and teardown all emit the same word.
    pub fn word(&mut self, n: u64) {
        self.group(&n.to_le_bytes());
    }
    #[inline]
    pub fn signed(&mut self, n: i64) {
        self.word(n as u64);
    }
    pub fn blob(&mut self, bytes: &[u8]) {
        self.fold(&[bytes.len() as u64], bytes);
    }
    /// Words, then bytes, in one fold call. The same wrapping polynomial fold
    /// as one byte at a time: eight-byte groups use h*k^8 + sum(b[i]*k^(7-i)),
    /// which is equal modulo 2^64 and shortens the chain.
    #[inline(always)]
    pub fn fold(&mut self, words: &[u64], bytes: &[u8]) {
        if SEPARATE_FOLDS {
            self.0 = fold::<31>(self.0, words, bytes);
            self.1 = fold::<131>(self.1, words, bytes);
            return;
        }
        for &word in words {
            self.group(&word.to_le_bytes());
        }
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.group(chunk);
        }
        for &b in rest {
            self.0 = self.0.wrapping_mul(31).wrapping_add(u64::from(b));
            self.1 = self.1.wrapping_mul(131).wrapping_add(u64::from(b));
        }
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub digest: Digest,
    pub packets: u64,
    pub bytes: u64,
}

#[cfg(test)]
mod digest_tests {
    use super::Digest;

    fn serial(start: Digest, bytes: &[u8]) -> Digest {
        let mut d = start;
        for &b in bytes {
            d.0 = d.0.wrapping_mul(31).wrapping_add(u64::from(b));
            d.1 = d.1.wrapping_mul(131).wrapping_add(u64::from(b));
        }
        d
    }

    #[test]
    fn grouped_fold_equals_byte_fold() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let data: Vec<u8> = (0..4099)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for start in [Digest(0, 0), Digest(u64::MAX, 1), Digest(seed, !seed)] {
            for len in (0..40).chain([255, 256, 1023, 4099]) {
                for offset in 0..3 {
                    let bytes = &data[offset..offset + len.min(data.len() - offset)];
                    let mut grouped = start;
                    grouped.fold(&[], bytes);
                    assert_eq!(grouped, serial(start, bytes));
                }
            }
            let mut grouped = start;
            grouped.fold(&[], &[0xff; 64]);
            assert_eq!(grouped, serial(start, &[0xff; 64]));
            // Words fold as their little-endian bytes, before the bytes.
            let words = [seed, u64::MAX, 0, 90000];
            let mut joined: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            joined.extend_from_slice(&data[..13]);
            let mut grouped = start;
            grouped.fold(&words, &data[..13]);
            assert_eq!(grouped, serial(start, &joined));
        }
    }
}
