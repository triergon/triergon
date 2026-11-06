# Multibyte charset profiles

Mapping data, sources and licences for `text::charset::multibyte`. Each profile
is selected explicitly with `MultibyteCharset::from_label`, `Charset::Multibyte`
or `Encoding::Multibyte`; no label or content is guessed.

## Strict profiles (`MultibyteCharset::from_label`)

| Profile | Labels (ASCII case-insensitive) | Mapping data |
| --- | --- | --- |
| `GbkCp936` | gbk, cp936, windows-936 | Microsoft CP936 table 2.01, including 0x80 → U+20AC. Not GB18030. |
| `Big5Unicode` | big5, big-5 | Unicode historical BIG5 table 2.0; its seven NO MAPPING entries are errors. |
| `ShiftJisUnicode` | shift_jis, shift-jis, sjis | Unicode historical SHIFTJIS table 2.0 (0x5C yen, 0x7E overline), plus C0/DEL identity. |
| `EucKr` | euc-kr, euc_kr | CP949 table 2.01 restricted to A1–FE byte pairs, plus ASCII. UHC extension pairs are refused. |
| `Windows932` | cp932, windows-932 | Microsoft CP932 table 2.01. |
| `Windows949` | cp949, windows-949 | Microsoft CP949 table 2.01, including UHC extensions. |
| `Windows950` | cp950, windows-950 | Microsoft CP950 table 2.01. |

Other aliases, padded labels, HKSCS, GB18030 and ISO-2022 are refused.

## FFmpeg/glibc profiles (`native_charset::receiver_charset`)

These match the label set and repertoire the pinned FFmpeg build accepts through
glibc iconv when converting ASS packets (`native_charset::AssPacketJob`).

| Profile | Labels | Mapping data |
| --- | --- | --- |
| `GbkCp936`, `ShiftJisUnicode`, `Windows949`, `Windows950` | gbk, cp936, windows-936, shift_jis, shift-jis, sjis, cp949, cp950 | As above. |
| `ReceiverBig5Glibc239` | big5, big-5 | GNU Big5 charmap 0.92 from glibc 2.39 (14,040 records). |
| `ReceiverEucKrC1` | euc-kr | `EucKr` plus the 32 C1 controls. |
| `ReceiverEucKrGlibc239` | euc-kr (`receiver_charset_v2` only) | `ReceiverEucKrC1` plus A2E8 → U+327E; equals the glibc 2.39 EUC-KR charmap. |
| `ReceiverWindows932Pua` | cp932 | CP932 plus the 1,880 user-defined codes F040–F9FC → U+E000–U+E757. |

`euc_kr` and `windows-932/949/950` are unavailable in this view.

## Data and licences

Production tables are in `mapping-data/` as sorted big-endian `(u16 code, u16
scalar)` records; no mapping exceeds U+FFFF. Unicode and Microsoft tables are
under the Unicode License V3 (`charset/UNICODE-LICENSE.txt`,
`mapping-data/UNICODE-LICENSE.txt`). The GNU Big5 charmap, with its
free-distribution notice, is `mapping-data/RECEIVER-BIG5-DATA.txt`. The original text tables, including
KSC5601 for cross-checking, are in `tests/multibyte-mapping-data/` and
`tests/receiver-mapping-data/`. Mapping code is original; no iconv or FFmpeg
code is used.

## Decoder behaviour

`MultibyteDecoder` holds at most one lead byte and consumes one byte per work
unit, with at most sixteen binary-search comparisons. Each scalar keeps its
source byte range. Unmapped but well-formed pairs are errors; invalid trail
bytes are never replayed as ASCII. Chunk boundaries do not change the result.
A lead byte pending at `finish` is `Truncated`. The decoder does not allocate.

Tests compare every table entry at work quanta 1, 7 and 4096 and check all 256
single-byte and 65,536 two-byte inputs per profile.
