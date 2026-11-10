# triergon-core

Shared vocabulary for every Triergon format and codec crate: exact rational
time, capacity-accounted byte storage, a poll-based input cursor, resumable
steps, per-step work budgets, one error type and the format-neutral container
model. It is the bottom layer of the workspace and depends only on `std`. It
performs no I/O, owns no threads and implements no format: packet boundaries,
codec configuration and clock epochs belong to the format crates built on it.

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, Rounding, SourceCapabilities, SourceId, Step,
    Time, TimeBase, WorkBudget,
};

// The caller owns the transport; here it is a byte slice.
let source: &[u8] = &[0x1a, 0x45, 0xdf, 0xa3, 0x01, 0x02];

// One pool charges every live allocation, including caller-retained aliases.
let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: Some(source.len() as u64), random_access: true };
let mut cursor = InputCursor::new(&pool, SourceId(1), caps, 64 * 1024)?;
let cancel = AtomicBool::new(false);
let mut work = WorkBudget::new(1 << 20);

let mut header = [0u8; 4];
let mut filled = 0;
while filled < header.len() {
    match cursor.read_into(&mut header[filled..], &mut work, &cancel)? {
        Step::Ready(n) => filled += n,                    // n bytes copied
        // Supply 1..=req.maximum bytes at req.offset, or finish at end of source.
        Step::NeedInput(req) => match source.get(usize::try_from(req.offset)?..) {
            Some(rest) if !rest.is_empty() => {
                let bytes = Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
                cursor.provide_owned(InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            }
            _ => cursor.finish(req.source, req.token)?,
        },
        Step::Yield => work = WorkBudget::new(1 << 20),   // step budget spent; resume later
        Step::End(_) => break,                            // source ended first
    }
}
assert_eq!(header, [0x1a, 0x45, 0xdf, 0xa3]);

let base = TimeBase::new(1, 90_000)?;
let pts = Time::from_ticks(183_003, base)?;               // exact, reduced
assert_eq!(pts.to_nanoseconds(Rounding::NearestAway)?, 2_033_366_667);
assert_eq!(base.rescale(183_003, TimeBase::new(1, 1000)?, Rounding::Floor)?, 2033);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Exports

| Area | Items |
|---|---|
| Time | `Time` (reduced `i128`/`u128` seconds), `TimeBase` (reduced positive `u64` ratio), `Rounding` |
| Storage | `Budget`, `Reservation`, `Charged<T>`, `Bytes`, `BytesWriter`, `BudgetVec<T>`, `BudgetVecIntoIter`, `OwnedInputBytes`, `ByteStorageAddress`, `nonzero_prefix_len` |
| Input | `InputCursor`, `InputRequest`, `InputWindow`, `InputToken`, `SourceId`, `SourceCapabilities`, `SourceLocation`, `SourceFailure` |
| Steps | `Step<T, B>`, `Backpressure` |
| Work | `WorkBudget` |
| Container model | module `container`: `ContainerReader`, `ReaderStep`, `Event`, `PacketInfo`, `TrackInfo`, `TrackId`, `MediaKind`, `CodecId`, `RandomAccess`, `Trim`, seek and colour types |
| Errors | `Error`, `Result`, `FreezeError`, `PushError<T>`, `TransferError` |

## Behaviour and limits

**Time.** `Time` is an exact, always-reduced rational number of seconds. Unknown
time is `Option<Time>`; no value is a sentinel. `checked_add`/`checked_sub`
reduce before range checks, so a wide intermediate alone never rejects a valid
result; products that do not fit `u128` go through a fixed 256-bit scratch
calculation without heap allocation. Rounded views (`to_ticks`, `to_i64_ticks`,
`to_nanoseconds`, `TimeBase::rescale`) take an explicit `Rounding`:
`TowardZero`, `AwayFromZero`, `Floor`, `Ceil` or `NearestAway`. Results that do
not fit return `Error::Overflow`; a zero timebase or denominator is `Invalid`.
Clock projections use checked 64-bit multiplication and division when
the scaled magnitude fits, with the selected rounding policy.

**Budget and storage.** `Budget::new(limit)` creates one accounting pool; clones
share it. Its limit includes its own fixed control block
(`Budget::control_storage_bytes()`). Every owned allocation holds a linear
`Reservation` until it drops. `Charged<T>` keeps a value charged for as long as it
lives (`Charged::boxed` charges a boxed value's size). A `Reservation` held apart
from its storage is for output a caller owns, such as a decoder's output frame:
`resize_to` charges that storage before it grows. `Bytes` is an immutable shared span: slicing and
cloning share one backing allocation, and every alias keeps the whole backing
capacity charged (`retained_capacity`). `try_into_writer` turns a uniquely owned whole
allocation into a `BytesWriter`, keeping its bytes; a shared allocation or a slice is
refused and returned. `BytesWriter::try_write_at` writes in place across steps, the fill
seeing the earlier bytes or zero past them; growth charges old and new buffers together
before it reserves, and a refused charge or allocation changes nothing. `finish`
publishes the bytes. `BudgetVec<T>` is the charged mutable
vector and reads and writes as a slice (`try_filled` builds one filled with a value,
`try_clone` charges a copy); growth charges old and new buffers together, and a rejected push,
reserve or `freeze` returns the original value or builder unchanged.
`BudgetVecIntoIter<T>::as_slice()` borrows the remaining elements in traversal
order without advancing or allocating. The iterator keeps its capacity charged
until it drops, including after its last element is consumed.
`OwnedInputBytes` adopts a caller `Vec<u8>` as immutable input; produced
packets or views cannot be converted back into it. `used()` and `peak()` report
requested capacity, not RSS or allocator overhead.

Two capacity errors are distinct. `Error::Limit` means the request can never fit
the policy. `Error::Backpressure` means it fits once the caller releases retained
output; the operation can be retried without losing state. Readers that own output
storage report the same condition as `Step::Backpressure` (see below).

**Input cursor.** `InputCursor` holds at most one supplied window at a time
(at most `maximum_window` bytes, or 12 inline bytes via `provide_inline`
without a heap owner). Offsets are checked `u64`. A window is accepted only at
the requested offset and with the current `InputToken`, which binds pool,
parser generation and range request; anything else returns
`StaleGeneration`, `StaleRequest`, `ForeignPool` or `SourceMismatch` and leaves
the cursor unchanged. A short or empty response is starvation, never EOF: end of
source is the separate `finish` call, after which reads return
`Step::End(SourceLocation)`. A known-length source that ends early is a sticky
`Truncated`. `fail` records a sticky `Transport` or `Cancelled` failure.

| Transition | Generation | Effect |
|---|---|---|
| `request_range(offset)` | kept | internal metadata/index jump; outstanding requests become stale |
| `request_range_reusing_window(offset)` | kept | internal jump that retains the current shared window if it covers the offset |
| `reset(offset)` | new | logical seek or recovery; the parser resets its own state too |
| `continue_source(id, caps)` | kept | next entity of the same logical stream (playlist segment) |

Reads come in four forms: `read_into` (copy into a slice), `read_append` (copy
into a `BudgetVec<u8>`), `read_discard` (skip without copy) and `read_span`
(zero-copy `Bytes` alias of the window). `buffered()` peeks without consuming.
A parser that still needs bytes after a partial read can use `pending_request()`
to ask for the next window once the current window is empty and the source is
still active. It checks cancellation before returning a request.
A read that yields, starves or hits backpressure keeps its position, so the
caller retries the same call.

**Work and cancellation.** Every read charges one `WorkBudget` unit per byte.
Format crates also charge parsing, decompression, indexing and pixel work before
doing it; an exhausted budget returns `Step::Yield` (or the format's equivalent)
at a resumable point. Cancellation is an `&AtomicBool` checked at every read;
once observed it is sticky until `reset`.

**Steps.** `Step<T, B = Infallible>` is the outcome of one bounded unit of
resumable work: `Ready(T)`, `NeedInput(InputRequest)`, `Yield` (work budget
spent), `Backpressure(B)` or `End(SourceLocation)` (end of this source entity; a
playlist or transport parser decides whether it is also the logical end). `B` is
the backpressure payload. Operations that retain no output storage, such as cursor
reads and most framers, keep the uninhabited default, so callers match without a
backpressure arm. Readers that own output storage return `Step<T, Backpressure>`;
`Backpressure { requested, available }` says how much retained output must be
released before stepping again.

## Container model

`triergon_core::container` is the vocabulary every container reader shares.
Readers keep their own rich packet and track types and expose them through views,
so nothing is converted or copied per packet and format detail stays on the format
type.

| Item | Meaning |
|---|---|
| `ContainerReader` | The resumable reader interface: `provide`, `finish`, `fail`, `reset`, `step`, `request_seek`, `track_count`, `track`, `capabilities`. Associated types `Packet: PacketInfo`, `Track: TrackInfo + ?Sized`, `Other` |
| `ReaderStep<R>` | What `step` returns: `Step<Event<R::Packet, R::Other>, Backpressure>` |
| `Event<P, X>` | `Packet(P)`, `Seek(SeekReceipt)` or `Other(X)` (configuration, documents, structure) |
| `PacketInfo` | `track`, `payload`, `time_base`, `pts`, `dts`, `duration` (ticks in `time_base`), `random_access`, `trim`, and `pts_time` as exact `Time` |
| `TrackInfo` | `id`, `kind`, `codec_tag` (the container's own identifier, unnormalised), `codec` (the `CodecId` the reader identifies from it), `configuration` (codec configuration bytes), `language`, `default_track`, `forced`, `colour` |
| `TrackId` | Matroska TrackNumber, ISO BMFF track_ID or MPEG-TS elementary PID |
| `MediaKind`, `RandomAccess`, `Trim` | `Video`/`Audio`/`Subtitle`/`Data`/`Unknown`; `Sync`/`NotSync`/`Unknown`; samples to discard at start and end |
| `SeekRequest`, `SeekPolicy` | track, target `Time`, `PreviousRandomAccess` or `PlaybackPreroll` |
| `SeekReceipt`, `SeekMethod` | `requested`, `landing`, `discard_until`, `scanned_bytes`, `replays_from_start`; `Index`, `BoundedScan` or `Restart` |
| `ColourDescription` | ITU-T H.273 primaries, transfer, matrix and `ColourRange`, chroma siting, `MasteringDisplay` (SMPTE ST 2086), `ContentLight` (CTA-861.3) |
| `Sourced<T>`, `Origin` | a value with its provenance: `Container`, `Bitstream` or `SpecificationDefault` |

Every implementation follows the same rules: no fabricated values (`None` or
`RandomAccess::Unknown` unless the source or a documented rule supplies them), no
sentinels, and specification defaults only with `Origin::SpecificationDefault`.
`dts` is `None` where the container carries no decode time. H.273 codes stay
numeric, and a conflict between container and bitstream colour is reported as two
descriptions, never resolved. Readers are concrete types used through generics or
one closed enum; there is no registry and no dynamic dispatch on the packet path
(`Track` may be `dyn TrackInfo` for a closed enum). Implementations:
`triergon_matroska::typed::Reader`, `triergon_isobmff::Reader` and the facade's
`triergon::AnyReader`. The
semantic contract is described in
[docs/architecture.md](../../docs/architecture.md#container-contract).

## Conformance

The arithmetic follows exact rational semantics; there is no external
specification. Verification:

- Integration tests in `tests/` cover ownership and charge accounting across
  clone, teardown, reset and cross-thread release; stale-token, foreign-pool and
  source-mismatch rejection; inline and owned windows; and clock boundary widths.
- `python3 tools/test-core-time.py` (also part of `cargo xtask check`) runs an independent Python
  `fractions.Fraction`/integer oracle against `examples/time-oracle.rs` in debug
  and release profiles over a versioned corpus of exact-time and wide-arithmetic
  cases.
- Two `compile_fail` doctests on `OwnedInputBytes` prove produced storage
  cannot be turned back into caller input.
- Container-model unit tests check exact packet time without fabricated values and
  that the neutral value types stay small on 32-bit targets; the format crates test
  their implementations.

The crate forbids `unsafe` code and has no dependencies. Project-wide gates and
the FFmpeg comparison harness are described in [docs/quality.md](../../docs/quality.md).
