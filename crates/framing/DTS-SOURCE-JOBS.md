# DTS source inspection and low14-bit payload jobs

The source slice adds a source-header inspection job and an explicit
BE/LE low14-bit word payload adapter. The original strict canonical DTS probe
and frame reader are unchanged. `DtsSourceIdentity` carries caller-supplied
context for complete owned records; it does not validate transport freshness.

Inspection distinguishes physical missing/partial fields, syntax-absent HCRC and
coded zero, retaining all source bytes and independent validation issues. Ready
means an inspection is available, not that a DTS frame or coded audio is valid.

The low14-bit adapter follows ETSI TS102114 V1.2.1 §5.3:16-bit words carry14
payload bits; upper two source bits are retained as unused facts. Full output
bytes and remaining payload bits are separate, so no missing final byte is
zero-filled. Word order is explicit; physical source bit mapping is preserved.
FSIZE conversion, complete14-bit frame acquisition, auto sync detection,
termination presentation, HD assets and decoder/audio behavior remain open.

Source/output owners share the core pool. Work, cancellation, retriable reserve
and freeze pressure, permanent size limits and retained capacities are explicit.
No third-party/native dependency, unsafe code, C or decoder is added.

All 103 framing tests pass on native Linux, including the original 76,
14 inspection tests and 13 packing tests. The isolated integration also passed
release tests and 16 Python field-mapping controls; the public checkpoint passed
Clippy with warnings denied, the no-default library check and release example
builds. The public receipt is `linux-framing-gate-1793484533696285403` under
`.research/aws-linux-extended1/exports/3.122.247.247/`. Earlier proposals and
failures remain under `.research/parallel-framing/dts-inspect-integration-v1/`.
These are source/host checks. Complete native parity,
delivery <=1.25, optimization <=0.80 and strict lower heap/RSS remain unqualified.
