//! The differential harness: run the same operation through a Triergon worker
//! and a reference worker (FFmpeg or FreeType), compare canonical output, and grade the
//! results against the feature contract. Failures are recorded, never dropped.

pub mod acceptance;
pub mod build;
pub mod comparison;
pub mod diff;
pub mod features;
pub mod gate_build;
pub mod grade;
pub mod linux;
#[cfg(test)]
mod linux_tests;
pub mod manifest;
pub mod memory;
pub mod oracle;
pub mod parity;
pub mod policy;
pub mod provenance;
pub mod report;
pub mod reuse;
pub mod screen;
#[cfg(test)]
mod tests;
pub mod timing;
pub mod validation;
pub mod vectors;
pub mod worker;
