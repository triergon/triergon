# Subtitle bridge fixtures

Authored synthetic streams, copied from the deterministic subtitle corpus
generators in `harness/subtitles/` (output under `.research/subtitle-corpus/`).
Expected values in `subtitle_bridges.rs` come from the reference worker
(`target/harness/ffmpeg-subtitles-worker sub-trace FILE KIND`, FFmpeg
`d32b387f`).

| File | Generator, case | SHA-256 |
|---|---|---|
| `pgs-basic.m2ts` | `pgs_streams.py`, `basic` (M2TS carrier, PGS on PID 0x1200) | `92a2d25df2e6a322be96a67be382acc0a777815c04082f0dd11bc5abd96001d3` |
| `teletext-basic.ts` | `teletext_streams.py`, `basic` (teletext on PID 0x100) | `6beb3af3849df22437f9356b1dcae781fe0e4bb44224f65628549429630b538d` |
| `dvd-commands.mp4` | `dvd_streams.py`, `commands`, muxed with `ffmpeg -i commands.idx -map 0 -c copy commands.mp4` | `34a22e6f89a858d78f5cd1cd0fd5b62843ffa0c9634fe71f1447e0a8d5e4087f` |
| `cc-h264-a53.mp4` | `cc_streams.py`, `h264-a53` (libx264 encode with A/53 SEI, muxed with `ffmpeg -c copy`) | `27a9f78ad9151fd8b707d2d675de008bd4d94b25fb0b8046ead5558b2a89abf7` |

Muxing and encoding use Homebrew `ffmpeg` 9.0.2 as an authoring tool. The
caption times are the reference's plus the MP4 edit list's media time (3003
ticks at 90 kHz), which the playback source view keeps.
