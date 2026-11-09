# triergon-harness-runner

The command line and measured loop the Rust harness workers (`playback-worker`,
`subtitle-worker`) share. A worker mode is `{prefix}fold FILE ARGS`,
`{prefix}bench FILE ITERATIONS ARGS`, `{prefix}memory FILE ARGS` and, where the worker
traces, `{prefix}trace FILE ARGS`. [`Invocation`] parses it, and [`measure`] runs the
operation as often as it asks and prints the result line the reference worker
(`harness/reference.c`) prints for the same mode. Input preparation, process start-up
and the result line stay outside the timed loop and the allocation window; an
operation that leaves storage charged to the preloaded input's pool fails the run.
It is tooling, not part of the library.

## Usage

```rust,no_run
use triergon_harness_runner::{Invocation, Totals, measure};

let args: Vec<String> = std::env::args().skip(1).collect();
let invocation = Invocation::parse(&args, "demo-", " ARG", 1..=1, false)?;
let input = invocation.input(1 << 20)?;
measure(&invocation, "demo", None, || {
    Ok(Totals {
        packets: 1,
        bytes: input.len() as u64,
        ..Totals::default()
    })
})?;
# Ok::<(), Box<dyn std::error::Error>>(())
```
