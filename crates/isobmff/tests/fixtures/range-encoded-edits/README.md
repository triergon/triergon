# Encoded range edit fixtures

The Dolby and QuickTime fixtures use a generated sine and a solid color. They cover a
leading encoder-delay edit in each Dolby audio format and a reordered QuickTime
AVC track with an unspecified Macintosh language and a media handler distinct
from its data handler.

```sh
ffmpeg -nostdin -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=0.187' \
  -ac 2 -c:a ac3 -b:a 96k -use_editlist 1 -movie_timescale 48000 leading-ac3.mp4
ffmpeg -nostdin -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=0.187' \
  -ac 2 -c:a eac3 -b:a 96k -use_editlist 1 -movie_timescale 48000 leading-eac3.mp4
ffmpeg -nostdin -f lavfi -i 'color=c=blue:s=32x32:r=25:d=0.24' \
  -c:v libx264 -threads:v 1 -g 3 -bf 2 -pix_fmt yuv420p -use_editlist 1 quicktime-avc.mov
ffprobe -v error -fflags +noparse+nofillin -show_packets -of json <fixture>
```

The tests check all six packet records per file against FFprobe 9.0, including
timestamps, byte ranges and skip-samples side data. The Dolby fixtures use a
movie timescale of 48,000, so the edit duration converts exactly to media ticks.
[ETSI TS 102 366, Annex F](https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/01.04.01_60/ts_102366v010401p.pdf).
[QuickTime language code values](https://developer.apple.com/documentation/quicktime-file-format/language_code_values).

`aac-program-edit.mov` uses two generated tones and the repository licence.
Its channel configuration is zero, with a stereo Program Configuration Element
in AudioSpecificConfig. The edit skips 1,024 samples at the start. The test checks
all nine packet records and the trim against the packet view. Its SHA-256 is
`0042a7fcaa5815e7541c61ce569a66e3cfeb547861123e5554ca6ea453c16e73`.

```sh
ffmpeg -nostdin -f lavfi -i 'aevalsrc=0.13*sin(2*PI*440*t)|0.13*sin(2*PI*523*t):s=48000:c=stereo' \
  -frames:a 8 -c:a aac -aac_pce 1 -b:a 64k -threads:a 1 -flags:a +bitexact -fflags +bitexact \
  -use_editlist 1 -movie_timescale 48000 aac-program-edit.mov
```

`avc-opus-edit.mp4` is an authored sine and test pattern, with the same licence
as the repository. The Matroska intermediate gives the MP4 sample table unequal
first and last audio durations. The edit trims 336 samples, while `dOps` records
312. The video clock is 16,000 Hz. The SHA-256 of the fixture is
`109d8d19991c550fc5381c26c8a7fe9f1f006fa400a80d84ea31675cc0e0c18a`.

```sh
ffmpeg -nostdin -f lavfi -i 'testsrc2=size=96x64:rate=25' \
  -f lavfi -i 'sine=frequency=997:sample_rate=48000' -t 3.2 \
  -pix_fmt yuv420p -c:v libx264 -crf 30 -preset ultrafast -g 25 -keyint_min 25 -sc_threshold 0 -bf 0 \
  -c:a libopus -ac 2 -b:a 32k -map_metadata -1 -fflags +bitexact avc-opus-edit.mkv
ffmpeg -nostdin -i avc-opus-edit.mkv -map 0:v:0 -map 0:a:0 \
  -c copy -movflags +faststart -video_track_timescale 16000 -map_metadata -1 -fflags +bitexact avc-opus-edit.mp4
```
