//! Decoder measurement worker with every operation: the session decoders' decode
//! operations and the container readers' `packets` and `seek` commands.
#![forbid(unsafe_code)]

fn main() {
    codec_worker::main("codec-worker", codec_worker::Operations::All);
}
