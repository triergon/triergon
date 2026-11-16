//! Decoder measurement worker with the AAC decode operations alone: `audio-aac` and
//! the access-unit operations. The gate build builds it with the AAC decoder's `simd`
//! feature, by the pinned nightly; every other operation stays with `codec-worker`.
#![forbid(unsafe_code)]

fn main() {
    codec_worker::main("codec-aac-worker", codec_worker::Operations::Aac);
}
