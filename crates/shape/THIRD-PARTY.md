# Third-party notices

## Unicode

The generated tables in `src/unicode` (`tables.rs`, `syllabic_tables.rs` and
`bidi/tables.rs`) hold data derived from the Unicode Character Database, from the
hash-pinned files that `cargo xtask shape ucd` names in each table's header:
`tables.rs` and `syllabic_tables.rs` from Unicode 17.0.0 and 18.0.0, under the Unicode
License V3 below, and `bidi/tables.rs` from Unicode 14.0.0, under the licence agreement
in force at that release, quoted after it.

The bidirectional algorithm in `src/bidi` follows Unicode Standard Annex #9, revision
44 (Unicode 14.0.0), and was written with the Unicode Consortium's C reference
implementation of that version as its model, as Unicode publishes it: "as a general
model for other implementations, to assist in correct interpretation" of UAX #9. No
code was copied from it. The release read, Version 14.0.0 of September 23, 2021, from
<https://www.unicode.org/Public/PROGRAMS/BidiReferenceC/14.0.0/>:

| File | SHA-256 |
| --- | --- |
| `ReadMe.txt` | `4d3289dc7b3ae6673dd145774ae2ac49c87640ac9c8daad1de9ce1f28e880911` |
| `include/bidiref.h` | `d89f32c200553a08f950e2bd5543f63e2d2c7fdfa188e0c2b5b0649d323c2529` |
| `source/bidiref.c` | `794fd81cb5f16bc91f63e3c41df36ef6d189b69e694fe89f0796e9237ce16d77` |
| `source/bidiref1.c` | `c73740849c862c64f69c616063b56c21af5d75afc43b69dea714d57e120220dc` |
| `source/bidirefp.h` | `a0649dbad6438d01f5a3da7429adfcaf0ba269a20d246a1a3196ab4c31a030b4` |
| `source/brinput.c` | `7bda7638095b3fdd61f317023e30e47940631b24a4dde075c237156cc5cc9d98` |
| `source/brrule.c` | `3dbc3c0ada964c9004387903490351fd572b1f4beed951fa4aa84f656d0a8bb7` |
| `source/brtable.c` | `acf81da7720932da535b0ace286f05378300690a76755d828a2315db95203635` |
| `source/brtest.c` | `c99a465acdcac71534bf09bf777cdf700cf210bc7ff32c3f217412f8465f1640` |
| `source/brutils.c` | `9c1c57a6c648e76c0e7bcea7065ac1252c462aa6905e8d1dc553b735da05bc6a` |

The release carries no licence text of its own. Its source files state "Copyright 2021,
Unicode, Inc. All rights reserved." (`brinput.c` 2016, `brtest.c` 2018, `brrule.c` 2020),
and its `ReadMe.txt` says the
source code "is distributed, under the general Unicode Character Database terms of use
(see https://www.unicode.org/copyright.html)".

The Unicode 17.0.0 and 18.0.0 data files are under the Unicode License V3. The Unicode
14.0.0 data files, and the reference implementation through the terms of use its
`ReadMe.txt` names, are under the licence agreement that
<https://www.unicode.org/license.txt> published when 14.0.0 was released (as archived on
8 November 2021). Both texts are quoted in [LICENSE](LICENSE).

## HarfBuzz and libass

Most modules outside `src/bidi` are translations of HarfBuzz 14.6.0, or tables generated
from its source files, and `src/metrics.rs` and `src/rotations.rs` follow libass 0.17.5.
[LICENSE](LICENSE) holds both projects' notices, and
[docs/provenance.md](../../docs/provenance.md) lists the modules.
