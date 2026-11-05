// Original Source-only physical top-level facts, independently prepared with Python.
// Full SHA-256 values are in ROOT-PHYSICAL-PROOFS.json; Rust checks exact bytes plus FNV-1a.
pub struct Root {
    pub start: u64,
    pub end: u64,
    pub kind: [u8; 4],
    pub header: u8,
    pub fold: u64,
}
pub struct Fixture {
    pub length: usize,
    pub fold: u64,
    pub roots: &'static [Root],
}
pub static NO_SECOND_TFDT: Fixture = Fixture {
    length: 971,
    fold: 8269190648279780950,
    roots: &[Root {
        start: 799,
        end: 927,
        kind: *b"moof",
        header: 8,
        fold: 6548602352357682690,
    }],
};
pub static SECOND_MOOV_REFUSAL: Fixture = Fixture {
    length: 1574,
    fold: 10336056661822857704,
    roots: &[Root {
        start: 799,
        end: 947,
        kind: *b"moof",
        header: 8,
        fold: 776119288425038210,
    }],
};
