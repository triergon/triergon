/* Public FFmpeg operations for codec views. */
#include "reference_views_protocol.h"
#include "reference_codec_readers.h"
#include "reference_codec_video.h"
#include "reference_codec_audio.h"
#include "reference_codec_blocks.h"

int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "dsp-raw"))
        return codec_blocks_raw(argc, argv);
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"media-probe-", codec_readers_prepare},
        {"elementary-metadata-", codec_readers_prepare},
        {"ogg-tracks-", codec_readers_prepare},
        {"ogg-pages-", codec_readers_prepare},
        {"picture-props-", codec_video_prepare},
        {"h264-inspect-", codec_video_prepare},
        {"hevc-access-units-", codec_video_prepare},
        {"video-nal-", codec_video_prepare},
        {"adts-sync-", codec_audio_prepare},
        {"ac3-header-", codec_audio_prepare},
        {"ac3-frame-info-", codec_audio_prepare},
        {"layouts-", codec_audio_prepare},
        {"crc-", codec_blocks_prepare},
        {"rescale-", codec_blocks_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
