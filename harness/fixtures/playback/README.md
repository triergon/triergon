# Playback startup and seek sources

Synthetic Matroska files for the `matroska-startup-seek` contract
(`harness/features/matroska-startup-seek/`). Together with five files in
`crates/playback/tests/fixtures/mkv-feed/`, they are the sources of its 46 capsules,
which `cargo xtask fixtures` builds and checks against the manifest's SHA-256.

The files were produced from synthetic test signals and muxed by FFmpeg 9.0.2
(libavformat 63.1.102); the HEVC streams were encoded by x265 4.3. Encoder output is
not reproducible across encoder builds, so the files are kept as committed and pinned
by the `source_sha256` of every manifest row.

| File | Video | Pictures | Reorder depth | Tracks | Cues |
|---|---|---|---|---|---|
| `avc-eight-pictures.mkv` | AVC | 8 | 0 | 1 | yes |
| `avc-high-b.mkv` | AVC | 12 | 2 | 1 | yes |
| `avc-high-b0.mkv` | AVC | 12 | 0 | 1 | yes |
| `avc-pgs-early-canvas.mkv` | AVC | 12 | 0 | 2 | yes |
| `avc-seek.mkv` | AVC | 64 | 2 | 1 | yes |
| `avc-six-pictures.mkv` | AVC | 6 | 0 | 1 | yes |
| `hevc-b0.mkv` | HEVC | 12 | 0 | 1 | yes |
| `hevc-b1.mkv` | HEVC | 12 | 1 | 1 | yes |
| `hevc-b3.mkv` | HEVC | 12 | 2 | 1 | yes |
| `hevc-seek-no-cues.mkv` | HEVC | 64 | 2 | 1 | no |
| `hevc-seek-no-default.mkv` | HEVC | 64 | 2 | 1 | yes |
| `hevc-vps-timed-no-default.mkv` | HEVC | 12 | 1 | 1 | yes |

`avc-pgs-early-canvas.mkv` is a stream copy of the authored AVC and PGS fixtures.
The first PGS composition specifies an 8×4 canvas. Its packet is placed at 300 ms,
after the eighth video packet. From the repository root, using the pinned FFmpeg
9.0.2 decode profile:

```sh
.research/reference-decode/install/bin/ffmpeg -nostdin -hide_banner -v error -y \
  -i harness/fixtures/playback/avc-high-b0.mkv \
  -i crates/playback/tests/fixtures/matroska-subtitle-probe/early-canvas.mkv \
  -map 0:v:0 -map 1:s:0 -map_metadata -1 -map_chapters -1 -c copy \
  -bsf:s 'setts=pts=300:dts=300' -fflags +bitexact \
  harness/fixtures/playback/avc-pgs-early-canvas.mkv
```
