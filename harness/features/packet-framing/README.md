# Packet framing

This contract compares complete AVC and HEVC packet-framing operations against
FFmpeg's public bitstream-filter API. All 131 scripts are required, including
configuration boundaries, state transitions, malformed input and packet metadata.
The inputs are authored synthetic structures; their codec bodies are opaque framing data.

The operation opens a fresh session, publishes configuration, processes every
packet, flush and drain action, closes the session and reads every retained packet
again. It folds exact signed packet fields, time bases, complete payloads, side
data and initialization or receive failures in order. Send/EAGAIN/EOF progress is
outside this output projection. `eof: true` means the complete script was consumed.

`inputs.json` holds the original script bytes as hexadecimal. `corpus.json` pins
each input and its recorded public-API observation. `observations.json` retains
those observations verbatim; the serial oracle derives both independent byte
polynomials from them. A word is eight little-endian bytes. Each iteration folds
both whole-operation digest words, including iterations with zero output packets.
Both workers fold eight-byte groups with the same polynomial recurrence and
process any trailing bytes individually.

`avc-profile-extension-inputs.json` adds the five fixed-header lengths and a
nonempty configuration update. `cargo xtask framing manifest --record` records
their packet counts and whole-operation folds from the built FFmpeg worker.

The xtask entry point generates all inputs and delegates reports, paired timing,
memory collection, isolation and gate-build checks to the shared harness. There
is no case filter. A baseline comparison cannot be an acceptance run. Workers
preload identical padded, immutable input templates before timing and memory
markers, and retain every output until after close. The Rust worker enables the
framing crate's `reference-trace` feature for comparison-only error codes.

`provenance.json` binds the inputs and observations to their original archive and
source hashes. The historical measurements used compiler overrides and are not
current gate results. Qualification requires the pinned stable build and all
131 cases on native AArch64 Linux.

[`fate.json`](fate.json) lists the Annex B byte projections of the upstream
AVC and HEVC framing cases. Build the ISO BMFF example and use an FFmpeg
binary with MOV demuxing and muxing, the AVC and HEVC Annex B filters, and
the H.264 metadata filter enabled.

```sh
CARGO_INCREMENTAL=0 cargo build --locked --release -p triergon-isobmff --example packet_annex_b
CARGO_INCREMENTAL=0 cargo xtask framing fate --ffmpeg /path/to/ffmpeg \
  --candidate target/release/examples/packet_annex_b \
  --output .research/framing-fate
```

The runner checks the input SHA-256 and upstream output MD5, then compares
the candidate and reference Annex B streams by SHA-256. It records every
case and returns failure if any case cannot be projected or differs. The
binary hashes and reference version are recorded in `report.json`. These
are byte projections of the framing operation. The upstream cases can also
check decoded frames or a second remux.
