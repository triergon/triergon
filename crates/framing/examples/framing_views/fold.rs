use triergon_harness_protocol::digest::{fold, fold_bytes};

#[derive(Default)]
pub(crate) struct Work {
    pub packets: u64,
    pub bytes: u64,
    pub digest: u64,
}

impl Work {
    pub fn record(&mut self, kind: u64, words: &[u64], payload: &[u8]) {
        fold(&mut self.digest, kind);
        for &word in words {
            fold(&mut self.digest, word);
        }
        if !payload.is_empty() {
            fold_bytes(&mut self.digest, payload);
        }
        self.packets += 1;
        self.bytes += payload.len() as u64;
    }

    pub fn iteration(&mut self, once: &Self) {
        self.packets += once.packets;
        self.bytes += once.bytes;
        fold(&mut self.digest, once.digest);
    }
}
