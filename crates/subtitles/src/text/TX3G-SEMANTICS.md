# Tx3gSourceSemantics1

This additive operation interprets the complete defined sample-modifier grammar
against an explicitly identified description. It does not render glyphs, perform
font substitution, reorder Unicode, fetch hyperlinks, emit ASS or replace a
Streem/libass renderer. Original source carriage remains a separate operation.

The primary basis is ETSI TS126245 V18.0.0, sections5.1/5.2,5.6-5.8,5.15-5.18.
The existing private PDF cache is pinned at SHA256
`76f1e5591aa984906ea90fb8ccef8e73f4cbbe592dda67d73c7fc8ea5bed9024`.
The source lock records its original public ETSI URL. The document is used for
grammar/semantic facts, not reproduced as library source or distributed with the
production payload. Algorithms and controls are independently authored safe Rust;
no framework implementation or new production dependency is incorporated.

## Operation and source binding

`SemanticJob::new` borrows an Input, explicit optional configuration layout,
optional nonzero track timescale, shared Budget, limits and expected generation.
It runs the existing explicit ISO-box tx3g Extractor internally, followed by the
existing configuration parser when a layout is supplied. Publicly constructible
Cue/Configuration values are never treated as validated scope capabilities.
Only the internally produced immutable source views enter semantic interpretation.
Constructor rejection leaves every caller owner intact. Foreign pools and stale
generations are rejected by the existing ingestion contract.

Output retains the entire original Cue, ordered modifier ranges, input bytes,
declared labels, configuration owner/epoch, source ID, generation, sequence and
exact timing. Its optional Configuration retains all original source fields and
ordered children. Derived style spans, effects, fonts, runs and point styles do
not overwrite those facts. Unknown modifier boxes are retained by the source Cue
and have no defined semantic effect. Default Extractor behavior and the original
UTF8-only ConfigJob constructor remain unchanged.

`ConfigJob::new_unicode_fonts` is an explicit additive string profile supporting
UTF8 or FEFF-prefixed UTF16BE font names. It reuses Probe/Cursor validation.
Semantic font names expose both raw source ranges and strictly decoded UTF8,
including empty strings, NUL and comma-separated alternatives. They do not trim,
normalize or select an available font. Duplicate IDs and references absent from
a supplied complete description are errors. Without a parsed description, font
IDs remain requested source values and their family binding is unknown.

## Grammar and character ranges

All defined sample kinds are interpreted: styl, hlit, hclr, krok, dlay, href,
tbox, blnk, twrp and disp. Description disparity is read separately. Scalar
fields and length/count formulas are checked against the exact physical body;
known-body suffixes, incomplete records and invalid UTF8 URL/alternate strings
are errors. Existing ISO size0/size1/UUID envelope rules are reused. The 2048-byte
text recommendation does not reduce the u16 syntax boundary.

Offsets in this named profile count Unicode scalars, including each nonprinting
and line-break scalar. A surrogate pair is one scalar; a UTF16 BOM is excluded.
There is no grapheme clustering, line-ending conversion or glyph-count inference.
The original wire u16 offsets stay visible. Effects have half-open character
ranges. The documented N+1 endpoint for highlight/karaoke is represented by an
explicit sentinel flag and a derived end-of-text boundary. It is not a general
out-of-bounds clamp; styles/blink/links do not adopt that exception.

Style records and repeated range effects must be ordered and non-overlapping
within their type, across physical boxes. Empty selected ranges retain their records
and directly select no character; continuous karaoke separately ignores the start
offset when deriving a prefix. A styl record overrides the description style only over
its selected range; omitted description attributes remain unknown. Every RGBA
byte, font ID/size, face bit and unknown face/display bit remains exact. The three
defined face bits have explicit accessors; unknown bits are not rendered as a
known style.

Globally scoped hclr/dlay/tbox/twrp/disp are unique in this strict profile. A
second krok is rejected even if empty. This includes a strict refusal for the
original duplicate-twrp fixture; its old source extraction still retains both
boxes. No last-box-wins repair is silently introduced.

## Resolution and precedence

Static runs cover every character exactly once, with real raw-source and decoded
UTF8 byte spans. They carry requested style/source origin, bound font index when
known, static highlight, hyperlink and blink membership. A forward interval
sweep and one forward scalar pass construct the spans without a per-character
heap map or a quadratic scan of every effect for every character.

Hyperlinks have terminal-selected presentation authority over the requested
underlying style; no arbitrary blue/underline/font substitution is invented.
The requested base style stays visible. Static highlighting with hclr carries
a background-colour intent without rewriting text colour. Without hclr this
profile exposes an inverse/highlight intent for the terminal. Dynamic highlight
exposes the requested foreground-colour intent; absence of a colour remains
terminal-dependent. Blinking remains an independent request, including when the
terminal may decline a combination. Soft-wrap and scrolling are intents, not
implemented layout or movement. Signed text-box coordinates are retained without
inventing a missing region or clipping them to an unrelated video rectangle.

Karaoke character/time records are ordered. Time intervals are half-open and
zero-duration records never become active and therefore do not create derived
cross-kind application conflicts. All records still undergo source range/time
ordering checks and remain in the output. Continuous-prefix conflict checking
uses the same positive-duration extent as point queries. Continuous karaoke applies the
description flag to the derived prefix extent while retaining each original
range. Dynamic highlight cannot overlap static highlight or hyperlinks. These
conflicts are validated by forward scans; known continuous-prefix conflicts are
included. If description flags are unavailable, definite conflicts still refuse
and possible prefix conflicts remain explicitly unchecked in ValidationContext.

PointQuery performs resumable logarithmic run/event searches and reports the
resolved PointStyle without mutation or allocation. Its optional relative tick
argument is explicitly in the sample's track units. Unknown query time is not
zero; unavailable continuous-extent context yields Truth::Unknown rather than a
fabricated boolean or a validated-scope token.

## Timing, defaults and ownership

Karaoke and scroll delay use track ticks, not assumed milliseconds. Exact Time
values are derived only from the supplied nonzero timescale; the API never
infers it from an unrelated input clock. Known sample-duration bounds are checked
only when that scale is available. Raw ticks, optional exact values and the
validation context remain distinct. There is no next-packet/end inference.

Sample tbox/disp override parsed description values. The original default and
sample facts both remain. A parsed description with no disparity permits the
specified derived zero; an unavailable/unparsed description does not. Scroll
delay has a separately marked specified absent default of zero. Disparity stays
in signed sixteenth-pixel units; the optional integer-pixel view truncates toward
zero, widening before negation. No missing text style/font/colour is guessed.

All variable storage uses the caller's BudgetVec/Bytes pool. Font lookup uses
sparse pages allocated from actual demand, bounded by the u16 ID space; there is
no maximum-size per-font reservation. Parsing, pending records, font conversions,
range scans, run construction and point searches preserve real cursors across
work1 and cancellation. Work0 performs no data progress/allocation. Output
pressure returns NeedOutput and preserves pending owned values; fatal errors are
sticky, free private construction and publish no partial semantic object.
Cancellation is resumable. Retained output owns source/configuration/font/decoded
buffers and their full backing charges until final release. No core fork or
unaccounted container is added.

## Evidence and limits

The authored guards cover the original raw fixtures, every defined modifier,
range boundaries/precedence, track units, missing context, font strings/IDs,
strict refusal, retained ownership, foreign pools, generations, work0/1,
cancellation, pressure, limits and full u16 text length. Actual Linux gate results
are bound to frozen source maps in the private handoff. Mac Cargo/native execution
is prohibited; authored guards alone are not a GREEN claim.

All old tests/golds and the original177+74 native capture memberships and refusal
classifications remain required and immutable. The separately tracked charset
compatibility differences are unchanged. A source-semantic operation and native
mov_text-to-ASS decoding are different interfaces until a concrete full comparison
contract is satisfied. No reference SDK/interface/options/baseline change is
activated here. Full native/module/consumer/renderer and target integration, and
the unchanged runtime1.25/lower-heap/RSS gates, remain open.

Unknown query time produces an unknown dynamic intent only where a positive-duration source event can affect the character. A known noncontinuous description uses charged binary range lookup; continuous or missing description uses the privately derived possible prefix. The continuous flag ignores the start offset, so an empty selected range can hold its prefix during a pause. Empty karaoke headers still obey a supplied, known duration bound.
