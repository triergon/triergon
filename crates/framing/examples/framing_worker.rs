//! Whole packet-framing operations for the differential harness.
#![forbid(unsafe_code)]

#[path = "../../../harness/features/packet-framing/worker.rs"]
mod worker;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    worker::main(&std::env::args().skip(1).collect::<Vec<_>>())
}
