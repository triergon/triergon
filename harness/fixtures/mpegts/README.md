# MPEG-TS packet inputs

Twelve synthetic TS188 inputs for the `mpegts-pes` contract (`harness/features/mpegts-pes/`).
The contract's other inputs are the authored streams under
[`crates/mpegts/tests/data/`](../../../crates/mpegts/tests/data/manifest.json). Like
those, these were constructed byte by byte from ITU-T H.222.0, with no encoder, upstream
media or other implementation; the payloads are opaque markers, not decodable media.

| Inputs | Construction |
|---|---|
| `duplicate-middle-low-occupancy-{finite,unbounded}.ts` | PAT and PMT, then one AVC PES with a 400-byte body, finite or unbounded, in five packets: the second of the two partly filled leading packets is repeated with the same continuity counter |
| `finite-continuation-body-{400,401,408,409,414,415}.ts` | PAT and PMT, then one finite AVC PES with a 400-byte body whose third and last packet brings the carried body to the named length, so all but the first carry bytes beyond the declared PES |
| `pat-pointer-then-clean-pusi.ts` | `psi-pointer-overrun.ts` (a PAT whose pointer field overruns its payload) followed by `ts188-basic.ts` with the PAT continuity counter advanced |
| `pmt-pointer-then-clean-pusi.ts` | `ts188-basic.ts` with an overrunning-pointer section inserted on the PMT PID before its PMT, whose continuity counter is advanced |

The contract's `corpus.json` pins each file by SHA-256.

`dts-core-mono.dts` holds 12 canonical 512-byte DTS core frames of a synthetic sine
wave, under the repository's MIT or Apache-2.0 terms; the DTS transport controls rewrap
them without decoding (SHA-256
`a1db20abc4bce08a504108de2998337f4d906eda3373acdc3abab39fcaaf1330`).
`free-format-mono.mp3` is the MPEG-1 Layer III stream the `mpegts-free-audio` inputs
rewrap (`xtask/src/transport_fixtures/matched.rs` pins it).

```sh
ffmpeg -f lavfi -i 'sine=frequency=523:sample_rate=48000:duration=0.128' \
  -c:a dca -strict experimental -b:a 384k -f dts dts-core-mono.dts
```

The VVC controls `vvc-finite.ts` and `vvc-unbounded.ts` each contain five TS188
packets (940 bytes): PAT, PMT naming stream type `0x33`, and three PES on PID
256 with stream ID `0xe0`. The payloads are `00 00 01 00 NN 0f ff f8`, where
NN is 00, 01, 02. PTS/DTS are (3003, 0), (6006, 3003), (9009, 6006).
The finite PES lengths are 21; the unbounded lengths are zero, completed by
the next PUSI or final EOF. No payload is a claim of decodable VVC.

Regenerate the inputs and verify their existing pins with:

```sh
CARGO_INCREMENTAL=0 cargo xtask fixture transport-vvc --output .
```

To record against an existing pinned formats build, add
`--record REFERENCE_ROOT --observations PRIVATE_DIR`. The recorder verifies the
formats worker hash against that root's `target/harness/identity.json` and requires
revision `d32b387f2b0a484599d4587d651891f0c63c4238`. It saves raw outputs outside
git, requires actual EOF, and changes only the two VVC rows and required profiles.
The original 97 rows and all their pins stay unchanged; the AU corpus continues
to use exactly those original rows and 272 request associations.

The two functional folds can also be reproduced directly after
`CARGO_INCREMENTAL=0 cargo build -p transport-worker` (adjust the candidate path
when using `CARGO_TARGET_DIR`):

```sh
for kind in finite unbounded; do
  /tmp/triergon-ts-dts-ci/target/harness/ffmpeg-formats-worker ts-fold \
    harness/fixtures/mpegts/vvc-$kind.ts 188 7
  target/debug/transport-worker ts-fold harness/fixtures/mpegts/vvc-$kind.ts 188 7
done
```

Every output must report `operation: mpegts-pes-packets`, `iterations: 1`,
`packets: 3`, `bytes: 24`, `digest: 1768033906666164040`, and `eof: true`.
The existing required report comparison runs all 99 cases and folds every
packet field, payload byte and side-data byte in order.
