# Contributing

## Setup

You need Rust (see `rust-version` in `Cargo.toml`; the full check also installs the
dated nightly toolchain pinned in xtask for the optional SIMD lane), Python 3, a C compiler,
a C++ compiler and `make` (for the FFmpeg, FreeType, HarfBuzz, FriBidi and libass builds),
`pkg-config` and zlib headers (NASM on x86), and `zstd` (for zstd-compressed evidence sets
and their test). The references are only needed for differential and performance work.

```sh
cargo xtask fixtures              # generate the authored test corpus into .research/
cargo test --workspace            # library tests
cargo xtask prepare-check         # once: pinned references (FFmpeg, FreeType,
                                  # HarfBuzz, FriBidi, libass), fixtures, font
                                  # corpus, workers
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
- **Specifications first.** Implement from the standard. Every reference
  implementation (FFmpeg, FreeType, HarfBuzz, libass and the rest) is consulted as a
  black box: its inputs and outputs, never its source. Nothing new is translated from
  another implementation. The modules listed in
  [docs/provenance.md](docs/provenance.md) are earlier translations of FreeType,
  HarfBuzz and libass, kept for now under those projects' permissive licences and
  notices. They may be rewritten from specifications later, and a listed module that
  has a public specification may be rewritten from it whenever it is being changed
  anyway. The CJK auto-hinter in `triergon-font` is the only translation added after
  this rule, by explicit decision; it is listed with the others and may likewise be
  rewritten later. The reference implementations Unicode publishes with its standards
  count as specification. Tables the specification declares normative but does not
  print may be recovered from reference output, provided the recovery is scripted and
  reproducible (see `crates/codec-dca`). Behaviour, rules as well as tables, may
  likewise be recovered by scripted, reproducible black-box probes of the reference
  binary where no specification text is available; the code documents it as observed
  behaviour and names the probe that establishes it.
- **Lints are policy.** Clippy (`all` and `pedantic`) and rustdoc run with
  `-D warnings`. The workspace `[lints]` table states the pedantic lints the code does
  not follow and why; each library's `lib.rs` enables the documentation, visibility,
  console and `unwrap` lints it already passes and lists those still pending. Fix the
  cause; an `#[allow]` needs a one-line reason.
- **Bounded by construction.** Input-dependent allocation goes through a `Budget`;
  loops over input run under a `WorkBudget`. Malformed input must never panic or hang.
- **Rust first in tooling.** Prefer Rust for tools and tests; the repository tooling is
  `cargo xtask`. Python is used where it is clearly the better fit: the independent
  oracles (`harness/*_oracle.py`), the numpy table-recovery chains under
  `harness/codecs/`, the capture input generator `harness/capture/generate_inputs.py`
  (numpy's PCG64 generator and float draws; the cited capture inputs must not change)
  and fixture authoring scripts. C is used only for workers that link a reference
  (FFmpeg, FreeType, HarfBuzz, FriBidi and libass).

## Changing behaviour

1. Describe the feature and the reference operation it must match.
2. Add or extend the corpus and the comparison in the harness.
3. Implement it. Keep worker traces byte-identical for everything you did not mean to
   change.
4. Run `cargo xtask report --fate-cache .research/fate-samples`, and the same with
   `--benchmark --memory --require-acceptance` on an isolated AArch64 Linux machine when
   the change touches parsing or decoding hot paths; for `triergon-font`,
   `cargo xtask fonts compare --all` with the same options, for `triergon-shape`
   `cargo xtask shape compare --all`, for `triergon-raster`
   `cargo xtask raster compare --all`, for `triergon-ass` `cargo xtask ass compare`,
   and
   `cargo xtask fuzz --contract harness/features/<feature>/contract.json --output DIR`.
   When a reference or a fixture changes, re-record its pins with `cargo xtask fonts
   manifest --record`, `cargo xtask shape manifest --record`, `cargo xtask raster
   manifest --record` or `cargo xtask ass manifest --record`; pins are never
   edited by hand. Put the
   per-case results in the pull request and update [docs/status.md](docs/status.md).

Register comparisons in `harness/features/<id>/contract.json`. The contract
selects the candidate, reference and operation. Set its `gate` to `required`
to include the comparison in `cargo xtask check`. Worker identities bind the
registered contracts and their corpus inputs. Generated manifests record and
verify their pins against the reference named in the contract. A generated
feature can set `pin_note` to describe those pins. Required comparisons check
manifest profiles and expected folds before running. Set `compare_order` to
order a generator's reports. Unranked features follow in ID order.

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

`triergon-font`, `triergon-shape`, `triergon-ass` and `triergon-raster` contain modules
translated from FreeType, HarfBuzz and libass. Each of these crates ships the projects'
notices in its `LICENSE` file, and [docs/provenance.md](docs/provenance.md) lists the
modules, versions and licences. A change that rewrites a listed module from its
specification updates that list.

`xtask` (repository tooling, never linked into a library crate) contains a deflate
encoder derived from zlib, under the zlib licence, and an MT19937 generator seeded as
CPython's `random` module does, under the Mersenne Twister BSD licence and the PSF
licence. See [xtask/THIRD-PARTY.md](xtask/THIRD-PARTY.md).

[`harness/rss-bound/`](harness/rss-bound/README.md) is a Linux kernel module licensed
GPL-2.0-only, as the GPL-only kernel symbols it uses require. It is measurement tooling,
never linked into a library crate or shipped.
