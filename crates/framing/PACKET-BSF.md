# Stateful AVC/HEVC packet receiver

`NalPacketSession` implements the independently observed d32 public
`h264_mp4toannexb` / `hevc_mp4toannexb` complete-packet operation.
`NalPacketConfigWriter` publishes its initialization configuration bytes. Both
operate on the existing parsed records and shared Budget/Bytes/BudgetVec and
WorkBudget. The selected production graph has only the owned core dependency.

| Feature | Implemented behavior |
| --- | --- |
| Packet framing | Checked complete length-prefixed packets; explicit passthrough; source physical spans and prefix bytes retained |
| AVC state | In-band and injected SPS/PPS availability; first/continuation IDR handling; non-IDR rearming; exact three/four-byte native prefixes |
| HEVC state | Bounded whole-packet IRAP lookahead; injection before the earliest parameter-set/IRAP when that packet contains IRAP; source ordering retained |
| Config changes | Exact-byte-verified parsed AVC NEW_EXTRADATA, explicit receiver epoch; HEVC side data retained/forwarded with initial native configuration kept |
| Errors/flush | Valid AVC side-data changes survive physical packet errors; partial in-band state does not; cancellation/resource refusal does not adopt the pending update; flush preserves current config and rearms state |
| Ownership/source | Full source packet/config bytes, per-segment injection origins and arbitrary caller metadata move intact; outputs survive session teardown |
| Resources/work | Checked source/output/NAL/segment limits; charged tables/buffers; shared raw owners deduplicated in live lower bounds; one-byte work and retryable pressure |

`NalPacket<M>` carries caller metadata without interpreting or narrowing it.
Canonical exact clocks and unknown values can remain in that object. The default
`NalPacketFields` only contains literal native-observer scalar values; it does
not certify source-clock knownness. Configuration source identity/generation and
the separately counted receiver epoch remain distinct. Constructors consume
their supplied objects; callers can retain a raw handle or pass borrowed metadata
when they need it after a constructor refusal. A started failed job retains its source for inspection until the caller moves
it into an explicit receiver rejection, or until the job is dropped. The rejection
retains the original source and caller metadata after session teardown.

The default receiver consumes nonempty AVC NEW_EXTRADATA; a present empty entry
is forwarded. HEVC forwards this side data without adopting it. These are native
projection decisions; every original side-data byte stays in the source object.
Other caller properties/side data can remain in opaque M. Integrating their native
serialization and arbitrary receiver scheduling is still an end-to-end task.

The normative basis covers record wire syntax and fixed NAL headers. Parameter
set bodies, complete access-unit validity, later AVC profiles, complete codec
conformance and future record editions remain outside these parsers' proof. The
receiver state/prefix policies are controlled interoperability observations,
independently implemented without reference body copying. The private source
ledger pins primary H.264/H.265 documents, source contracts, SDK, C observer,
fixtures, alternative forecasts and every raw outcome.

The unchanged ISO source parsers refuse width three even though the external
length-framing API can declare it and the pinned native receiver accepts it.
Those disagreements and every malformed/error case stay required in the
functional review. No comparator sorts packets, normalizes prefixes/timestamps,
rewrites golds or turns errors into parity passes. There is no complete O21,
full-codec, numerical, lower-heap/RSS or application-integration claim.
