//! The differential harness: run the same operation through a Triergon worker
//! and an FFmpeg reference worker, compare canonical output, and grade the
//! results against the feature contract. Failures are recorded, never dropped.

pub mod acceptance;
pub mod comparison;
pub mod diff;
pub mod memory;
pub mod oracle;
pub mod parity;
pub mod provenance;
pub mod report;
#[cfg(test)]
mod tests;
pub mod timing;
pub mod worker;
