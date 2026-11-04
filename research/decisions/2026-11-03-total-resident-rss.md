# Initial-delivery RSS criterion v2

On 2026-11-03 the owner approved softening the RSS gate in response to the
concrete total-peak-versus-operation-growth proposal in
`research/review/2026-11-03-rss-gate-choice/README.md`: “Let's optimise later and
slightly soften the gate now”. This decision adopts that proposed total-peak
criterion for initial delivery. It does not change runtime, heap or parity.

For every required case, retain all 30 paired measurements. Compute
`(candidate proven resident peak ceiling + 2 candidate pages) /
(reference observed resident peak - 2 reference pages)` for each pair. Apply
the existing paired median-ratio 95% interval; its upper bound must be strictly
less than 1.0. Do not substitute reference virtual peak or an unproved candidate
peak, remove the allowance, use only the median, or hide missing observations.

Complete selected-feature parity, runtime upper95 <= 1.25, strictly lower live
heap, all observation/calibration/provenance/isolation checks and all original
required cases remain hard requirements. Runtime upper95 <= 0.80 and strictly
lower operation-growth RSS remain optimization targets.

Frozen v1 collectors, raw receipts, claims, source/binary pins and original gate
verdicts are immutable. Re-evaluate admitted existing observations in a separate
v2 report without remeasurement. Original growth failures stay visible as
diagnostics. The implementation in `harness/delivery_memory_v2.py` performs only
arithmetic over previously validated measurements; it cannot admit measurements
or qualify a complete module by itself.

A passing selected campaign does not establish full Streem integration, target
proof or FFmpeg removal. The owner can tighten the diagnostic target later;
this decision grants no other waiver.
