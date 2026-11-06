# Notes for automated contributors

Read [CONTRIBUTING.md](CONTRIBUTING.md), [docs/architecture.md](docs/architecture.md)
and [docs/quality.md](docs/quality.md) first. In addition:

- Run cargo with `CARGO_INCREMENTAL=0`. Never time or benchmark while anything else
  is compiling, fuzzing or benchmarking on the same machine.
- Keep worker traces byte-identical unless the change intends otherwise; say which
  traces changed and why.
- Never weaken a comparison, drop a corpus case or relabel a failure to pass a gate.
- Keep large media, downloaded sources and raw measurement data out of git.
- Write for library users: describe what the code does today. No process narration,
  run code names, ledgers, or references to particular downstream products.
- Update the crate README and [docs/status.md](docs/status.md) in the same change when
  behaviour or results change.
