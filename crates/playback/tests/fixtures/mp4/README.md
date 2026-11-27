The fixtures named here are synthetic and use the repository's dual MIT or
Apache-2.0 licence. The [`mp4-range`](../../../../../harness/features/mp4-range/corpus.json)
and [`mp4-forward`](../../../../../harness/features/mp4-forward/corpus.json) corpora
record their SHA-256.

## Range playback

`avc-aac-quicktime-surround.mov` contains synthetic AVC and six-channel, 48 kHz
AAC. The range comparison also runs with the optional format and marker atoms
replaced by free boxes. `avc-aac-quicktime-reordered.mov` adds 44.1 kHz AAC and
AVC with B pictures. `avc-aac-quicktime-fragmented.mov` exercises open and seek
with empty initialization sample tables and a version 1 sound description. It
remuxes the synthetic `fragmented-avc-aac.mp4` fixture.
`avc-aac-quicktime-7-1.mov` uses the same synthetic video pattern and distinct
tones at 251, 379, 503, 71, 641, 769, 911 and 1031 Hz for the eight speakers.
FFmpeg 9.0.2 encoded the fixture as 44.1 kHz AAC-LC with channel configuration
12.

`avc-aac-program.mov` uses an authored AVC pattern and seven independent tones
in a 48 kHz AAC-LC program with a leading edit. The range corpus compares its
original configuration and every selected packet on open and backward seek.
To regenerate it with FFmpeg 9.0.2 and libx264:

```sh
signal='0.13*sin(2*PI*107*t)|0.13*sin(2*PI*211*t)|0.13*sin(2*PI*307*t)|0.13*sin(2*PI*53*t)|0.13*sin(2*PI*409*t)|0.13*sin(2*PI*509*t)|0.13*sin(2*PI*601*t)'
ffmpeg -nostdin -f lavfi -i 'testsrc2=s=96x64:r=25' \
  -f lavfi -i "aevalsrc=${signal}:s=48000:c=6.1(back)" -t 2.4 \
  -c:v libx264 -threads:v 1 -g 25 -keyint_min 25 -sc_threshold 0 -bf 0 -pix_fmt yuv420p \
  -c:a aac -aac_pce 1 -threads:a 1 -b:a 224k -flags:a +bitexact -fflags +bitexact \
  -video_track_timescale 16000 -movie_timescale 48000 -use_editlist 1 -movflags +faststart \
  avc-aac-program.mov
```

`avc-aac-rtp-hints.mp4` contains a synthetic AVC test pattern, AAC sine audio
and two RTP hint tracks, with the first hint track between the video and audio
tracks.

## Forward playback

`forward-aac-priming.mp4` uses a 64 × 48 AVC test pattern at 12 fps and a
440 Hz mono AAC tone at 44.1 kHz. To reproduce it with FFmpeg 9.0.2 and
libx264, run:

```sh
ffmpeg -nostdin -v error \
  -f lavfi -i 'testsrc2=s=64x48:r=12' -f lavfi -i 'sine=f=440:r=44100' \
  -t 0.35 -map 0:v:0 -map 1:a:0 -c:v libx264 -threads:v 1 -profile:v baseline \
  -pix_fmt yuv420p -g 2 -bf 0 -c:a aac -b:a 40k -movflags +faststart forward-aac-priming.mp4
```

`forward-aac-tail-trim.mp4` keeps the 44,100 Hz movie timescale and sets
the audio edit to media time zero and duration 16,459.
`forward-aac-fractional-edit.mp4` sets the movie timescale to 1,000, its
duration and the video edit's duration to 334, and the audio edit's media
time and duration to 64 and 45. The audio edit rounds to an end one sample
into the third packet.

`forward-fragment-aac-edit.mp4` and `forward-fragment-aac-late-track.mp4`
use an FFmpeg-generated AVC test pattern and mono AAC tone. Both carry an
open-ended audio initialization edit. The latter starts with a video-only
fragment. `forward-fragment-aac-short.mp4` truncates an authored AVC/AAC
fragment inside its first AAC packet. `forward-fragment-mp3-edit.mp4` uses a
generated mono MP3 tone with an initialization edit to exercise the same clock
projection for a different MPEG-4 audio object type. The `mp4-forward` corpus
compares source byte offsets, reduced time bases and AAC trim along with the
packet data.
