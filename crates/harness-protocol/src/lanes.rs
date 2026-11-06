//! Eight-lane output digest of the D4 workers (`codec-worker`, `harness/d4/reference_decode.c`).
//!
//! Over the concatenated output in 64-byte blocks, lane `i` takes
//! `lane_i <- (lane_i ^ le64(word 8k+i)) * K`; a short final block is zero-padded.
//! The result is `h <- 31*h + lane_i` over the eight lanes, then `31*h + byte count`.
//! Every output byte enters it at about 1% of the decode cost; it is a fingerprint of
//! equal output, not a cryptographic hash.

const K: u64 = 0x9E37_79B9_7F4A_7C15;

/// Streaming state; feed with [`LaneDigest::update`], read with [`LaneDigest::finish`].
#[derive(Clone, Debug, Default)]
pub struct LaneDigest {
    lanes: [u64; 8],
    total: u64,
    pending: Vec<u8>,
}

impl LaneDigest {
    fn block(lanes: &mut [u64; 8], block: &[u8]) {
        for (lane, word) in lanes.iter_mut().zip(block.chunks_exact(8)) {
            *lane = (*lane ^ u64::from_le_bytes(word.try_into().expect("8 bytes"))).wrapping_mul(K);
        }
    }

    /// Append `bytes` to the digested output.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if !self.pending.is_empty() {
            let take = (64 - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() < 64 {
                return;
            }
            Self::block(&mut self.lanes, &self.pending);
            self.pending.clear();
        }
        let blocks = bytes.chunks_exact(64);
        self.pending.extend_from_slice(blocks.remainder());
        for block in blocks {
            Self::block(&mut self.lanes, block);
        }
    }

    /// The digest of everything appended so far.
    pub fn finish(&self) -> u64 {
        let mut lanes = self.lanes;
        if !self.pending.is_empty() {
            let mut last = [0_u8; 64];
            last[..self.pending.len()].copy_from_slice(&self.pending);
            Self::block(&mut lanes, &last);
        }
        let h = lanes
            .iter()
            .fold(0_u64, |h, &lane| h.wrapping_mul(31).wrapping_add(lane));
        h.wrapping_mul(31).wrapping_add(self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(chunks: &[&[u8]]) -> u64 {
        let mut d = LaneDigest::default();
        for chunk in chunks {
            d.update(chunk);
        }
        d.finish()
    }

    #[test]
    fn golden_vectors() {
        // Pinned from the codec-worker implementation the D4 receipts recorded.
        let data: Vec<u8> = (0..1000_u32).map(|i| (i * 7 + i / 13) as u8).collect();
        assert_eq!(digest(&[]), GOLDEN[0]);
        assert_eq!(digest(&[b"triergon"]), GOLDEN[1]);
        assert_eq!(digest(&[&data[..64]]), GOLDEN[2]);
        assert_eq!(digest(&[&data]), GOLDEN[3]);
    }

    #[test]
    fn chunking_does_not_change_the_digest() {
        let data: Vec<u8> = (0..1000_u32).map(|i| (i * 7 + i / 13) as u8).collect();
        for split in [1, 7, 63, 64, 65, 128, 999] {
            let (a, b) = data.split_at(split);
            assert_eq!(digest(&[a, b]), GOLDEN[3], "split at {split}");
        }
        let bytes: Vec<&[u8]> = data.chunks(1).collect();
        assert_eq!(digest(&bytes), GOLDEN[3]);
    }

    const GOLDEN: [u64; 4] = [
        0,
        0xd176_81f6_47a4_cf8c,
        0xe482_5679_ee25_2376,
        0xffb0_d3e7_a5fe_bb02,
    ];
}
