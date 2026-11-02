# Manual behavior-claim and native-reference audit

Reviewed README, implementation/testing/scope/architecture/licensing prose,
specification feature mapping, FATE rules and module-gate registry against current
source and raw receipts. Corrected stale unsupported/pending zlib and Cues claims,
historical/current snapshot wording, old FATE support statuses and overbroad
library-hash wording. No remaining contradiction was found in reviewed current
behavior claims. Missing broader module features remain explicitly pending.

All 34 declared profiles remain present. Four exact parity failures and seven
matching zlib speed failures remain failures, including 0.8004435808978564. No
packet/payload normalization, case removal, tolerance, threshold change or checksum
work change was used to qualify them. Previous source/runtime failures are kept as
historical evidence. No full module, decoder, Streem or TV acceptance is claimed.

The owned Rust workspace still forbids unsafe code and has no third-party packages
or build scripts. Host regressions cover short reads, truncation/fault versus EOF,
seek retries/EOF/cancellation, malformed and extreme indexes, sparse tail lookup,
private metadata aggregate limits, laced decoded capacity, zlib checksum/trees,
window/history/overlap, expansion/output/work and deterministic mutations.

Native reference changes are confined to separate tooling. Public n9.0 headers
match the linked pinned n9.0 archives; AVCodecParameters.seek_preroll is converted
from samples to nanoseconds, and backward seeking uses public av_seek_frame.
System zlib linkage/version and OS build are recorded. No production Rust FFI or
webOS startup/linkage change was introduced. The native reference and actual
upstream FATE paths were executed; demux-only evidence is labeled separately.

Current source/build-input hashes and raw gate verdicts are checked before handoff.
The camera was resumed. No commit, push, deployment or Streem edit was made.
