# MPEG-TS packet inputs

Ten synthetic TS188 inputs for the `mpegts-pes` contract (`harness/features/mpegts-pes/`).
The contract's other inputs are the authored streams under
[`crates/mpegts/tests/data/`](../../../crates/mpegts/tests/data/manifest.json). Like
those, these were constructed byte by byte from ITU-T H.222.0: no encoder, upstream media or other implementation is involved, and the
payloads are opaque markers rather than decodable media.

| Inputs | Construction |
|---|---|
| `duplicate-middle-low-occupancy-{finite,unbounded}.ts` | PAT and PMT, then one AVC PES with a 400-byte body, finite or unbounded, in five packets: the second of the two partly filled leading packets is repeated with the same continuity counter |
| `finite-continuation-body-{400,401,408,409,414,415}.ts` | PAT and PMT, then one finite AVC PES with a 400-byte body whose third and last packet brings the carried body to the named length, so all but the first carry bytes beyond the declared PES |
| `pat-pointer-then-clean-pusi.ts` | `psi-pointer-overrun.ts` (a PAT whose pointer field overruns its payload) followed by `ts188-basic.ts` with the PAT continuity counter advanced |
| `pmt-pointer-then-clean-pusi.ts` | `ts188-basic.ts` with an overrunning-pointer section inserted on the PMT PID before its PMT, whose continuity counter is advanced |

`corpus.json` pins each file by SHA-256.
