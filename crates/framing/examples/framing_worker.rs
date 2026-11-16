//! Whole packet-framing operations for the differential harness.
#![forbid(unsafe_code)]

#[path = "../../../harness/features/packet-framing/worker.rs"]
mod worker;

fn main() -> std::process::ExitCode {
    worker::main(std::env::args_os())
}
