# Contributing

## Setup

You need Rust (see `rust-version` in `Cargo.toml`; the full check uses nightly for
formatting and the optional SIMD lane), Python 3, a C compiler, `make`, `pkg-config`
and zlib headers (NASM on x86). The FFmpeg reference is only needed for differential
and performance work.

```sh
cargo xtask fixtures              # generate the authored test corpus into .research/
cargo test --workspace            # library tests
make prepare-check                # once: pinned FFmpeg reference, fixtures, workers
cargo xtask check                 # everything CI runs
```

Set `CARGO_INCREMENTAL=0` for direct cargo commands; the reference builds and
benchmarks assume it.

## Ground rules

- **Safe Rust only.** Every crate has `unsafe_code = "forbid"`. There is no exception
  for performance.
- **No dependencies by default.** A third-party crate is admitted only if its whole
  dependency graph is safe Rust, it fits the exact contract (timing, bounds, errors,
  incremental input), it passes the same gates as our own code, and reusing it is
  clearly cheaper than writing and owning the bounded component. Open an issue with
  that comparison before adding one. Tooling follows the same rule.
- **Specifications first.** Implement from the standard. FFmpeg is consulted as a
  black box: its inputs and outputs, never its source. Do not translate FFmpeg or any
  other implementation. Tables the specification declares normative but does not
  print may be recovered from reference output, provided the recovery is scripted and
  reproducible (see `crates/codec-dca`).
- **Lints are policy.** Clippy runs with `-D warnings`. Fix the cause; an `#[allow]`
  needs a one-line reason.
- **Bounded by construction.** Input-dependent allocation goes through a `Budget`;
  loops over input run under a `WorkBudget`. Malformed input must never panic or hang.
- **Rust first in tooling.** Prefer Rust for tools and tests. Python is used where it
  is clearly the better fit, and C only for workers that link the FFmpeg reference.

## Changing behaviour

1. Describe the feature and the reference operation it must match.
2. Add or extend the corpus and the comparison in the harness.
3. Implement it. Keep worker traces byte-identical for everything you did not mean to
   change.
4. Run `make report`, and `make qualify` on an isolated AArch64 Linux machine when the
   change touches parsing or decoding hot paths. Put the per-case results in the pull
   request and update [docs/status.md](docs/status.md).

See [docs/quality.md](docs/quality.md) for the gates and the measurement rules, and
[docs/architecture.md](docs/architecture.md) for where code belongs.

## Pull requests

- One topic per pull request, on a short descriptive branch (`matroska/lace-bounds`,
  `docs/quality`).
- The description says what changed, why, and how it was verified, with numbers for
  performance work.
- Docs change with the code they describe. A crate README describes the crate as it
  is today.
- Commit messages are plain: an imperative subject line and a body that explains why.

## Licence

Contributions are dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
Media fixtures and specification texts keep their own terms; record the source,
licence and hash of any new input, and keep inputs without redistribution rights out
of the repository.

`xtask` (repository tooling, never linked into a library crate) contains a deflate
encoder derived from zlib, under the zlib licence, and an MT19937 generator seeded as
CPython's `random` module does, under the Mersenne Twister BSD licence and the PSF
licence. See [xtask/THIRD-PARTY.md](xtask/THIRD-PARTY.md).

[`harness/rss-bound/`](harness/rss-bound/README.md) is a Linux kernel module licensed
GPL-2.0-only, as the GPL-only kernel symbols it uses require. It is measurement tooling,
never linked into a library crate or shipped.
