# O00 source responsibility inventory

The source-inventory milestone is complete for Streem revision
`9c7c0fb5f7316c92ab8086beb19db03d5855dd6e`. The parent reviewed the prepared
responsibility matrix and rechecked all **126 file hashes**, HEAD and clean worktree
status before accepting this source receipt. No Streem file was changed.

The inventory covers 50 dynamic FFmpeg imports, 119 call expressions, declared
release/development components, two simulator subprocess launch sites and the
capture conversion/encoder/mux path. The lexical scan examined 857 tracked
first-party source/build/tool files and identified 99 candidate files. These are
static source counts, not executed coverage or compiler reachability proof.

- [Source identities and review receipt](source-files.json)
- [Responsibility matrix](scope-matrix.md)
- [Dynamic symbols and call-site assignments](runtime-symbols.json)
- [Declared components and package responsibilities](enabled-components.json)
- [Open simulator/capture contracts](decoder-boundary-questions.md)
- [Integration contract drafts](draft-contracts.json)

`$STREEM_ROOT` denotes the caller-selected clean source checkout. Run
`python3 tools/check-streem-inventory.py --source /path/to/streem` to recheck this
receipt. A different revision needs a refreshed inventory, including newly added
uses; checking only the old file list is insufficient.

Configured auto-selected FFmpeg registries, built library/package contents, actual
simulator codecs/output and hardware behavior are not established by this source
review. Those remain O21/O30/O31/O40/O50/O60/O61/O62/O70 obligations. No decoder,
encoder, subtitle or format feature was removed from the offboarding scope. All
eleven implementation/integration/qualification/removal milestones remain open.
