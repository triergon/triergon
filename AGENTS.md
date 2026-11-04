# triergon project constraints

This is an experimental implementation/research repository. Read README.md,
docs/implementation.md, docs/scope.md, docs/architecture.md, docs/dependency-policy.md,
docs/testing.md and specs/README.md before implementation.

- Bootstrap the independent core from specifications. Do not mechanically translate or copy
  FFmpeg, GPL Streem code, or another framework into newly licensed files.
- Production code and the complete selected third-party graph must be pure safe Rust. Enforce
  forbid(unsafe_code) in owned crates; that lint does not audit dependencies. std/compiler/OS
  are the explicit foundation. Native reference tools and evaluation candidates are isolated
  research tooling, not production dependencies or approved safety exceptions.
- Every dependency needs an exact-fit and bootstrap-versus-reuse decision with evidence. No
  dependency is currently admitted. A broad framework or safe C wrapper is not a shortcut.
- FFmpeg is each selected module's correctness/feature/performance reference. Minimum acceptance:
  full declared feature parity, initial-delivery runtime at most 1.25 of equivalent FFmpeg work, and lower measured
  memory. Preserve per-case failures and confidence evidence; never manufacture acceptance by
  reducing the work or excluding inconvenient required inputs.
  The 0.80 runtime ratio remains the optimization target; see
  research/decisions/initial-delivery-runtime.md for the owner's later authorization.
  The owner also approved total resident peak RSS for initial delivery; use the
  versioned policy in research/decisions/2026-11-03-total-resident-rss.md. Keep
  original operation-growth verdicts as diagnostics and never rewrite frozen H0
  collectors, raw data or claims to apply the newer policy.
- Begin implementation with the shared H0 harness and corpus mapping, then bounded core and
  the first MKV packet slice. Do not claim all of FFmpeg or even full module parity from a slice.
- Use immutable source pins and input hashes. FATE identities are not tests executed. Sample
  source/redistribution rights need review before vendoring; keep large media outside git.
- Performance runs must be isolated, paired and target-specific. Do not time competing builds,
  fuzzing or other benchmarks. Mac results do not establish 32-bit TV performance.
- Distinguish authored code, upstream observations, a working prototype, full parity and a
  release-qualified result. The current spike collects mismatches; its exit 0 is not a parity pass.
- Set CARGO_INCREMENTAL=0 on direct cargo commands. Do not modify Streem, deploy to a TV,
  publish a repository, commit or push unless the current task authorizes it.
