# Contributing

## Setup

You need Rust (see `rust-version` in `Cargo.toml`; `build-workers` and the full check
also install the dated nightly toolchain pinned in xtask, which builds the AAC codec
worker and runs the `std::simd` checks), Python 3, a C compiler, a C++ compiler and
`make` (for the FFmpeg, mpg123, FreeType, HarfBuzz, FriBidi and libass builds),
`pkg-config` and zlib headers (NASM on x86), and `zstd` (for zstd-compressed evidence
sets and their test). The references are only needed for differential and performance
work.

```sh
cargo xtask fixtures              # generate the authored test corpus into .research/
cargo test --workspace            # library tests
cargo xtask prepare-check         # once: the pinned references, fixtures, vectors,
                                  # font corpus and workers
cargo xtask check                 # everything CI runs
```

`cargo xtask help` lists every command. CI runs `check` in four parts and a 32-bit ARM
test job, and a pull request that changes only documentation runs `cargo xtask check
docs` instead; [quality](docs/quality.md#harness) describes each part. Set
`CARGO_INCREMENTAL=0` for direct cargo commands; the reference builds and benchmarks
assume it.

## Ground rules

- **Safe Rust only.** Every library crate forbids `unsafe_code` in its `lib.rs`. There is
  no exception for performance. The workspace lint table, which every package inherits,
  denies it rather than forbidding it, so that each worker binary's C entry point is its
  one allowed exception ([quality](docs/quality.md#measuring-performance)).
- **No dependencies by default.** A third-party crate is admitted only if its whole
  dependency graph is safe Rust, it fits the exact contract (timing, bounds, errors,
  incremental input), it passes the same gates as our own code, and reusing it is
  clearly cheaper than writing and owning the bounded component. Open an issue with
  that comparison before adding one. Tooling follows the same rule.
- **Specifications first.** Implement from the standard. Every reference
  implementation (FFmpeg, FreeType, HarfBuzz, libass and the rest) is consulted as a
  black box: its inputs and outputs, never its source. Nothing is translated from
  another implementation; [docs/provenance.md](docs/provenance.md) lists the modules
  that are translations, and a change that rewrites one from its specification removes
  it from that list. The reference implementations Unicode publishes with its standards
  count as specification. Tables the specification declares normative but does not
  print may be recovered from reference output, provided the recovery is scripted and
  reproducible (see `crates/codec-dca`). Behaviour, rules as well as tables, may
  likewise be recovered by scripted, reproducible black-box probes of the reference
  binary where no specification text is available; the code documents it as observed
  behaviour and names the probe that establishes it.
- **Lints are policy.** Clippy (`all` and `pedantic`) and rustdoc run with
  `-D warnings`. The workspace `[lints]` table states the pedantic lints the code does
  not follow and why; each library's `lib.rs` also enables `missing_docs`,
  `missing_debug_implementations`, `unreachable_pub`, `clippy::print_stdout`,
  `clippy::print_stderr`, `clippy::unwrap_used` and `clippy::expect_used`. Fix the
  cause; an `#[allow]` needs a one-line reason.
- **Bounded by construction.** Input-dependent allocation goes through a `Budget`;
  loops over input run under a `WorkBudget`. Malformed input must never panic or hang.
- **Rust first in tooling.** Prefer Rust for tools and tests; the repository tooling is
  `cargo xtask`. Python is used where it is clearly the better fit: the independent
  oracles (`harness/*_oracle.py`), the numpy table-recovery and model scripts under
  `harness/codecs/`, the capture input generator `harness/capture/generate_inputs.py`
  (numpy's PCG64 generator and float draws; the cited capture inputs must not change)
  and fixture authoring scripts. C is used only for workers that link a reference.

## Changing behaviour

1. Describe the feature and the reference operation it must match.
2. Add or extend its [feature contract](docs/quality.md#feature-contracts) and corpus.
3. Implement it. Keep worker traces byte-identical for everything you did not mean to
   change.
4. Compare it: `cargo xtask report --contract harness/features/<feature>/contract.json`
   (or the family's command: `fonts|shape|raster compare --all`, `ass compare`,
   `oracle-compare`, `codecs`, `capture`), and `cargo xtask fuzz --contract
   harness/features/<feature>/contract.json --output DIR`. When the change touches a
   parsing or decoding hot path, run the comparison with `--benchmark --memory
   --require-acceptance` on an isolated Linux host of each gate target (AArch64 and
   32-bit ARMv7), or gate the items it reaches
   ([running the gates](docs/quality.md#running-the-gates)). When a reference or a
   fixture changes, re-record its pins with `cargo xtask
   <fonts|shape|raster|ass> manifest --record`; pins are never edited by hand.
5. Register every difference that remains as
   [when a result differs](docs/quality.md#when-a-result-differs) describes, never by
   weakening the comparison.
6. Put the per-case results in the pull request. Index the evidence set that holds
   cited results ([evidence](docs/quality.md#evidence)) and regenerate the status page
   with `cargo xtask status --write`.

See [docs/quality.md](docs/quality.md) for the gates and the measurement rules, and
[docs/architecture.md](docs/architecture.md) for where code belongs.

## Pull requests

- One topic per pull request, on a short descriptive branch (`matroska/lace-bounds`,
  `docs/quality`).
- The description says what changed, why, and how it was verified, with numbers for
  performance work.
- Docs change with the code they describe. A crate README describes the crate as it
  is.
- Commit messages are plain: an imperative subject line and a body that explains why.

## Licence

Contributions are dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
Media fixtures and specification texts keep their own terms; record the source,
licence and hash of any new input, and keep inputs without redistribution rights out
of the repository. The translated modules of `triergon-font`, `triergon-shape`,
`triergon-ass` and `triergon-raster` keep their projects' notices in each crate's
`LICENSE` ([provenance](docs/provenance.md)).

`xtask` (repository tooling, never linked into a library crate) contains a deflate
encoder derived from zlib, under the zlib licence, and an MT19937 generator seeded as
CPython's `random` module does, under the Mersenne Twister BSD licence and the PSF
licence. See [xtask/THIRD-PARTY.md](xtask/THIRD-PARTY.md).

[`harness/rss-bound/`](harness/rss-bound/README.md) is a Linux kernel module licensed
GPL-2.0-only, as the GPL-only kernel symbols it uses require. It is measurement tooling,
never linked into a library crate or shipped.
