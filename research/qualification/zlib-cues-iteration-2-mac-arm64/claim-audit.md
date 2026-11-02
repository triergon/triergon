# Manual behavior/documentation claim audit

Scope: 64-byte Adler-32 lane updates, initialized fresh reference seeks, equivalent
polynomial expansion of the native timing workers' byte checksum, and the full
Streem offboarding target.

- Corrected README, implementation evidence, module gate registry and zlib/Cues doc
  so the earlier K0/K1 receipts qualify their archived source, not the later edits.
- Removed the unknown-Cluster case from the current unresolved-difference list and
  recorded the failing-before/passing-after reference initialization regression.
- Preserved all three other raw differences as failures. No case, timestamp,
  payload or tolerance was silently removed from the parity contract.
- Distinguished every enabled application runtime from the external reference
  oracle. Simulator playback and debug encode/mux/scale now have mandatory open
  milestones; neither is implicitly completed by release-TV offboarding.
- The checksum's polynomial, order, byte coverage, metadata/timestamp coverage and
  placement inside measured operations remain unchanged in both workers. Scalar
  Rust and independent Python trace tests verify its value. This is matched
  tooling optimization; new ratios require new measurements, and all original
  receipts are retained. No old timing numbers qualify the changed workers.
- Complete modules, deferred targets, decoder recovery on a real device and Streem
  integration remain unqualified. No shipping replacement or complete goal claim
  is made.

Current `make check`: worker test result 1 passed/0 failed; library 8 passed/0
failed; integration 24 passed/0 failed; Python 17 tests OK; clippy,
no-default-features, release build, safety and design checks passed. The log is
`.research/check-polynomial.log`; build/binary identities are in
`target/harness/identity.json`. This note is not a benchmark acceptance receipt.
