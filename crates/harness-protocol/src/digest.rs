//! Ordered work digest: `h <- 31*h + v (mod 2^64)` per value.
//!
//! [`fold_bytes`] folds one value per byte. It is an exact algebraic expansion of
//! the sequential recurrence, regrouped so timing workers do not serialise on one
//! multiply chain; every byte contributes and the result equals the definition.

const STEP_8: u64 = 852_891_037_441; // 31^8
const STEP_32: u64 = 3_671_467_063_254_694_913; // 31^32 mod 2^64
const LANE_WEIGHTS: [u64; 3] = [
    13_349_005_091_172_011_265, // 31^24 mod 2^64
    12_662_688_596_514_954_753, // 31^16 mod 2^64
    STEP_8,
];

/// Fold one value.
#[inline]
pub fn fold(h: &mut u64, value: u64) {
    *h = h.wrapping_mul(31).wrapping_add(value);
}

/// Two's-complement bits of a signed field, as folded by both workers.
#[inline]
pub fn signed(value: i64) -> u64 {
    value as u64
}

/// Append a separately accumulated digest of `words` values.
#[inline]
pub fn append_words(h: &mut u64, digest: u64, words: u32) {
    *h = h
        .wrapping_mul(31u64.wrapping_pow(words))
        .wrapping_add(digest);
}

/// Fold the length, then every byte.
#[inline]
pub fn fold_framed(h: &mut u64, bytes: &[u8]) {
    fold(h, bytes.len() as u64);
    fold_bytes(h, bytes);
}

/// Fold every byte of `bytes` in order.
///
/// Timing-sensitive: both timing workers spend part of every measured operation
/// here. This exact loop form vectorises the four 32-byte-strided lanes; an
/// `as_chunks` rewrite measured about 10% slower on the AAC-heavy Matroska workloads,
/// so the slice-chunk form is kept deliberately.
#[allow(clippy::chunks_exact_to_as_chunks)]
pub fn fold_bytes(digest: &mut u64, bytes: &[u8]) {
    // Each four-byte group is below 2^23, so the narrow products stay exact.
    let four = |b: &[u8]| {
        u32::from(b[0]) * 29_791 + u32::from(b[1]) * 961 + u32::from(b[2]) * 31 + u32::from(b[3])
    };
    let eight = |b: &[u8]| u64::from(four(&b[..4])) * 923_521 + u64::from(four(&b[4..]));
    let bytes = if bytes.len() >= 256 {
        // Four independent recurrences, recombined at their exact polynomial
        // positions together with the incoming digest.
        let mut groups = bytes.chunks_exact(32);
        let mut exponent = groups.len();
        let mut power = 1_u64;
        let mut base = STEP_32;
        while exponent != 0 {
            if exponent & 1 != 0 {
                power = power.wrapping_mul(base);
            }
            base = base.wrapping_mul(base);
            exponent >>= 1;
        }
        let mut lanes = [0_u64; 4];
        for block in &mut groups {
            for (lane, b) in lanes.iter_mut().zip(block.chunks_exact(8)) {
                *lane = lane.wrapping_mul(STEP_32).wrapping_add(eight(b));
            }
        }
        *digest = digest
            .wrapping_mul(power)
            .wrapping_add(lanes[0].wrapping_mul(LANE_WEIGHTS[0]))
            .wrapping_add(lanes[1].wrapping_mul(LANE_WEIGHTS[1]))
            .wrapping_add(lanes[2].wrapping_mul(LANE_WEIGHTS[2]))
            .wrapping_add(lanes[3]);
        groups.remainder()
    } else {
        bytes
    };
    let mut groups = bytes.chunks_exact(8);
    for b in &mut groups {
        *digest = digest.wrapping_mul(STEP_8).wrapping_add(eight(b));
    }
    for b in groups.remainder() {
        fold(digest, u64::from(*b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequential(mut h: u64, bytes: &[u8]) -> u64 {
        for &byte in bytes {
            fold(&mut h, u64::from(byte));
        }
        h
    }

    #[test]
    fn expansion_equals_the_sequential_definition() {
        let data: Vec<u8> = (0..4099_u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for len in (0..300).chain([511, 512, 513, 1023, 1024, 4099]) {
            for seed in [0, 1, u64::MAX, 0x1234_5678_9abc_def0] {
                let mut h = seed;
                fold_bytes(&mut h, &data[..len]);
                assert_eq!(
                    h,
                    sequential(seed, &data[..len]),
                    "length {len}, seed {seed}"
                );
            }
        }
        let all = [0xff_u8; 777];
        let mut h = 7;
        fold_bytes(&mut h, &all);
        assert_eq!(h, sequential(7, &all));
    }

    #[test]
    fn golden_vectors() {
        // Pinned outputs shared with the C reference workers.
        let mut h = 0;
        fold_bytes(&mut h, b"triergon");
        assert_eq!(h, sequential(0, b"triergon"));
        assert_eq!(h, 0x2ff598039ee);
        let mut framed = 0;
        fold_framed(&mut framed, &[1, 2, 3]);
        assert_eq!(framed, ((3 * 31 + 1) * 31 + 2) * 31 + 3);
        assert_eq!(signed(-1), u64::MAX);
        let mut words = 5;
        append_words(&mut words, 9, 2);
        assert_eq!(words, 5 * 961 + 9);
    }
}
