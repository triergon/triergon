# H.264 notes

Supported features, refusals and the verification method are in
[crates/codec-h264](../../../crates/codec-h264/README.md). The decoder is written from
Rec. ITU-T H.264 (V15, 08/2024); FFmpeg 9.0.2 was used only as a black-box binary
(`framemd5`, raw output, `ffprobe`). Tables were transcribed from the specification text
with the table number in a comment; Table 9-26 prints ctxIdx 641 in the 484–571 range
where 541 is meant. Per-picture reference digests are in
`crates/codec-h264/tests/ffmpeg_framemd5_digests.txt`.
