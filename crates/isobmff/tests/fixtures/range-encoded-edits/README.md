# Encoded range edit fixtures

These three synthetic files use a generated sine and a solid color. They cover a
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
