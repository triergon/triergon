# RSS gate comparison — approved 2026-11-03

The owner approved the proposed total-peak criterion. The current versioned policy is [recorded here](../../decisions/2026-11-03-total-resident-rss.md). The proposal below is the preserved pre-approval review; statements that no gate had changed refer to that earlier review state. The original112 growth failures remain visible.

No gate has changed. The current 4,173-file Matroska campaign still fails 112 of 116 required attributable-RSS cases. Every original result remains preserved, and no failed worker was retimed.

The current contract gates RSS growth during the operation: candidate peak ceiling minus its starting resident memory, divided by reference observed peak minus its own starting memory. This answers how much resident memory the operation adds after its measurement window starts. It differs from the total resident memory required to run the operation.

In the fresh 116-case campaign, all 116 runtime delivery cases and all 116 live-heap cases pass. The runtime median is0.918192 of FFmpeg; the worst paired95 upper bound is1.228626. Only 4 RSS-growth cases pass. In the old/new records inspected, the Rust growth is stable while FFmpeg's starting resident memory varies; the cause of that variation is still under investigation.

As a separate recorded-data diagnostic, all 116 cases also have a strictly lower total resident peak under the same paired95 calculation. This comparison uses the Rust proven peak ceiling plus two pages, divided by FFmpeg's observed peak minus two pages. The worst upper bound is 0.9633802816901409. No source frame, work field, corpus member, observer control or confidence calculation is removed. It is an additional interpretation of the existing measurements, not a new measurement or a current qualification.

For one actual avc-aac-sub observation, the same records show:

| Memory | Rust | FFmpeg |
|---|---:|---:|
| Resident memory at window start | 2.066 MiB | 2.766 MiB |
| Resident peak during operation | 2.504 MiB | 2.961 MiB |
| Growth over that starting value | 0.438 MiB | 0.195 MiB |

Rust uses less total RAM in this observation but adds more relative to its lower starting value. Across the paired records, FFmpeg's starting value also varies. These are different memory questions; the arithmetic does not establish the cause of that variation.

The decision is whether the RSS hard gate should continue to require lower operation growth, or instead require lower total resident peak with the same conservative page allowance. Replacing it would change the frozen criterion and allow the 112 currently failing Matroska growth cases under that new criterion; it must not be applied without the owner's explicit decision. Runtime ≤ 1.25, the 0.80 optimization target, strictly lower live heap, complete selected-feature parity, all receipt/proof checks and all historical failures would remain. The original growth results would stay visible as diagnostics. This would accept only the 116-case selected campaign, never the full Streem offboarding scope.

Evidence: `.research/m1-current4173-de-qualification-v1/MEMBERSHIPS116-FINAL-v1.json`, `TOTAL-RSS-DIAGNOSTIC-v1.json`, `TOTAL-RSS-WITH-ALLOWANCE-PROPOSAL-DATA-v1.json`, `RSS-RECORDED-FIELD-CONTRAST-v1.json` and `FINAL-HANDOFF-v1.md`. All numbers are from existing native Linux ARM64 records. No Mac benchmark, extra native collection or formula change has been credited.
