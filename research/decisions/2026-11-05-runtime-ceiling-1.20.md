# Initial delivery runtime policy v2: ceiling 1.20

On 2026-11-05 the owner lowered the initial-delivery runtime ceiling from **1.25**
to **1.20** against pinned FFmpeg. This supersedes the ceiling in
[policy v1](initial-delivery-runtime.md) (2026-10-31) for every new assessment.
Everything else in v1 is unchanged:
- the upper paired 95% confidence bound;
- equal work, all required cases and at least 30 pairs per case;
- the **0.80** optimization target;
- full selected-feature parity;
- strictly lower live heap, and total resident peak RSS under its
  [own policy](2026-11-03-total-resident-rss.md).

The owner set this as the headline gate for the modular refactor programme
([ADR-0004](../../docs/refactor-plan.md)). The refactor must preserve behaviour,
remove net code and improve performance; it may not trade speed for structure.

## Effect on existing evidence

- Historical receipts, raw observations and their verdicts stay immutable. A receipt
  accepted under v1 records `initial-delivery-runtime-v1` and remains a v1 result.
- A case whose retained upper bound lies in (1.20, 1.25] passed v1 and does **not**
  pass v2. For example, the worst runtime upper bound in the
  [offboarding record](../../docs/offboarding-target.md) is 1.2397.
- A v2 classification of a retained measurement is a new policy assessment, not a new
  measurement. Fresh qualification of changed source uses v2 directly.

## Contract changes

The [machine-readable record](2026-11-05-runtime-ceiling-1.20.json) lists
before/after hashes. In each of the six working slice contracts, only
`runtime_ratio_max` (1.25 → 1.2) and `runtime_policy` (`initial-delivery-runtime-v1`
→ `-v2`) changed. `research/module-gates.json`, `research/offboarding-target.json`,
the zlib and seek contract generators, `tools/check-design.py`,
`harness/test_delivery_policy.py` and the capture gate (`harness/capture/qualify.py`)
carry the same value.
