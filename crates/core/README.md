# triergon-core

Shared vocabulary for every Triergon format and codec crate: exact rational
time, capacity-accounted byte storage, a poll-based input cursor, per-step work
budgets and one error type. It is the bottom layer of the workspace and depends
only on `std`. It performs no I/O, owns no threads and knows nothing about any
container: packet boundaries, codec configuration and clock epochs belong to the
format crates built on it.

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, Rounding, SourceCapabilities, SourceId, Step,
    Time, TimeBase, WorkBudget,
};

// One pool charges every live allocation, including caller-retained aliases.
let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: None, random_access: false };
let mut cursor = InputCursor::new(&pool, SourceId(1), caps, 64 * 1024)?;
let cancel = AtomicBool::new(false);
let mut work = WorkBudget::new(1 << 20);

let mut header = [0u8; 4];
loop {
    match cursor.read_into(&mut header, &mut work, &cancel)? {
        Step::Ready(n) => break,                          // n bytes copied
        Step::NeedInput(req) => {                         // supply 1..=req.maximum bytes
            let bytes = Bytes::copy_from_slice(&pool, fetch(req.offset, req.maximum))?;
            cursor.provide_owned(InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            // or cursor.finish(req.source, req.token)? at end of source
        }
        Step::Yield => work = WorkBudget::new(1 << 20),   // step budget spent; resume later
        Step::End(_) => break,
    }
}

let base = TimeBase::new(1, 90_000)?;
let pts = Time::from_ticks(183_003, base)?;               // exact, reduced
let ns = pts.to_nanoseconds(Rounding::NearestAway)?;
let ms = base.rescale(183_003, TimeBase::new(1, 1000)?, Rounding::Floor)?;
```

## Exports

| Area | Items |
|---|---|
| Time | `Time` (reduced `i128`/`u128` seconds), `TimeBase` (reduced positive `u64` ratio), `Rounding` |
| Storage | `Budget`, `Reservation`, `Bytes`, `BytesWriter`, `BudgetVec<T>`, `BudgetVecIntoIter`, `OwnedInputBytes`, `ByteStorageAddress`, `nonzero_prefix_len` |
| Input | `InputCursor`, `InputRequest`, `InputWindow`, `InputToken`, `SourceId`, `SourceCapabilities`, `SourceLocation`, `SourceFailure`, `Step<T>` |
| Work | `WorkBudget` |
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

**Budget and storage.** `Budget::new(limit)` creates one accounting pool; clones
share it. Its limit includes its own fixed control block
(`Budget::control_storage_bytes()`). Every owned allocation holds a linear
`Reservation` until it drops. `Bytes` is an immutable shared span: slicing and
cloning share one backing allocation, and every alias keeps the whole backing
capacity charged (`retained_capacity`). `BudgetVec<T>` is the charged mutable
builder; growth charges old and new buffers together, and a rejected push,
reserve or `freeze` returns the original value or builder unchanged.
`OwnedInputBytes` adopts a caller `Vec<u8>` as immutable input; produced
packets or views cannot be converted back into it. `used()` and `peak()` report
requested capacity, not RSS or allocator overhead.

Two capacity errors are distinct. `Error::Limit` means the request can never fit
the policy. `Error::Backpressure` means it fits once the caller releases retained
output; the operation can be retried without losing state.

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
| `reset(offset)` | new | logical seek or recovery; the parser resets its own state too |
| `continue_source(id, caps)` | kept | next entity of the same logical stream (playlist segment) |

Reads come in four forms: `read_into` (copy into a slice), `read_append` (copy
into a `BudgetVec<u8>`), `read_discard` (skip without copy) and `read_span`
(zero-copy `Bytes` alias of the window). `buffered()` peeks without consuming.
A read that yields, starves or hits backpressure keeps its position, so the
caller retries the same call.

**Work and cancellation.** Every read charges one `WorkBudget` unit per byte.
Format crates also charge parsing, decompression, indexing and pixel work before
doing it; an exhausted budget returns `Step::Yield` (or the format's equivalent)
at a resumable point. Cancellation is an `&AtomicBool` checked at every read;
once observed it is sticky until `reset`.

## Conformance

The arithmetic follows exact rational semantics; there is no external
specification. Verification:

- Integration tests in `tests/` cover ownership and charge accounting across
  clone, teardown, reset and cross-thread release; stale-token, foreign-pool and
  source-mismatch rejection; inline and owned windows; and clock boundary widths.
- `make core-oracle` (also part of `make check`) runs an independent Python
  `fractions.Fraction`/integer oracle against `examples/time-oracle.rs` in debug
  and release profiles over a versioned corpus of exact-time and wide-arithmetic
  cases.
- Two `compile_fail` doctests on `OwnedInputBytes` prove produced storage
  cannot be turned back into caller input.

The crate forbids `unsafe` code and has no dependencies. Project-wide gates and
the FFmpeg comparison harness are described in [docs/quality.md](../../docs/quality.md).
