//! The differential harness: run the same operation through a Triergon worker
//! and an FFmpeg reference worker, compare canonical output, and grade the
//! results against the feature contract. Failures are recorded, never dropped.

pub mod acceptance;
pub mod build;
pub mod comparison;
pub mod diff;
pub mod linux;
#[cfg(test)]
mod linux_tests;
pub mod memory;
pub mod oracle;
pub mod parity;
pub mod provenance;
pub mod report;
pub mod screen;
#[cfg(test)]
mod tests;
pub mod timing;
pub mod vectors;
pub mod worker;
