# Explicit multibyte text profiles, version 1

This additive API is independently authored safe Rust against licensed mapping
DATA. It adds no runtime dependency. No FFmpeg or framework decoder code was
copied or translated. It does not assert universal charset-label equivalence or
normative conformance to an unavailable national standard. The original
`Charset::from_label` resolver and all of its Unsupported outcomes stay unchanged.
Callers explicitly opt into `MultibyteCharset::from_label`, `Charset::Multibyte`
or `Encoding::Multibyte`; there is no language, filename or content detector.

| Profile | Admitted labels (ASCII case insensitive) | Exact mapping DATA |
| --- | --- | --- |
| GbkCp936 | gbk, cp936, windows-936 | Unicode/Microsoft CP936 table 2.01, 2000-01-07; includes 80 -> U+20AC. Not GB18030. |
| Big5Unicode | big5, big-5 | Historical Unicode BIG5 table 2.0, mapping revision 1994; seven explicit NO MAPPING records are strict errors. |
| ShiftJisUnicode | shift_jis, shift-jis, sjis | Historical Unicode SHIFTJIS table 2.0, mapping revision 1994; 5C is yen, 7E is overline. |
| EucKr | euc-kr, euc_kr | Unicode/Microsoft CP949 table 2.01 restricted to pairs whose two bytes are A1..FE, plus ASCII. Includes later euro/registered-sign additions; refuses UHC extension pairs outside that geometry. |
| Windows932 | cp932, windows-932 | Unicode/Microsoft CP932 table 2.01, 1998-04-15. |
| Windows949 | cp949, windows-949 | Unicode/Microsoft CP949 table 2.01, 2000-01-07; UHC extension pairs are included. |
| Windows950 | cp950, windows-950 | Unicode/Microsoft CP950 table 2.01, 2000-01-07; distinct punctuation and ETEN additions. |

All aliases in this table are declarations of this API's exact selected profile,
not assertions about every system iconv implementation. Other aliases, leading
or trailing spaces, embedded NUL, HKSCS, GB18030 and ISO-2022 modes are refused.
The Windows profiles do not invent private-use mappings missing from their pinned
tables. The historical Big5/Shift-JIS profiles are deliberately named; modern web encodings may
map the same labels to broader Windows-compatible repertoires. Complete selected
consumer compatibility and other legacy coverage remain open until evaluated.

The original mapping text, including copyright, limitations and historical
ambiguity warnings, is retained in `tests/multibyte-mapping-data`. The packed
production DATA is a sorted sequence of big-endian `(u16 wire, u16 scalar)`
records. No mapping has a surrogate or scalar above U+FFFF. Unicode License V3
is distributed with both data locations; KSC5601 also retains its original
file-specific permission notice as independent evidence; its 1995 core mapping agrees with the selected EUC subset except the two later CP949 euro/registered-sign additions. The private source lock records every URL,
raw-file hash and generated-data hash. This is data reuse, not admission of a
production package. C0/DEL identity mappings are explicitly added to Shift-JIS;
ASCII is explicitly added to Big5 and the EUC subset. The Big5 table's seven
U+FFFD records explicitly mean NO MAPPING and are all retained as required
negative controls, never emitted as replacement characters.

`MultibyteDecoder` retains at most one lead byte, consumes one byte per charged
work unit and emits the exact original physical byte range for each scalar.
Each unit includes at most sixteen binary-search comparisons over immutable
mapping DATA. Lead/trail syntax is explicit and a structurally legal but unmapped
pair is an error. Invalid trailing bytes are never replayed as ASCII. Caller
chunk partitions do not change interpretation. Empty availability is not EOF;
only `finish` asserts final source EOF. A partial lead at finish is sticky
Truncated. Other malformed and overflow errors are sticky; cancellation is
resumable. Zero work consumes no byte. The decoder allocates nothing and does
not own or infer source IDs, labels, transport status, epochs or clocks.

`CharsetJob` uses the same scalar cursor as explicit SubRip extraction. It
validates/counts the entire source before allocation/publication, then encodes
into the exact charged output length. Work yields retain real lead/trail and
pending output state. Source and even contradictory declared-label owners stay
unchanged. Full backing capacities remain charged through caller retention.
SubRip preserves raw physical ranges, caller generation/sequence/configuration
epoch and exact cue times. Default formats and UTF8-only formats stay unchanged.
There is no replacement, Unicode/line normalization, source rewriting or clock
inference. Character conversion does not change ASS, tx3g rendering or modifiers.

Whole-mapping fixtures independently parse original text tables, compare all
profile entries at work quanta 1/7/4096 and require outcomes for all 256 one-byte
and 65,536 two-byte streams per profile. Other controls cover every physical
chunk partition of authored mixed-width streams, EOF, invalid/unmapped bytes,
overflow, cancellation, source-owner retention, foreign pools, decoded caps,
allocation pressure and SubRip source facts. These are functional controls;
no speed, heap/RSS, module, target-device or native-parity claim is made.

Native reference captures, the original177 memberships and24 D2 guards remain
required and unchanged. Existing FFmpeg build/iconv capabilities must be checked
on a separately assigned Linux lease before any new native comparison claim.
Any new reference interface or build configuration needs its own pinned review;
this source batch activates neither. Original SDKs, workers, flags and golds are
immutable. Remaining tx3g modifier semantics are a separate required feature.
