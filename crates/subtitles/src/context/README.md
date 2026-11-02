# Owned subtitle context

This handoff accepts already parsed track/configuration/language fields, ordered
metadata and complete attachment byte owners. Existing consumer ASS, charset and
font rendering policy remains unchanged. It performs no container/font parsing.

Offers are recoverable on rejection. Record transfers and publication yield on
WorkBudget; cancellation, charged record storage and output pressure are explicit.
All original Bytes backing capacity stays charged until its final retained owner
drops. Logical visible_bytes is not a heap/RSS measurement. Packet binding checks
source, generation, selected track number and configuration epoch without touching
the packet time or payload. Both raw language sources and absence remain distinct.

The parent ran runtime RED1/33 and candidate34 debug/release/all-target Clippy plus
existing323 tests successfully on the frozen private source. The source pins
and original fixtures remain in the private context-batch-1 artifacts. Full native
context integration, valid-font consumer proof and numerical qualification remain
open. Opaque probe bytes are not font-rendering/conformance evidence.
