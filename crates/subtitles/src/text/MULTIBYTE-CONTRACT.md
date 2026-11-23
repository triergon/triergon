# Multibyte charset profiles

Mapping data, sources and licences for `text::charset::multibyte`. Each profile
is selected explicitly with `MultibyteCharset::from_label`, `Charset::Multibyte`
or `Encoding::Multibyte`; no label or content is guessed.

A strict profile follows the current definition of its charset: the decoder of the
WHATWG Encoding Standard for its label, and for a `cp` label the Windows code page as
Windows converts it. A `Receiver` profile follows the C library of the reference build
instead, and is what `compat::charset::receiver_charset` selects.

## Strict profiles (`MultibyteCharset::from_label`)

| Profile | Labels (ASCII case-insensitive) | Mapping data |
| --- | --- | --- |
| `Gbk` | gbk, cp936, windows-936 | GBK as the Encoding Standard decodes it: index-gb18030 for the two-byte codes (lead bytes 81–FE, trail bytes 40–7E and 80–FE), which holds every CP936 2.01 code and 2,149 more of GB18030; 0x80 → U+20AC; the four-byte codes of GB18030 by index-gb18030-ranges, pointer 7457 as U+E7C7, and pointers 189000 to 1237575 as U+10000 onwards. |
| `Big5` | big5, big-5 | Big5 as the Encoding Standard decodes it (index-big5): lead bytes 81–FE, the Hong Kong supplement with 1,713 plane 2 characters, and the codes 8862, 8864, 88A3 and 88A5 as a letter and a combining mark each. No single byte beyond ASCII. |
| `ShiftJis` | shift_jis, shift-jis, sjis | Shift_JIS as the Encoding Standard decodes it: index-jis0208 (lead bytes 81–9F and E0–FC), the user-defined codes F040–F9FC → U+E000–U+E757, 0x80 → U+0080, A1–DF → U+FF61–U+FF9F, ASCII as itself: the table of `Windows932`, with 0x80. |
| `EucKr` | euc-kr, euc_kr | EUC-KR as the Encoding Standard decodes it: index-euc-kr (lead bytes 81–FE, trail bytes 41–FE), KS X 1001 with the UHC extension pairs, the table of `Windows949`. A pair in the gaps 5B–60 and 7B–80 is unmapped where `Windows949` refuses its trail byte. No single byte beyond ASCII. |
| `Windows932` | cp932, windows-932 | Microsoft CP932 table 2.01 plus the 1,880 user-defined codes F040–F9FC → U+E000–U+E757, as Windows converts them. |
| `Windows949` | cp949, windows-949 | Microsoft CP949 table 2.01, including UHC extensions. |
| `Windows950` | cp950, windows-950 | Microsoft CP950 table 2.01 plus the 6,217 user-defined codes FA40–FEFE, 8E40–A0FE, 8140–8DFE and C6A1–C8FE → U+E000–U+F848 in that order, 0x80 → U+0080 and 0xFF → U+F8F8, as Windows converts them. |

Other labels (gb18030 among them), padded labels and ISO-2022 are refused.

## FFmpeg/glibc profiles (`compat::charset::receiver_charset`)

These match the label set and repertoire the pinned FFmpeg build accepts through
glibc 2.39 iconv when converting ASS packets (`compat::charset::AssPacketJob`).

| Profile | Labels | Mapping data |
| --- | --- | --- |
| `ReceiverGbk` | gbk, cp936, windows-936 | Microsoft CP936 table 2.01, including 0x80 → U+20AC. No four-byte codes. |
| `ReceiverShiftJis` | shift_jis, shift-jis, sjis | Unicode historical SHIFTJIS table 2.0 (0x5C yen, 0x7E overline, lead bytes 81–9F and E0–EF), plus C0/DEL identity, with 815F → U+FF3C (JIS X 0208 1-32, as JIS0208.TXT and Windows map it) where SHIFTJIS.TXT gives U+005C. |
| `ReceiverEucKrC1` | euc-kr | KS X 1001:2002 in EUC-KR: CP949 table 2.01 restricted to A1–FE byte pairs, plus A2E8 → U+327E, which KS X 1001:2002 adds and CP949 2.01 predates, ASCII and the 32 C1 controls 80–9F: the glibc 2.39 EUC-KR charmap. |
| `ReceiverBig5` | big5, big-5, cp950 | GNU Big5 charmap 0.92 from glibc 2.39 (14,040 records), which its `iconv` converts both labels with: of the user-defined areas only C6A1–C8FE, and 0x80 → U+0080. |
| `Windows932`, `Windows949` | cp932, cp949 | As above. |

`euc_kr` and `windows-932/949/950` are unavailable in this view.

## Data and licences

Production tables are in `charset/mapping-data/` as sorted big-endian `(u16 code,
u16 scalar)` records, codes below 0x100 standing for single bytes. Unicode and
Microsoft tables are under the Unicode License V3 (`charset/UNICODE-LICENSE.txt`,
`charset/mapping-data/UNICODE-LICENSE.txt`). The Encoding Standard's indexes are
WHATWG's, under the BSD 3-Clause License (`charset/mapping-data/WHATWG-LICENSE.txt`),
in the edition dated 2024-09-18 (each index file names its identifier, recorded in
`tests/multibyte-mapping-data/SOURCE-LOCK.json`). They are not in the repository;
https://encoding.spec.whatwg.org/ publishes them. `cargo xtask subtitle-views
charset-tables big5 INDEX` and `cargo xtask subtitle-views charset-tables gbk INDEX
RANGES` read `index-big5.txt`, `index-gb18030.txt` and `index-gb18030-ranges.txt`,
compare the tables with them, and with `--write` write them: `big5.bin` and, for the
plane 2 characters, `big5_plane2.bin` with the low 16 bits of each scalar; `gbk.bin`
and `gb18030_ranges.bin`, the four-byte ranges below U+10000 as `(u16 pointer, u16
scalar)` records. `ShiftJis` and `EucKr` decode `windows932.bin` and `windows949.bin`:
`cargo xtask subtitle-views charset-tables shift_jis INDEX` and `... euc-kr INDEX`
check that `index-jis0208.txt` and `index-euc-kr.txt` give those tables' records
beyond ASCII, with Shift_JIS's 0x80, which the decoder gives as itself. The tests decode every one- and
two-byte input of the Big5 tables and check hand-picked pointers of `index-big5.txt` with their code points.
The receiver tables are `receiver_gbk.bin`, `receiver_shift_jis.bin` and
`receiver_euc_kr.bin` from the Microsoft and Unicode tables above, and the GNU Big5
charmap, with its free-distribution notice, `charset/mapping-data/RECEIVER-BIG5-DATA.txt`
(`receiver_big5.bin`). The original text tables, including KSC5601 for cross-checking,
are in the crate's `tests/multibyte-mapping-data/` and `tests/receiver-mapping-data/`.
Mapping code is original; no iconv or FFmpeg code is used.

## Decoder behaviour

`MultibyteDecoder` holds at most one lead byte, or the part of a four-byte GB18030
pointer read so far, and consumes one byte per work unit, with at most sixteen
binary-search comparisons. Each scalar keeps its
source byte range. Unmapped but well-formed pairs are errors; invalid trail
bytes are never replayed as ASCII. Chunk boundaries do not change the result.
The trail byte of a Big5 code that is two characters gives the letter and stays the
next byte to push (`position`), where it gives the mark.
A lead byte pending at `finish` is `Truncated`. The decoder does not allocate.

Tests compare every table entry at work quanta 1, 7 and 4096 and check all 256
single-byte and 65,536 two-byte inputs per profile.
