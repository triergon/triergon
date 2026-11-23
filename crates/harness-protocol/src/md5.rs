//! MD5 (RFC 1321) for comparing decoded output with recorded reference digests
//! (`ffmpeg -f md5`/framemd5 style). Test tooling only; not a security primitive.

const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The per-step constants, the integer part of `2^32 * |sin(i + 1)|` (RFC 1321, 3.4).
const SINES: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// MD5 digest of `data`.
pub fn digest(data: &[u8]) -> [u8; 16] {
    let mut h: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut message = data.to_vec();
    message.push(0x80);
    message.resize((message.len() + 8).next_multiple_of(64) - 8, 0);
    message.extend_from_slice(&(data.len() as u64).wrapping_mul(8).to_le_bytes());
    for block in message.as_chunks::<64>().0 {
        let m: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ])
        });
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(SINES[i])
                .wrapping_add(m[g])
                .rotate_left(SHIFTS[i]);
            (a, d, c, b) = (d, c, b, b.wrapping_add(rotated));
        }
        for (state, value) in h.iter_mut().zip([a, b, c, d]) {
            *state = state.wrapping_add(value);
        }
    }
    let mut out = [0; 16];
    for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *bytes = word.to_le_bytes();
    }
    out
}

/// Lower-case hex MD5 of `data`.
pub fn hex_digest(data: &[u8]) -> String {
    crate::hex(&digest(data))
}

#[cfg(test)]
mod tests {
    use super::hex_digest;

    #[test]
    fn rfc_1321_vectors() {
        assert_eq!(hex_digest(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex_digest(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex_digest(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        let long = vec![b'a'; 1_000];
        for len in [55, 56, 57, 63, 64, 65, 119, 120, 1_000] {
            assert_eq!(hex_digest(&long[..len]).len(), 32);
        }
        assert_eq!(hex_digest(&long[..56]), "3b0c8ac703f828b04c6c197006d17218");
        assert_eq!(hex_digest(&long[..64]), "014842d480b571495a4a0363793f7367");
    }
}
