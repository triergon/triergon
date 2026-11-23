# Multibyte charset profiles

Mapping data, sources and licences for `text::charset::multibyte`. Each profile
is selected explicitly with `MultibyteCharset::from_label`, `Charset::Multibyte`
or `Encoding::Multibyte`; no label or content is guessed.

A strict profile follows the current definition of its charset: the decoder of the
WHATWG Encoding Standard where that is the profile, a Windows code page as Windows
converts it, a national standard in its current edition. A `Receiver` profile follows
the C library of the reference build instead, and is what
`compat::charset::receiver_charset` selects.

## Strict profiles (`MultibyteCharset::from_label`)

| Profile | Labels (ASCII case-insensitive) | Mapping data |
| --- | --- | --- |
| `GbkCp936` | gbk, cp936, windows-936 | Microsoft CP936 table 2.01, including 0x80 → U+20AC. Not GB18030. |
| `Big5` | big5, big-5 | Big5 as the WHATWG Encoding Standard decodes it (index-big5): lead bytes 81–FE, the Hong Kong supplement with 1,713 plane 2 characters, and the codes 8862, 8864, 88A3 and 88A5 as a letter and a combining mark each. No single byte beyond ASCII. |
| `ShiftJisUnicode` | shift_jis, shift-jis, sjis | Unicode historical SHIFTJIS table 2.0 (0x5C yen, 0x7E overline), plus C0/DEL identity, with 815F → U+FF3C (JIS X 0208 1-32, as JIS0208.TXT, Windows and the WHATWG decoder map it) where SHIFTJIS.TXT gives U+005C. |
| `EucKr` | euc-kr, euc_kr | KS X 1001:2002: CP949 table 2.01 restricted to A1–FE byte pairs, plus A2E8 → U+327E, which KS X 1001:2002 adds and CP949 2.01 predates, and ASCII. UHC extension pairs are refused. |
| `Windows932` | cp932, windows-932 | Microsoft CP932 table 2.01 plus the 1,880 user-defined codes F040–F9FC → U+E000–U+E757, as Windows and the WHATWG Shift_JIS decoder convert them. |
| `Windows949` | cp949, windows-949 | Microsoft CP949 table 2.01, including UHC extensions. |
| `Windows950` | cp950, windows-950 | Microsoft CP950 table 2.01 plus the 6,217 user-defined codes FA40–FEFE, 8E40–A0FE, 8140–8DFE and C6A1–C8FE → U+E000–U+F848 in that order, 0x80 → U+0080 and 0xFF → U+F8F8, as Windows converts them. |

Other aliases, padded labels, GB18030 and ISO-2022 are refused. `ShiftJisUnicode`,
`EucKr` and `GbkCp936` are narrower than the Encoding Standard's decoders of their
labels, which decode Windows-31J, UHC and GB18030.

## FFmpeg/glibc profiles (`compat::charset::receiver_charset`)

These match the label set and repertoire the pinned FFmpeg build accepts through
glibc iconv when converting ASS packets (`compat::charset::AssPacketJob`).

| Profile | Labels | Mapping data |
| --- | --- | --- |
| `GbkCp936`, `ShiftJisUnicode`, `Windows932`, `Windows949` | gbk, cp936, windows-936, shift_jis, shift-jis, sjis, cp932, cp949 | As above. |
| `ReceiverBig5` | big5, big-5, cp950 | GNU Big5 charmap 0.92 from glibc 2.39 (14,040 records), which its `iconv` converts both labels with: of the user-defined areas only C6A1–C8FE, and 0x80 → U+0080. |
| `ReceiverEucKrC1` | euc-kr | `EucKr` plus the 32 C1 controls; equals the glibc 2.39 EUC-KR charmap. |

`euc_kr` and `windows-932/949/950` are unavailable in this view.

## Data and licences

Production tables are in `charset/mapping-data/` as sorted big-endian `(u16 code,
u16 scalar)` records; no mapping exceeds U+FFFF. Unicode and Microsoft tables are
under the Unicode License V3 (`charset/UNICODE-LICENSE.txt`,
`charset/mapping-data/UNICODE-LICENSE.txt`). The Big5 index is WHATWG's, under the BSD
3-Clause License (`charset/mapping-data/WHATWG-LICENSE.txt`); it is held as
`big5.bin` and, for the plane 2 characters, `big5_plane2.bin` with the low 16 bits of
each scalar, and its text form for the tests, `tests/multibyte-mapping-data/INDEX-BIG5.TXT`,
was written from the tables the `encoding_rs` crate (0.8.35) generates from the index. The GNU Big5 charmap, with its
free-distribution notice, is `charset/mapping-data/RECEIVER-BIG5-DATA.txt`. The
original text tables, including KSC5601 for cross-checking, are in the crate's
`tests/multibyte-mapping-data/` and `tests/receiver-mapping-data/`. Mapping code is
original; no iconv or FFmpeg code is used.

## Decoder behaviour

`MultibyteDecoder` holds at most one lead byte and consumes one byte per work
unit, with at most sixteen binary-search comparisons. Each scalar keeps its
source byte range. Unmapped but well-formed pairs are errors; invalid trail
bytes are never replayed as ASCII. Chunk boundaries do not change the result.
The trail byte of a Big5 code that is two characters gives the letter and stays the
next byte to push (`position`), where it gives the mark.
A lead byte pending at `finish` is `Truncated`. The decoder does not allocate.

Tests compare every table entry at work quanta 1, 7 and 4096 and check all 256
single-byte and 65,536 two-byte inputs per profile.
