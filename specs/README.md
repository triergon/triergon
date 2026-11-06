# Specifications

Triergon is implemented from published standards. [`sources.json`](sources.json)
pins the texts the Matroska reader is checked against, with their origin and
SHA-256: EBML (RFC 8794), Matroska (RFC 9559, element schema, codec mappings and
notes) and zlib/DEFLATE (RFC 1950, RFC 1951).

```sh
cargo xtask fetch-specs       # download into the ignored .research/specs/ and verify hashes
```

A hash mismatch means the upstream text changed and needs review; it is never updated
automatically. Each crate README lists the standards that crate implements, with
section references.
