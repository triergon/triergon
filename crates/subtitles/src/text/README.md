# Named text source-ingestion contracts, version 1

`triergon_subtitles::text` provides bounded source extraction and opaque
carriage. The original private implementation passed all96 unchanged regressions
and all-target Clippy with `-D warnings` at
`.research/subtitles-text-host-v2-1793455830972317000/`. Promotion changes module
namespace/import paths and documentation. The parent subsequently passed the
public161-test combination, all-target Clippy with `-D warnings` and the release
library build at `.research/subtitles-text-promoted-host1-1793458809112021000/`,
with source before/after unchanged. The private and public receipts are distinct
source identities; no documentation-build result was supplied in the latter.
No full native compatibility, source-conformance certificate or M05 numerical
acceptance is claimed.

| Contract | `Format` | Explicit source-ingestion boundary |
| --- | --- | --- |
| `SubRipStrict1` | `SubRip` | Numeric sequence identifier, two-or-more-digit hours, exact `hh:mm:ss,mmm --> hh:mm:ss,mmm`, multiline text, CRLF/LF/CR framing. This declared profile is not a universal SRT standard. |
| `WebVttSourceStrict1` | `WebVtt` | UTF-8 file/BOM marker, exact cue times and original identifier/settings/payload/NOTE/STYLE/REGION order. Strict malformed-input/interval errors; no claim of the complete browser recovery, CSS, DOM or semantic text view. |
| `Tx3gCarriage1` | `Tx3g` | u16 big-endian text-byte length, UTF-8 or FEFF-prefixed UTF-16BE, original modifier boxes/ranges/order. The 2048-byte recommendation is not treated as the syntax maximum. Modifier rendering/precedence and inherited size0/size1 box forms remain required gaps. |
| `Utf8Packet1` | `Utf8Packet` | Strict UTF-8 packet validation with an immutable alias of the original payload and exact caller timing; no invented document framing. |
| `MatroskaWebVttCarriage1` | `MatroskaWebVtt` | Raw stored cue text and opaque CodecPrivate; ID1 BlockAdditional settings/identifier/comments, unknown additions retained unchanged. Inline timestamps remain block-relative as stored. This is not WebM carriage. |
| `OpaqueAss1` | `OpaqueAss` | Exact ASS/SSA packet, configuration, declared charset label, source identity and raw clock carriage. No ASS syntax validation, charset-truth assertion, Dialogue reconstruction, reordering or renderer-clear inference. |

The byte-pinned primary register is
`tests/text-fixtures/SOURCE-LOCK.json`, relative to the subtitles crate. It
preserves version-qualified ETSI TS126245 V18.0.0, RFC3629/2781/9559, the dated
W3C WebVTT Candidate Recommendation Draft and Matroska codec-21 Internet-Draft,
plus the explicitly mutable primary explanatory snapshot. Drafts are not
represented as final standards. URLs, original cache paths and SHA-256 identities
are preserved; normative PDF/HTML cache bytes remain in the ignored research
lane rather than being redistributed as production data.

## Source facts and progress

`Input` contains one complete immutable document or demuxed sample, same-pool
owners for payload/configuration/codec/charset/additions, configuration epoch,
core SourceId/SourceLocation and caller generation/sequence. `provide` returns
the exact Input on rejection. `step` uses core WorkBudget and an AtomicBool;
NeedInput, Yield, Event, NeedOutput, Finished and typed errors remain distinct.
`finish_input` marks sequence completion; an empty packet is an event, not EOF.
Reset drops private state and changes generation; old output owners remain valid.

Unicode bytes, surrogate state, line boundaries and timestamp tokens retain real
forward cursors across yields and cancellation. Whole-input Unicode validation
precedes document publication. A separate forward framing pass preserves raw
byte ranges; timestamp tokens are parsed once per timing line. Already published
source records survive a later framing error. Cancellation leaves pending source
state resumable. Fatal errors free private construction and persist until reset.

`decoded_utf8` is strict character decoding only. It performs no Unicode,
line-ending, whitespace, entity, markup, ASS or clock normalization. Internal
CR/LF codepoints stay exact. UTF-8 document BOM framing differs from a BOM in a
demuxed UTF-8 packet, which remains text content. Explicit UTF16BE/LE differs
from the BOM-required profile; tx3g does not silently adopt byte-reversed UTF16.
Unknown declared document encodings are explicit Unsupported outcomes; opaque
ASS may preserve such bytes without asserting renderability.

`Timing::from_ticks` retains original clock numerator/denominator and optional
PTS/DTS/duration ticks while computing exact core rational Time values. Unknown
time, known zero duration and negative PTS remain distinct. End is computed only
from known start and duration. No next-packet inference or microsecond rounding
occurs. Raw configuration remains the upstream opaque owner; its preservation
does not prove that a complete ISO sample entry equals native AVCodec extradata.

## Ownership and policy

UTF-8 output aliases its validated original owner. UTF-16 output reserves the
exact byte count gathered during parsing, writes incrementally into BudgetVec,
and freezes without copying. Output pressure retains pending scalar/builder/
freeze state. Input/configuration/addition aliases and decoded output keep full
backing capacities charged until their final owners drop. There is no uncharged
Box on publication or rejection.

Five approved declaration-scoped Clippy expectations preserve exclusive inline
states and recoverable inline rejection. Compile-time physical ceilings guard
Item272, Event672, Action688, Status672 and ProvideError432 bytes, plus bounded
Result envelopes. The layout-specific Item/Event expectations apply on64bit;
all size guards compile on every target. These are per-value layout ceilings,
not aggregate stack, heap or RSS measurements or a numerical-gate waiver.

Default caller policy: input4MiB; config/addition1MiB each; labels256 bytes;
source line64KiB; cues4096/document; modifiers4096/sample; decoded output4MiB/cue.
Limits are explicit policy, not syntax redefinitions or accepted exclusions of
required sources. One-unit work, output pause, cancellation and retained lifetime
are covered by the unchanged original tests. No core API or dependency was added.

## Corpus and remaining scope

`tests/text.rs` is the same96-test body with three import/path substitutions.
`tests/text-fixtures/fixtures/` preserves the complete original72-case source,
expected bytes, generated Rust cases and identity manifest. The enclosing bundle
also preserves author.py, an independently written source oracle, primary pins,
resource cases and the original private contract snapshot. Private originals
remain intact. No native output supplied these golden expectations.

Complete selected native text compatibility/recovery, legacy charset coverage,
tx3g semantic/configuration integration, matched native output ownership and
all per-case paired95 delivery runtime<=1.25 (optimization target <=0.80) plus lower live heap/RSS gates remain required.
PGS, VobSub/DVD SPU and remaining DVB work are still required. Unavailable Blu-ray/
DVD normative bitstream sources are not replaced by inferred decoder grammar.
The original DVB source/native palette, order, tail and source-legality/tolerance
differences and unknown mandatory cached-ODS replay requirement remain unchanged.
