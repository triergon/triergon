# MPEG-TS packet inputs

Ten synthetic TS188 inputs for the `mpegts-pes` contract (`harness/features/mpegts-pes/`).
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
