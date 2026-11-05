# Captured Linux receiver charset view 1

This additive opt-in `native_charset::AssPacketJob` accepts one complete opaque
ASS packet. It validates the actual Input owners, generation, limits and timing
through Extractor. It retains the complete source Cue and full converted UTF-8
owner separately from a C-string prefix and the suffix after eight literal
commas. This is payload extraction, not ASS rendering or field validation. An
empty physical packet, missing separators and an empty text suffix are distinct
facts. None predicts decoder emissions or changes an unknown clock.

Both original label resolvers and all seven original strict mapping profiles
retain their behavior. There is no detection, trimming, substitution or implicit
profile upgrade. The opt-in resolver uses the twelve captured labels in the
D32/Linux observations. `euc_kr` and `windows-932/949/950` remain unavailable in
this receiver view, even though the original strict resolver can resolve them.
Availability is specific to the captured runtime, not universal alias identity.
Absent declarations and other charset labels are unsupported in this bounded
receiver operation; the original source resolvers remain available independently.

Three explicitly named mapping profiles are added:

* ReceiverBig5Glibc239: all 14,040 records in the freely distributable GNU Big5
  charmap version0.92 in the glibc2.39 snapshot, including irreversible decode
  mappings and C6/C7 records. It differs from the historical Unicode Big5 table.
* ReceiverEucKrC1: the original 8,354-record EUC-KR repertoire plus32 single-byte
  C1 controls. Evidence comparison against GNU EUC-KR finds one remaining extra
  A2E8/U+327E mapping, which this bounded profile explicitly refuses. It is not
  whole-glibc EUC-KR. CP949 extensions remain separate.
* ReceiverWindows932Pua: all7,915 original CP932 records plus Microsoft's1,880
  F040..F9FC user-defined records mapping to U+E000..U+E757. The underlying
  Shift-JIS yen/overline mappings remain distinct. Other unassigned codes are
  refused, with no best-fit reverse encoding or replacement.

Mapping DATA comes from primary mapping tables, not translated implementations.
GNU Big5's full free-distribution notice accompanies the data. Microsoft's
WindowsBestFit data uses the existing Unicode licence, retained unchanged.
EUC-KR's GNU file is evidence-only; the production C1 extension is the identity
mapping of explicitly named Unicode controls and retains the existing licensed
CP949-restricted mapping data. No external runtime dependency is introduced.
The source pins and generation records accompany the private handoff.

Every conversion validates the entire physical source before publishing the
prefix, including malformed bytes after NUL. Each mapping decode consumes one
bounded work unit; conversion/counting/encoding and prefix scanning resume on
work exhaustion. Cancellation is resumable; retained output pressure can be
released and retried. Every dynamic buffer uses the shared Budget, including
retained capacity and source owners. Refusals retain the complete source Cue.

Outcome values are Rust conversion facts, never reconstructed FFmpeg codes.
The old native wrapper's returned-byte count may describe re-encoded bytes;
31 original operations stopped when that exceeded the offered source byte count.
Those exact process failures, four context-open refusals,16 conversion refusals,
and all177+74 ordered native captures remain unchanged. Matching47 payloads
and preserving seven empty packet observations does not close native process
parity or full M05/charset/runtime/memory qualification. A different complete-
packet native operation would require a separately reviewed interface.
