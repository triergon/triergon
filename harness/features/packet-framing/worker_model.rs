//! Caller-owned, preloaded immutable input. Codec parsing remains in each operation.
use triergon_core::{Budget, Bytes, Error, Result};
use triergon_framing::{Codec, NalPacketFields};

const CAP: usize = 16 * 1024 * 1024;

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
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Overflow)?;
        let part = self.raw.get(self.at..end).ok_or(Error::Truncated)?;
        self.at = end;
        Ok(part)
    }
    fn word(&mut self, n: usize) -> Result<u64> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0, |a, b| (a << 8) | u64::from(*b)))
    }
    fn blob(&mut self, pool: &Budget, n: usize) -> Result<Bytes> {
        if n > CAP {
            return Err(Error::Limit {
                requested: n,
                maximum: CAP,
            });
        }
        // Same caller padding guarantee as the native input templates. The
        // logical Bytes view excludes padding and retains the charged storage.
        let source = self.take(n)?;
        let mut padded = Vec::new();
        padded
            .try_reserve_exact(n.checked_add(64).ok_or(Error::Overflow)?)
            .map_err(|_| Error::Allocation)?;
        padded.extend_from_slice(source);
        padded.resize(n + 64, 0);
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
        let codec = match r.word(1)? {
            1 => Codec::Avc,
            2 => Codec::Hevc,
            _ => return Err(Error::Invalid("case codec")),
        };
        let config_len = usize::try_from(r.word(4)?).map_err(|_| Error::Overflow)?;
        let config_offset = r.at as u64;
        let configuration = r.blob(&pool, config_len)?;
        let count = usize::try_from(r.word(4)?).map_err(|_| Error::Overflow)?;
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
            let event = match r.word(1)? {
                1 => Event::Flush,
                2 => Event::Drain,
                0 => {
                    let fields = NalPacketFields {
                        pts: r.word(8)? as i64,
                        dts: r.word(8)? as i64,
                        duration: r.word(8)? as i64,
                        position: r.word(8)? as i64,
                        flags: r.word(4)? as u32,
                        stream_index: r.word(4)? as i32,
                    };
                    let size = usize::try_from(r.word(4)?).map_err(|_| Error::Overflow)?;
                    let extra_size = r.word(4)?;
                    let offset = r.at as u64;
                    let bytes = r.blob(&pool, size)?;
                    let extra_offset = r.at as u64;
                    let extra = if extra_size == u64::from(u32::MAX) {
                        None
                    } else {
                        Some(r.blob(
                            &pool,
                            usize::try_from(extra_size).map_err(|_| Error::Overflow)?,
                        )?)
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

const fn powers(base: u64) -> [u64; 9] {
    let mut out = [1u64; 9];
    let mut i = 1;
    while i < 9 {
        out[i] = out[i - 1].wrapping_mul(base);
        i += 1;
    }
    out
}
const POW31: [u64; 9] = powers(31);
const POW131: [u64; 9] = powers(131);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digest(pub u64, pub u64);
impl Digest {
    /// Same wrapping polynomial fold as one byte at a time. Eight-byte groups use
    /// h*k^8 + sum(b[i]*k^(7-i)), which is equal modulo 2^64 and shortens the chain.
    #[inline]
    pub fn raw(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.group(chunk);
        }
        for &b in rest {
            self.0 = self.0.wrapping_mul(31).wrapping_add(u64::from(b));
            self.1 = self.1.wrapping_mul(131).wrapping_add(u64::from(b));
        }
    }
    #[inline(always)]
    fn group(&mut self, chunk: &[u8; 8]) {
        let (mut a, mut b) = (0u64, 0u64);
        for (i, &byte) in chunk.iter().enumerate() {
            a = a.wrapping_add(u64::from(byte).wrapping_mul(POW31[7 - i]));
            b = b.wrapping_add(u64::from(byte).wrapping_mul(POW131[7 - i]));
        }
        self.0 = self.0.wrapping_mul(POW31[8]).wrapping_add(a);
        self.1 = self.1.wrapping_mul(POW131[8]).wrapping_add(b);
    }
    /// A word is exactly one eight-byte group of its little-endian bytes.
    #[inline]
    pub fn word(&mut self, n: u64) {
        self.group(&n.to_le_bytes());
    }
    #[inline]
    pub fn signed(&mut self, n: i64) {
        self.group(&n.to_le_bytes());
    }
    pub fn blob(&mut self, bytes: &[u8]) {
        self.word(bytes.len() as u64);
        self.raw(bytes);
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
                    grouped.raw(bytes);
                    assert_eq!(grouped, serial(start, bytes));
                }
            }
            let mut grouped = start;
            grouped.raw(&[0xff; 64]);
            assert_eq!(grouped, serial(start, &[0xff; 64]));
        }
    }
}
