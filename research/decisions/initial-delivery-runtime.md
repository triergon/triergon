# Initial delivery runtime policy

On 2026-10-31 the owner selected a maximum execution-time ratio of **1.25**
against pinned FFmpeg for initial delivery. The **0.80** ratio remains the
optimization target. Both use the existing upper paired 95% confidence bound,
the same work, all required cases, and at least 30 pairs per case.

Full selected-feature parity and strictly lower live heap and attributable RSS
remain hard requirements. No input, profile, native discrepancy, source fact,
memory rejection or shipping-target requirement is removed by this decision.
This decision does not mark a complete module accepted.

The [machine-readable decision](initial-delivery-runtime.json) records every
working contract's before/after hash and the preserved requirements. Historical
0.80 qualification receipts remain immutable. Reclassification of retained first
timing measurements is a new policy assessment, not a new measurement or a rewrite
of a previous failure. The original timing and comparison-reference identities
remain attached to every assessment.

The first expanded-sprint optimization targets are the 24 existing cases above
1.25: four main AVC/HEVC workloads, eight seek workloads, eleven catalogue
workloads and the mandatory intact external HDR vector. Compiler vectorization,
compact data layout and zero-copy ownership are candidates to measure; they do
not authorize unsafe production code, hidden retained capacity, benchmark-only
paths or an altered reference workload.
