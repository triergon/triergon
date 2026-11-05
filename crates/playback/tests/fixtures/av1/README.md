Six frames of generated colour bars and moving shapes exercise the typed AV1
playback bridge. Both containers carry the same compressed video. The OBU
file records the output of FFmpeg 9.0.2's delimiter-insertion operation.

```sh
ffmpeg -f lavfi -i testsrc2=size=64x64:rate=3 -frames:v 6 -pix_fmt yuv420p \
  -c:v libsvtav1 -preset 12 -svtav1-params lp=1 -g 3 -crf 40 \
  -fflags +bitexact -flags:v +bitexact -movflags +faststart six-frames.mp4
ffmpeg -i six-frames.mp4 -map 0:v:0 -c copy -fflags +bitexact six-frames.webm
ffmpeg -i six-frames.mp4 -map 0:v:0 -c copy -bsf:v av1_metadata=td=insert \
  -f obu six-frames.obu
```
