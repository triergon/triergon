# Original text source corpus and oracle bundle

Copied from `.research/parallel-subtitles/text-batch-1/` after its exact private
96-test and Clippy pass. The private originals remain intact.

- `fixtures/`: all original72 format cases, source/config/addition bytes, exact
  UTF-8/time/field expectations, generated Rust cases/names and original manifest.
- `author.py`, `oracle.py`: original component author and independent Python
  source parser. Their paths work unchanged with this enclosing bundle layout.
- `SOURCE-LOCK.json`: original primary URLs, version/draft roles, immutable byte
  hashes and ignored-cache locations. No external decoder code or normative PDF
  is embedded in the runtime crate.
- `resource-cases.json`: the original additional12 resource/recovery cases.
- `ORIGINAL-CONTRACT.md`: verbatim historical private contract snapshot. Its
  package/status/queue paths describe that stage; the current public contract is
  `src/text/README.md` relative to the subtitles crate.

The public test changes only its package import and two include/module paths.
Every copied fixture, expected value, primary pin, generator, oracle and resource
file is byte-exact. The unchanged manifest's `fixtures/` paths resolve from this
bundle root. Source oracle agreement is distinct from Rust/native equality.
The parent passed the public161-test combination, all-target Clippy with
`-D warnings` and the release library build with unchanged source at
`.research/subtitles-text-promoted-host1-1793458809112021000/`. Documentation-build
evidence was not supplied in that receipt. No complete subtitle/native/numerical
qualification is claimed.
