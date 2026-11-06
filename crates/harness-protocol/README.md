# triergon-harness-protocol

The trace protocol of the differential harness. The Rust workers (`media-worker`,
`codec-worker`, `playback-worker`) and the trace examples and tests of the format and
codec crates print one JSON document per operation; this crate holds the field
encodings and digests they share with the FFmpeg reference worker
(`harness/reference.c`), so both sides of a comparison produce byte-identical traces
for equal behaviour. It is tooling: library crates use it only as a dev-dependency, and
it has no dependencies of its own.

## Usage

```rust
use triergon_harness_protocol::{digest, hex, quoted, unhex};

assert_eq!(hex(&[0x0f, 0xa0]), "0fa0");
assert_eq!(unhex("0FA0"), [0x0f, 0xa0]);
assert_eq!(quoted("a\"b\n"), r#""a\"b\n""#);

// Ordered work digest: h <- 31*h + v (mod 2^64).
let mut h = 0;
digest::fold(&mut h, 3);
digest::fold_framed(&mut h, b"abc"); // the length, then every byte
assert_eq!(h, 2_956_290); // 3, 3, 97, 98, 99 folded in order
```

## Contents

| Item | What it does |
|---|---|
| `push_hex`, `hex`, `write_hex` | Byte fields as lower-case hex |
| `unhex` | Decodes authored test-vector hex; `~` and `-` mean an empty payload. Panics on odd length or a non-hex digit |
| `push_quoted`, `quoted`, `write_quoted` | JSON string literals, escaping `"`, `\` and control characters |
| `digest` | Ordered work digest `h <- 31*h + v (mod 2^64)` over every field and payload byte, never sampled. Timed operations use it to prove both sides did equal work. `fold_bytes` regroups the per-byte recurrence for speed and equals it exactly; golden-vector tests pin the values `harness/reference.c` produces |
| `interleaved` | `InterleavedDigest`, the output fingerprint of the decode workers: eight lanes over 64-byte blocks folded into the ordered digest, then the byte count. It detects unequal output; it is not a cryptographic hash |
| `md5` | RFC 1321 MD5, for comparing decoded output with recorded `ffmpeg -f md5` and framemd5 digests. Not a security primitive |

The harness and its comparison rules are described in
[docs/quality.md](../../docs/quality.md).
