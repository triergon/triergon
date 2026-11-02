# Versioned text carriage boundary

This additive API keeps complete original `text::Input` owners, source timing,
configuration/tag/charset/addition presence, identity/generation/sequence/epoch
and location. It publishes a separate versioned native carriage view. Existing
strict source profiles/oracles and unchanged Streem consumer policy stay intact.

`Session::provide(&mut Option<Input>, Operation)` checks finite bounds before
taking input. `poll(WorkBudget, AtomicBool)` yields during source/Unicode/framing
work, publishes one immutable Snapshot and applies output pressure until taken.
`recover_pending` returns source owners after cancellation/resource failure.
Backing bytes, packet/side-data/metadata storage and caller retention are charged.
The native sample path preserves the entire payload, including invalid/opaque
text and tx3g framing. It performs no subtitle decoder/ASS/charset conversion.

Document framing uses exact millisecond fields and preserves native negative or
zero intervals; original exact source Time/RawClock is separately unchanged.
UTF16 scalar decoding follows RFC2781/RFC3629 with physical line marks. Native
cursor/byte compatibility facts are versioned observations, not claims of Unicode
or SubRip normative behavior. No post-output normalizer is used to compare data.

`Snapshot::native_view()` identifies `NativeView::D32` and its source pin.
The actual40 controls establish a UTF16 SubRip line-entry transition: return
the first converted byte and continue at the next scalar. UTF8/plain-BOM input,
mid-line scalars and direct packets remain intact. This is an opaque native byte
view, implemented while constructing packets. The complete Unicode conversion
and original source storage are never edited. The native implementation's
internal cause is not asserted and no byte value or fixture identifier is special.

All111 carriage regressions,40 native reader controls and five retained-owner
controls remain required, alongside Unicode/view unit tests and old161 tests.
Fixture provenance retains all177 original native observations; the 73 explicit
decoder/charset controls are diagnostics and never replace bypass-carriage work.
No complete M05, broad native compatibility, equivalent numerical operation or
memory/speed qualification follows from this slice. PGS/DVD normative blockers,
legacy charset integration, full consumer metadata/attachment inputs and DVB
native colour/order/tail gaps remain required and unqualified.
