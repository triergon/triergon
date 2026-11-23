# triergon-harness-runner

The command line, measured loop and result line of the Rust harness workers. A
worker mode is `{prefix}fold FILE ARGS`, `{prefix}bench FILE ITERATIONS ARGS`,
`{prefix}memory FILE ARGS` or, where the worker prints records,
`{prefix}trace FILE ARGS`.

- [`Invocation`] parses such a command line. [`measure`] runs the operation as
  often as it asks, checks that each completed run releases its budget charges,
  and prints one result line with the summed [`Totals`] or the operation's
  [`Refusal`]; [`measure_with`] appends named values computed while the input
  was prepared, and [`measure_repeated`] runs every planned repetition of an
  operation whose outcome is a refusal.
- [`serve`] is the whole worker for a table of `(prefix, Prepare)` entries: it
  reads the input, lets the entry prepare an [`Operation`] from it and runs that
  through the same loop, handing it the trace writer in trace mode. [`smoke`] is
  the pipeline check every table worker carries.
- [`records`] is the record vocabulary of the render workers: a [`records::Kind`]
  and its values, folded by `Totals` or printed by [`records::Dump`].
- Workers that time a span inside each operation keep their own loop and use
  [`Timer`], [`Boundary`] and [`write_summary`] or [`BenchLine`]. [`command`]
  reports a worker's diagnostic and exit status.

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

A table worker:

```rust,no_run
use std::process::ExitCode;
use triergon_harness_runner::{Prepare, serve, smoke};

const OPERATIONS: &[(&str, Prepare)] = &[("views-smoke-", smoke)];

fn worker_main() -> ExitCode {
    serve(std::env::args_os(), OPERATIONS)
}
# let _ = worker_main;
```
