# Third-party notices

## Unicode

The generated tables in `src/unicode` (`tables.rs`, `syllabic_tables.rs` and
`bidi/tables.rs`) hold data derived from the Unicode Character Database, from the
hash-pinned files that `cargo xtask shape ucd` names in each table's header.

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

The data files and the reference implementation are under the licence below.

```
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2026 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```
