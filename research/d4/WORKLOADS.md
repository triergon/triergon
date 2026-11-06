# D4 performance/memory workloads (frozen before grading)

Generated on the development Mac with Homebrew FFmpeg 9.0.2 (x264/x265/aac encoders),
deterministic lavfi sources:

    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=24 -t 4 -pix_fmt yuv420p -c:v libx264 -preset medium -profile:v high -bf 3 -refs 4 -colorspace bt709 -color_primaries bt709 -color_trc bt709 h264-high-1080p.h264
    ffmpeg -f lavfi -i testsrc2=size=1920x800:rate=24 -t 4 -pix_fmt yuv420p -c:v libx264 -profile:v main -bf 2 -colorspace bt709 h264-main-1920x800.h264
    ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=24 -t 4 -pix_fmt yuv420p -c:v libx264 -profile:v high -colorspace bt709 h264-high-720p.h264
    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=24 -t 4 -pix_fmt yuv420p -c:v libx265 -colorspace bt709 hevc-main-1080p.hevc
    ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=24 -t 4 -pix_fmt yuv420p10le -c:v libx265 -profile:v main10 -colorspace bt2020nc -color_primaries bt2020 -color_trc smpte2084 hevc-main10-1080p.hevc
    ffmpeg -f lavfi -i testsrc2=size=3840x2160:rate=24 -t 1 -pix_fmt yuv420p10le -c:v libx265 -profile:v main10 hevc-main10-2160p.hevc
    5.1 AAC-LC 384k and stereo AAC-LC from sine sources (-f adts); DTS core via -bsf:a dca_core

Hashes and the operation/iteration per workload are in WORKLOADS.json. Large media stays
outside git (`.research/m1d4/d4-inputs`).
