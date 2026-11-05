The synthetic one-second Matroska input has eight AVC frames and two AAC LC
tracks with different core sample rates. FFmpeg 9.0.2 generated the fixture.

```sh
ffmpeg -f lavfi -i testsrc2=size=64x48:rate=8 \
  -f lavfi -i sine=frequency=443:sample_rate=48000 \
  -f lavfi -i sine=frequency=757:sample_rate=44100 \
  -map 0:v:0 -map 1:a:0 -map 2:a:0 -t 1 -c:v libx264 -pix_fmt yuv420p \
  -preset ultrafast -g 4 -bf 0 -threads:v 1 -c:a aac -b:a 32k -ac 1 \
  -fflags +bitexact -flags:v +bitexact -flags:a +bitexact three-tracks.mkv
```

The feed reference takes complete packets and CodecPrivate from FFprobe
with `-fflags +noparse+nofillin -show_streams -show_packets -show_data`.
An independent projection writes four-byte Annex-B prefixes, including
the configuration SPS/PPS before an IDR, and a seven-byte AAC LC ADTS
header. The binary contains one track index, signed big-endian 64-bit
reference PTS in nanoseconds, container key flag, AVC IDR flag, unsigned
big-endian 32-bit output length, and complete feed bytes per packet.
The Player also retains the more precise source clock through the feed.

The mixed `hevc-dolby.mkv` fixture remuxes the repository's synthetic
HEVC/AAC, AVC/AC-3 and AVC/E-AC-3 inputs. It contains ten HEVC pictures
with reordered PTS, 21 AC-3 packets and 21 E-AC-3 packets.

```sh
ffmpeg -i harness/fixtures/encoded/hevc-aac.mkv \
  -i harness/fixtures/encoded/avc-ac3.mkv -i harness/fixtures/encoded/avc-eac3.mkv \
  -map 0:v:0 -map 1:a:0 -map 2:a:0 -map_metadata -1 -map_chapters -1 \
  -c copy -t 0.64 hevc-dolby.mkv
ffprobe -v error -fflags +noparse+nofillin -show_packets -show_streams \
  -show_data -of json hevc-dolby.mkv > hevc-dolby.json
python3 project_hevc_dolby_feed.py hevc-dolby.json hevc-dolby.feed
```

This gold uses the same record layout with the HEVC IRAP flag in place
of the AVC IDR flag. Four-byte start codes precede every video NAL and
the hvcC VPS/SPS/PPS precede the IRAP picture. Dolby packets keep their
source bytes and packet boundaries. Both FFprobe parser settings
produce the same reference on this fixture.

The audio discovery fixtures are short remuxes of the synthetic tracks in
`three-tracks.mkv` and `hevc-dolby.mkv`. The mixed fixture contains two
AAC-LC tracks and one each of AC-3 and E-AC-3. The damaged control uses
the same Dolby tracks and replaces the first AC-3 packet's initial sync
byte with zero. Its declared sample rate and channel count remain intact.

`avc-reordered-discovery.mkv` contains twelve 64×48 pictures at 24 fps
from `testsrc2`, encoded with libx264 High profile and one B picture.
`hevc-no-default-duration-discovery.mkv` encodes the same synthetic
sequence with libx265 and one B picture. The original DefaultDuration
element is replaced with a same-size EBML Void so packet offsets are
preserved.

`hevc-indexed-seek.mkv` contains 64 synthetic 96×64 pictures at 24 fps
with a closed eight-picture GOP, two B pictures, 280 ms cluster limit
and Cues at the front. A seek to 700 ms resolves to the 667 ms keyframe
inside a cluster containing four earlier video packets.
