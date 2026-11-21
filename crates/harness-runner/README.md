# triergon-harness-runner

Shared command-line and reporting code for the Rust harness workers. A worker
mode is `{prefix}fold FILE ARGS`, `{prefix}bench FILE ITERATIONS ARGS`,
`{prefix}memory FILE ARGS` or, where the worker prints records,
`{prefix}trace FILE ARGS`. [`Invocation`] parses the command line.

[`measure`] repeats the operation, checks that each completed run releases
its budget charges, and prints the work totals or the operation's refusal.
[`measure_with`] appends named values computed during input preparation.
Workers that own their measured loop use [`Timer`] and [`Boundary`] at the
edges of the work they measure, then print their work with [`write_summary`]
or [`BenchLine`]. [`command`] reports a worker's diagnostic and exit status.

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
