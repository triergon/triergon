# subtitle-text inputs

Inputs of the `subtitle-text` feature (`harness/features/subtitle-text/`), written by
`harness/subtitle_corpus.py capsules SAMPLES`. The session capsules are derived byte
for byte from authored-synthetic fixtures already in the repository, except the ASS
packets and documents the script authors itself:

| Directory | Source |
| --- | --- |
| `mov-text/` | `crates/subtitles/tests/native-mov-text-fixtures/`: each case's extradata, packets, flushes, time base, frame size and clock from `PROVENANCE.json` |
| `tx3g/` | the samples and configurations each test in `crates/subtitles/tests/text_tx3g_semantics.rs` builds, recorded by running that test with `TRIERGON_TX3G_SAMPLES=SAMPLES`; consecutive repeats are folded into one packet, and each packet uses the tests' clock (1/90000, PTS 0, duration 135000) |
| `charset/` | the label and packet of each case in `crates/subtitles/tests/receiver-mapping-data/cases74.rs`, clock 1/1000, PTS 1000, duration 2000 |
| `ass/` | the `OpaqueAss` cases of `crates/subtitles/tests/text-fixtures/fixtures/manifest.json` with their codec private data (`ass-*.cap`), and the packets `ASS_PACKETS` authors for the decode call's UTF-8 check; same clock |
| `documents/` | the SubRip and WebVTT documents `DOCUMENTS` authors, one per rule of the `srt` and `webvtt` demuxers |

A capsule is `TRSTXT01`, a codec byte (1 mov_text, 2 ASS), a big-endian `u16`
charset label length and label, a `u32` operation count, then operations: `O`
(time base numerator and denominator, frame width and height as `u32`, an
extradata flag byte and `u32`-prefixed extradata), `P` (PTS and duration, each a
known byte and an `i64`, then a `u32`-prefixed payload) or `F` (flush). The
SubRip and WebVTT documents of the text fixtures are read in place from
`crates/subtitles/tests/text-fixtures/fixtures/input/`.
