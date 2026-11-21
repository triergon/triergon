//! Generated golds the tests compare against: MP4 source packets, progressive fragment
//! packets and catalogue root facts.
#![allow(
    dead_code,
    reason = "generated golds; each test reads the records of its own inputs"
)]

pub(crate) mod mp4 {
    include!("../../fixtures/mp4_golds.rs");
}
pub(crate) mod progressive {
    include!("../../fixtures/progressive/source_golds.rs");
}
pub(crate) mod catalogue_roots {
    include!("../../fixtures/catalogue_root_facts.rs");
}
