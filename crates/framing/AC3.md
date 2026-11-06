# AC-3 / E-AC-3 source-facts slice

This bounded independent O21 audio batch passed all 13 Rust regressions in the
parent targeted run and make10, alongside 13 NAL and 13 AV1/VP9 tests. The
240-prefix pinned native two-fact comparison remains NONPASS; the separate
[E-AC-3 BSI/evidence batch](EAC3-BSI.md) preserves all of its inputs and failures.
This lane ran no compiler, native reference, benchmark or fuzz job.
The sole runtime dependency remains owned `triergon-core`, with unsafe forbidden.

## Selected syntax and source boundary

Frame syntax is pinned to publicly acquired ATSC A/52:2018: base §§5.3–5.4,
Table 5.18, and Annex E §§2.2–2.3. Configuration record payload syntax comes
from ETSI TS 102 366 V1.4.1 (2017-09), Annex F.4/F.6. Exact URLs/SHA-256 hashes
are in `.research/parallel-framing/audio-batch/spec-sources-v1.json`.

The distinction between editions is intentional. ATSC explicitly specifies
E-AC-3 `fscod2` rates 24/22.05/16 kHz with six inferred blocks. The inspected
ETSI edition does not define that frame branch. The frame implementation follows
ATSC there; it does not invent an ETSI field. AC-3 bsid 0–8 and E-AC-3 bsid 11–16
use their specified common prefix. Bsid 9/10 and values above 16 are unsupported;
no reduced AC-3 rate or future layout is guessed.

`Ac3FrameReader` requires an **aligned, big-endian** elementary syncframe stream.
It checks `0B 77` at each exact declared frame boundary. It neither searches past
junk nor swaps `77 0B` words. `Ac3StreamSelection` explicitly allows either family
or requires one family. The common bsid position distinguishes the two layouts.
This is frame/header probing, not movie/audio decoding or stream discovery.

`probe_ac3_header` reads at most twelve bytes and ninety selected bits, with no
allocation or source-sized loop. It returns needed-prefix truncation, invalid
syntax, or unsupported layout distinctly. It computes frame bytes, sample rate,
coded blocks/samples, acmod/LFE and the exact selected-prefix bit boundary. AC-3
facts include raw CRC1, size/rate/service codes, optional mix/surround values,
dialnorm and optional first compression word. E-AC-3 facts include stream type,
substream ID, word-length code, physically present rate-or-block code, dialnorm,
compression, conditional dual-mono fields and dependent channel-map presence/data.

Reduced-rate `fscod2` and physically coded `numblkscod` remain different optional
facts. Six inferred blocks do not manufacture a present `numblkscod` field.
Absent compression/map and explicitly present zero values remain distinct. Local
reserved-value issues are exposed without defaulting or changing source values.
The selected prefix must fit before the mandatory final CRC word; this is only
a minimum structural check. Full BSI, blocks and CRC validity remain unverified.

`sample_span()` is an exact core `Time` for coded samples/rate. It is **not**
presentation time. A dependent frame shares its independent stream's programme
interval; summing those frame spans would invent time. Acmod channel counts are
literal mode-table counts, not an aggregated programme/channel-map topology.
AC-3 nominal bitrate is separate from the exact frame-bits/rate/sample ratio,
which can differ per 44.1-kHz frame. E-AC-3 has no invented nominal-rate value.

## Input, allocation and work

The reader owns a core `InputCursor`; all supplied windows must have the correct
pool/source/generation/token/offset. Arbitrary chunk boundaries through sync,
prefix, body and CRC are supported. `Step::{NeedInput,Yield,Ready,End}` remain
distinct; short/empty windows are starvation and explicit `finish` establishes
source completion. Transport failure, cancellation and truncation are sticky,
never synthetic EOF. A new reader represents a new entity/reset; cross-entity
partial-frame continuation is not implemented.

Each output is a complete pool-owned immutable byte-identical frame, including
all source CRC/body bytes, with source/generation/offset/header facts. No frame
is emitted before its declared envelope is present. CRC2 is exposed as a raw
last word, explicitly unchecked. Holding a clone retains its full backing charge
after reader drop and across thread release.

The fixed prefix is at most twelve stack bytes. Declared frame size and source
end are checked before an exact-size pool builder is reserved. Prefix/body copies
are charged by byte, with copies at most 4096 bytes before another cancel check.
Length inspection, bounded prefix inspection, reserve and freeze each cost one
fixed unit. One-unit budgets progress; zero budgets do not consume bytes. Freeze
and reserve pressure retain the exact checkpoint without payload reparse. Policy
bounds cover complete frame size, frame count and input window size. Allocation,
deallocation and fixed metadata copying remain foundation costs, not a hard
wall-clock latency guarantee.

## dac3 and dec3 records

`Ac3ConfigParser` receives a complete bounded same-pool box **payload**, with an
explicit `Ac3ConfigKind`; it does not guess size/type headers or auto-detect
record kinds. Dac3 is exactly three bytes, exposing fscod, bsid, bsmod, acmod,
LFE, bitrate code and all five reserved bits. Reserved fields and unsupported
codes are preserved with issues. Unsupported bsids produce no guessed rate hint.

Dec3 exposes the original data-rate field and the syntax's one-to-eight
independent descriptors, held inline in a fixed array. Each raw three/four-byte
descriptor preserves fscod, bsid, reserved bits, asvc, bsmod, acmod, LFE and raw
`num_dep_sub`. `chan_loc = Some(0)` is distinct from its absence; the alternate
one reserved bit remains observable when the field is absent. Original record,
descriptor spans and any trailing reserved bytes stay available as shared input
storage. Additional reserved tail bytes are permitted by F.6.2.14 and are not
discarded or interpreted as a guessed extension. The record has no fscod2 field;
fscod 3 is not turned into an inferred reduced rate.

F.6.2.12 describes `num_dep_sub` using a last dependent substream ID; this parser
keeps the raw four-bit field and its specified presence branch. It does not
derive or validate actual dependency counts, topology, stream associations,
programme duration, aggregate bitrate or agreement with samples. Source issue
flags are local checks, not decoder-ready/configuration conformance certificates.
Later dec3 reserved-tail/immersive extensions remain opaque.

Record parsers read fixed prefix/descriptor bytes with work accounting and
check cancellation between bounded stages. They allocate no output storage:
source acquisition backpressure belongs to the caller's core byte allocation;
returned records/views alias those bytes. No partial configuration is published.

## Authored evidence and open gates

The self-contained Python generator uses original bit-authored fields, a separate
whole-buffer bit-string oracle and `Fraction` sample/bitrate arithmetic. The
AC-3 oracle derives word counts arithmetically rather than importing the Rust
44.1-kHz table. Synthetic bodies and CRC words are opaque, deliberately including
false sync patterns and invalid/unchecked audio; successful fact extraction is
not encoded-audio validity.

| Family | Cases | Source-fact extractions | Negative vectors | Wire bytes |
|---|---:|---:|---:|---:|
| Header probes | 240 | 221 | 19 | 2823 |
| Complete frame sources | 18 | 13 | 5 | 10318 |
| dac3 payloads | 101 | 97 | 4 | 298 |
| dec3 payloads | 31 | 26 | 5 | 292 |

Total: **390 cases / 13731 authored bytes**, with 33 negative repros retained.
The full 3×38 AC-3 frame-size/rate matrix, all AC-3 modes, E-AC-3 block/rate/type
branches, reduced rates, optional zero values, custom maps, maximum sizes,
reserved values, all descriptor counts and reserved tails are represented.

The thirteen Rust regressions passed in the parent targeted run and make10. They compare the corpus,
split frame input across prefix/body/CRC boundaries with work 1/2/17/4096, and
exercise source identity, pool/generation rejection, starvation/EOF/failure,
allocation/freeze retries, cancellation, limits, selection, lifetime and coded
time/presence facts. Record tests use tiny windows/work. All polling is bounded.
Only small original Python preparation ran; the parent owns all Rust/native and
qualification runs. The pinned public-API comparison is separate from installed FFmpeg and remains
NONPASS; it compares only bsid/frame size. No numerical claim exists.

Open gaps include resynchronization search, word-swapped/IEC transport, complete
BSI/metadata/CRC/audio-block validation, decode, stream/dependent grouping, record
consistency, immersive extensions, target/integration proof and independent
reference parity. DTS core is **not selected**: its normative core syntax has
not been inspected/frozen into a coherent implementation batch. No claim is made
that its standard is unavailable. AVC/HEVC G01, SPS and AV1 semantic gaps remain
explicit and unchanged.

Full frozen selected parity, delivery runtime upper paired95 <=1.20 (optimization target <=0.80), lower live heap and
lower attributable RSS remain hard per-case **unqualified/unmeasured** native
Linux ARM64 gates. 32-bit webOS/device evidence is separate. The crate remains
experimental; this batch closes neither O21 nor the broader M02 audio family.

Primary sources: [ATSC A/52:2018](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf)
and [ETSI TS 102 366 V1.4.1](https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/01.04.01_60/ts_102366v010401p.pdf).
Existing Streem/GPL code was requirements inventory only and was not copied,
translated or used as binary-layout authority. No third-party dependency,
root/core/shared-harness edit, device/account action, commit or publish is part of
this batch.
