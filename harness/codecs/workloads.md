# Timing and memory workloads

The 20 workloads in `WORKLOADS.json` (input hashes, operation and iterations) are
generated with FFmpeg 9.0.2 (x264, x265 and native AAC encoders) from deterministic
lavfi sources:

    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=24 -t 4 -pix_fmt yuv420p -c:v libx264 -preset medium -profile:v high -bf 3 -refs 4 -colorspace bt709 -color_primaries bt709 -color_trc bt709 h264-high-1080p.h264
    ffmpeg -f lavfi -i testsrc2=size=1920x800:rate=24 -t 4 -pix_fmt yuv420p -c:v libx264 -profile:v main -bf 2 -colorspace bt709 h264-main-1920x800.h264
    ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=24 -t 4 -pix_fmt yuv420p -c:v libx264 -profile:v high -colorspace bt709 h264-high-720p.h264
    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=24 -t 4 -pix_fmt yuv420p -c:v libx265 -colorspace bt709 hevc-main-1080p.hevc
    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=24 -t 4 -pix_fmt yuv420p10le -c:v libx265 -profile:v main10 -colorspace bt2020nc -color_primaries bt2020 -color_trc smpte2084 hevc-main10-1080p.hevc
    ffmpeg -f lavfi -i testsrc2=size=3840x2160:rate=24 -t 1 -pix_fmt yuv420p10le -c:v libx265 -profile:v main10 hevc-main10-2160p.hevc

AAC-LC 5.1 (384 kbit/s) and stereo come from sine sources (`-f adts`). The two DTS
workloads are the cores of FATE `dts_es.dts` and `master_audio_7.1_24bit.dts`
(`-bsf:a dca_core`); the AC-3 and E-AC-3 workloads are FATE vectors used as they are.
The three HE-AAC workloads are the AAC lane's ADTS copies of FATE conformance vectors,
the same streams `correctness-manifest.json` hashes: `aac-he-stereo` and `aac-he-5.1` are
HE-AAC v1 (SBR, `al_sbr_cm_48_2` and `al_sbr_cm_48_5.1`) and `aac-hev2-stereo` is
HE-AAC v2 (SBR and Parametric Stereo, `al_sbr_ps_06_new`), each copied out of its MP4
with `-map 0:a:0 -c:a copy -f adts`.
Media stays outside git (`.research/codec-inputs`, `.research/fate-samples`). The inputs
under `.research/codec-inputs` are archived as they are under `evidence/codec-workload-inputs/`
in the evidence archive; fetch them rather than encode them again.

## FLAC, MPEG audio, Vorbis and Opus

`file-workloads.json` holds 10 workloads in the formats these codecs come in: native
FLAC (16-bit stereo at 44.1 kHz, 24-bit 5.1 at 96 kHz), MP3 (LAME CBR 128 kbit/s joint
stereo and VBR `-V 2`), MP2 (256 kbit/s), Ogg Vorbis (`-q 5` stereo, `-q 3` 5.1) and Ogg
Opus (128 kbit/s music, 24 kbit/s speech, 256 kbit/s 5.1).
`harness/codecs/file-workloads.sh FFMPEG` regenerates them byte for byte from
deterministic lavfi signals with flac 1.4.3, LAME 3.100, oggenc 1.4.2 and opusenc 0.2
(libopus 1.4) on Ubuntu 24.04 and FFmpeg's mp2 encoder.
