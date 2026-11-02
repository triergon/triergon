# Text extraction draft v1 — private capability-red batch

This is the original source-carriage contract for a private candidate. The
pre-implementation target is archived in implementation/before; the current
implementation and pending parent validation are identified by
implementation/checkpoint-1.json. It is not a complete M05 implementation, source-conformance checker,
native-equivalent operation, or qualification baseline. The published 65-test
DVB checkpoint remains held separately. No Rust or native command was run by
this lane for this batch.

## Selected work and source boundaries

`docs/scope.md` M05 explicitly selects SRT/WebVTT/mov_text extraction and
preservation of authored ASS/text packets. `research/streem-inventory.json`
also selects charset integration, but does not enumerate every required
encoding. No private Streem implementation was read to fill that gap.

The byte-pinned primary register is `SOURCE-LOCK.json`. Its dated WebVTT
Candidate Recommendation Draft and Matroska Internet-Draft are identified as
drafts. The Matroska explanatory page is a mutable primary snapshot whose
cached content hash is fixed; it is not an immutable publication URL.

| Input operation | First extraction contract | Primary basis and limit |
| --- | --- | --- |
| SubRip document | Numeric identifier, `hh:mm:ss,mmm --> hh:mm:ss,mmm`, multiline text, blank separators, exact times and original order | Matroska codec-21 §5.2 describes the four parts and mapping. The detailed strict grammar here is a declared ingestion profile, not a universal SRT standard. Hours have at least two digits. |
| WebVTT document | File marker/BOM, exact cue times, identifiers, raw settings, payload, and Header/NOTE/STYLE/REGION records in source order | W3C CRD 2026-05-20 §§2.2, 4.1, 6.1/6.3. This is source extraction with strict malformed-input errors, not the complete browser parser, CSS engine or conformance checker. |
| `tx3g` / mov_text sample | Big-endian u16 text-byte count, UTF-8 or FEFF-prefixed UTF-16BE, whole original modifier boxes in original order | ETSI TS 126 245 V18.0.0 §§5.1–5.2, 5.16–5.17. The 2048-byte author recommendation is not substituted for the u16 syntax limit. |
| Demuxed UTF-8 packet | Complete original packet, strict decoded UTF-8 view, caller's exact timing and configuration facts | RFC 3629; Matroska codec-21 §§3.5.11, 5.2. No SRT framing is invented inside an already demuxed packet. |
| Matroska WebVTT packet | Raw cue text plus separately retained CodecPrivate and BlockAdditional; ID 1 settings/identifier/comment field ranges | Matroska codec-21 §§3.5.12, 5.4. CodecPrivate and unknown additions remain opaque. Inline cue timestamps are already block-relative and remain unchanged. This is not WebM's distinct carriage. |
| Opaque ASS/SSA packet | Exact packet/configuration/declared charset bytes, identity, arrival order and source timing | Matroska codec-21 §5.3 for UTF-8 Matroska carriage. Opaque acceptance itself asserts neither valid ASS syntax nor truthful encoding nor Matroska conformance. |

SRT/UTF-8/UTF-16 document line framing recognizes CRLF, LF and CR. This is an
explicit SubRip profile choice; the three WebVTT line terminators are specified
by its primary source. Raw cue ranges exclude the final structural terminator
after the last payload line; the entire original input remains retained, so no
source bytes disappear. Internal payload CR/LF codepoints are preserved.

The `decoded_utf8` field performs only strict character decoding. It does not
normalize Unicode, line endings, whitespace, entities, markup, inline clocks or
ASS escapes. It is deliberately not named `VTTCue.text`: the W3C parser's LF
normalization/NUL replacement/recovery is a separate view and remains open.
WebVTT zero/negative intervals are strict authoring-profile errors here, not a
claim that every user agent aborts rather than recovers. Header/prefix records
already published before a later framing error remain observable. Whole-input
charset validation precedes document publication; malformed UTF input has no
published prefix in this batch.

## Charset and configuration facts

`Encoding::Utf8` validates RFC 3629 scalars and rejects overlong encodings,
surrogates and values above U+10FFFF. A document BOM is framing only and remains
in the original bytes. A BOM inside a demuxed UTF-8 packet is text content.

`Utf16Bom` explicitly requires a BOM and selects BE/LE from its original bytes.
This is not a general implementation of RFC 2781's unlabelled UTF-16 default.
Explicit `Utf16Be`/`Utf16Le` do not consume a leading FEFF as an endian selector;
it is a content codepoint. Odd tails, unmatched surrogates and invalid pairs
are errors. The original source byte ranges remain separate from UTF-8 byte
ranges; no codepoint/glyph/UTF-16-unit equivalence is inferred.

`Tx3gInBand` uses the 3GPP FEFF byte order when present. Byte-reversed FFFE is
not silently admitted as a required 3GPP baseline. Empty tx3g text and empty ASS
packets are source events, with no invented renderer-clear or EOF action.
All tx3g boxes are retained as source bytes; this batch does not apply styles,
font fallback, character offsets, box geometry, karaoke or modifier precedence.
Inherited ISO box size 1/0 forms remain explicit unsupported gaps in this first
contract, pending exact inherited box-source integration. Legacy QuickTime
`text` is distinct from `tx3g` and is not claimed implemented.

Raw `codec_id`, `codec_private`, charset-label bytes and configuration epoch
are retained unchanged. Absence differs from a present empty owner. The
`codec_private` slot carries the upstream opaque configuration owner; it does
not assert that a full ISO sample entry equals AVCodec extradata. That concrete
container boundary still needs parent integration evidence. No configuration
is synthesized, stripped, translated or interpreted as ASS styles here.

Unknown encodings can be carried in opaque ASS mode without a renderability
claim. Decoding a document with an unknown declared charset returns
`Unsupported("declared charset conversion")`; there is no guessing or lossy
replacement. Exhaustive selected legacy charset conversion remains required.

## Concrete API and shared-core requirements

`src/model.rs` defines `Input`, `Timing`, `RawClock`, `Limits`, `Cue`,
`Metadata`, `Field`, `Modifier` and their enums. These types stay local to the
subtitle lane. `Input` is one complete charged immutable document or demuxed
sample, with optional original owners for configuration, labels and additions.
The parent still owns transport reads, source/range tokens, assembly and
container demuxing. An empty input is not an EOF signal.

`Extractor::provide` either accepts all owners or returns `ProvideError` with
the exact input. It checks pool membership for every owner, per-field caps,
generation, busy state and explicit end state. `step(&mut WorkBudget,
&AtomicBool)` yields `NeedInput`, `Yield`, `Event`, `NeedOutput` or `Finished`.
`finish_input` ends the sequence after pending work; `reset(generation)` drops
pending private state while already returned output owners remain valid.
Cancellation is checked before work and at future bounded inner-loop units;
it returns `Cancelled` with resumable pending source state in this contract.

Concrete core names required by this contract (the baseline uses a subset):
`SourceId`, `SourceLocation`, `Budget::{new,clone,
reserve,used,available}`, `Bytes::{copy_from_slice,slice,belongs_to,
shares_storage_with,retained_storage_bytes}`, `BudgetVec::{new,try_push,
try_reserve_exact,try_extend_from_slice,freeze}`, `Time::{new,from_ticks,
checked_add,checked_sub}`, `TimeBase::new` and `WorkBudget::{take,try_charge,
remaining,spent}`. No core addition is required for this operation. A future
InputCursor adapter must respect its existing generation/range-request rules;
this batch does not add transport semantics to the core.

`Timing::from_ticks` retains raw numerator/denominator and PTS/DTS/duration
ticks while computing exact core Time values. Source clock ratios are not
rewritten merely because TimeBase reduces them. Unknown PTS/duration remain
`None`; known zero duration and negative PTS remain distinct values. End is
computed only when start and duration both exist, with exact checked rational
arithmetic. No next-packet duration inference or microsecond conversion occurs.

## Resource and work contract

Default prototype policy: input 4 MiB; opaque configuration/addition each 1 MiB;
each codec/charset label 256 bytes; a source line 64 KiB; 4096 cues per document;
4096 modifiers per sample; decoded output 4 MiB per cue. Limits are explicit
caller policy, not format syntax or frozen acceptance exclusions. The corpus
contains the u16 maximum 65535-byte tx3g text sample.

All output source views alias charged Bytes. UTF-8 packet output aliases its
validated source owner. UTF-16 output uses a charged BudgetVec and recoverable
freeze to immutable Bytes. Private line/index/modifier storage must also be
charged. Capacity growth accounts for simultaneous old/new allocations.
Output-pressure recovery preserves exact cursor/Unicode/run state and emits
no duplicate prefix. Returned owners keep both original and decoded capacities
charged until the final alias drops. Pool-control charge is measured as the
baseline before input allocation, not mistaken for a leak.

Work is charged before each bounded byte/scalar/framing/modifier operation.
No repeated complete-prefix scan is allowed on resume. Work=0 yields without
mutation; work=1/7/4096 produces the same source event sequence. These are
deterministic functional work units, not comparable native consumed-byte or
runtime measurements. Queue/output retention remains bounded by the caller's
shared pool and explicit event policy.

## Corpus and parent red request

`author.py` constructs 72 original source fixtures from authored components;
`oracle.py` separately parses them using Python's strict codecs, integer/struct
operations, regex framing and Fraction clocks. It imports neither author.py
nor Rust nor native media code. The authored expectations and independent
oracle agree in `oracle-check.json`: 72/72. This is source-oracle consistency,
not independent native parity or proof the Rust implementation works.

`regressions.rs` declares 72 separately named fixture tests plus 12 ownership/
work controls and 12 resource/lifetime/resume tests: 96 total. Fixtures run at
work 1/7/4096; individual resource tests state their schedules. `resource-cases.json`
identifies reused source spans and extra control inputs. Golden values remain
source facts, with no native-output rewriting.

The archived red baseline implemented only ownership/work setup and opaque ASS
carriage. Other extraction dispatch returned an explicit `Unsupported("text
extraction not implemented")`. Current candidate code implements the specified
paths; actual candidate Rust validation remains pending the parent queue. This is an honest new-capability runtime-red
target, not a regression found in existing production code. Actual parent baseline: 23 pass and 73 fail, preserved with its source and
raw log in implementation/red-proof.json. Parent instructions and exact commands are in `parent-request.json`.
Keep the frozen red source and raw failures before any subsequent parser fix.

## Reference operation and still-required work

The first reference operation must compare complete source extraction from the
same authored SRT/WebVTT or complete demuxed tx3g/UTF8/ASS samples, including
configuration, charset, timing, source progress/errors, order and owned output.
Native AVCodec conversion to an ASS event and a source-text extraction are
different operations until all conversion/preservation work and output facts
are explicitly accounted for. An extraction-only worker cannot be compared to
libass rendering. Record default options, public API units, discovery mode,
complete input tails, raw packet/config/event bytes and native error progress.
No native option is disabled here and no exploratory installation is treated
as the pinned qualification reference. Check pinned decoder availability in
the parent queue before requesting any new native adapter execution.

Still required: complete selected legacy encodings and profile inventory,
WebVTT UA recovery/semantic views if selected, complete tx3g modifier semantics
and inherited box forms, concrete ISO/config boundary, matched native output
ownership and functional parity, and all per-case runtime paired95 <=0.80 plus
lower live heap/RSS gates on native Mac ARM64. Those gates are unchanged.
PGS and VobSub/DVD SPU remain selected and required: Blu-ray/DVD normative
bitstream sources were not acquired, and their decoder grammar is not invented
from a mapping document or third-party code. DVB's original 48/13 traces,
source-legality/tolerance corrections, raw RGB/alpha/order/tail gaps and unknown
mandatory cached-ODS replay requirement remain preserved and unresolved.
