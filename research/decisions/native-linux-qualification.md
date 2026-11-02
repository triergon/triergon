# Native Linux qualification target

On 2026-10-31 the owner authorized native Linux qualification on isolated AWS
instances, replacing native Mac ARM64 for new performance runs. The active
development contracts target `aarch64-unknown-linux-gnu`. Existing immutable Mac
receipts remain evidence only for their recorded source and platform.

The frozen feature sets, input bytes and comparison rules are unchanged. Every
required case must retain full selected parity, a paired 95% runtime-ratio upper
bound at most 0.80, and strictly lower live heap and attributable RSS. Missing or
unqualified memory instrumentation is a non-pass. A new platform does not erase a
previous clean failure or qualify a complete module.

The current Linux runner is native AArch64 with Neoverse-V2 CPUs. Candidate
code generation may use the actual CPU features, recorded in its build identity;
the pinned reference remains unchanged. Each machine runs one build, fuzz or
benchmark workload at a time. Timing runs exclude the experimental RSS kernel
observer. Memory uses separate processes and separately qualified instrumentation.

Linux proof does not establish 32-bit webOS performance or complete Streem
integration. Those remain required shipping gates. Cloud credentials, host
addresses and infrastructure identifiers stay outside published source.

The owner subsequently authorized the [initial-delivery runtime policy](initial-delivery-runtime.md):
1.25 is now the delivery ceiling;0.80 remains the optimization target. Historical
Mac receipts and the original Linux measurements retain their recorded policies.
