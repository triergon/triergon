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

## Fragmented MP4

The two files in `../mp4/` contain synthetic AVC and AAC, one timed-text cue
from 0.200 to 2.600 seconds, and the DVD subpictures from `dvd-commands.mp4`.
`fragmented-subtitles.mp4` has no edits and includes non-sync text samples at
the beginning and in the last fragment. `fragmented-subtitle-edit.mp4` has a
200 ms leading empty subpicture edit before its open-ended media edit.

The MP4 range contract checks open and video seeks at 0, 1.5 and approximately
3.66 seconds. The video timescale is 16,384 ticks per second.

```sh
ffmpeg -nostdin -v error -y -f lavfi -i testsrc2=size=96x64:rate=8 \
    -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 -t 4 \
    -c:v libx264 -threads 1 -preset ultrafast -pix_fmt yuv420p -g 8 \
    -sc_threshold 0 -bf 0 -c:a aac -ac 2 av.mp4
printf '%s\n' 1 '00:00:00,200 --> 00:00:02,600' 'Across the seek' > cues.srt
ffmpeg -nostdin -v error -y -i av.mp4 -i cues.srt -itsoffset 0.2 -i dvd-commands.mp4 \
    -map 0:v:0 -map 0:a:0 -map 1:s:0 -map 2:s:0 -t 4 -c copy -c:s:0 mov_text subtitles.mp4
ffmpeg -nostdin -v error -y -i subtitles.mp4 -map 0 -c copy \
    -movflags +empty_moov+frag_keyframe+default_base_moof -use_editlist 0 fragmented-subtitles.mp4
ffmpeg -nostdin -v error -y -i subtitles.mp4 -map 0 -c copy \
    -movflags +delay_moov+frag_keyframe+default_base_moof fragmented-subtitle-edit.mp4
```
