# Indexed Opus fixture

`cued-opus.mka` remuxes the first second of the repository-licensed encoded
audio in `crates/isobmff/tests/fixtures/range-encoded-edits/avc-opus-edit.mp4`.
With FFmpeg 9.0.2 it is reproducible from the repository root as follows:

```sh
ffmpeg -nostdin -v error -y -i crates/isobmff/tests/fixtures/range-encoded-edits/avc-opus-edit.mp4 \
  -map 0:a:0 -c copy -t 1 -map_metadata -1 -fflags +bitexact \
  -cluster_time_limit 100 crates/media/tests/fixtures/seek/cued-opus.mka
```

SHA-256: `aa098b1bbb3b7283ad93b9668d2e7bb2db69c7fcdba7ef3423b107a2d48d77a2`.
The Segment has 1 ms ticks, CodecDelay 7 ms, SeekPreRoll 80 ms and CueTimes
0, 81, 181, 281, 381, 481, 581, 681, 781, 881 and 981. The tests select
the greatest CueTime no later than the floored `(target - 80 ms + 7 ms)`;
when this is before the first Cue but the requested time reaches it, they use
the first Cue. The source packet clock subtracts 7 ms. A request at 180 ms
lands at 74 ms for playback preroll and 174 ms for previous random access.
The pinned FFmpeg source-view worker with `AVSEEK_FLAG_BACKWARD` starts at
94 ms for that request. The pinned formats ffprobe with
`-fflags +noparse+nofillin -read_intervals '0.18%+#4'` starts at 174 ms.
An equivalent reference for the complete seek operation remains open.
