/* Public FFmpeg operations for playback views. */
#include "reference_views_protocol.h"
#ifdef TRIERGON_PLAYBACK_SUBTITLE_WRAP
#include "reference_subtitles_host.h"
#endif
#include "reference_playback_probe.h"
#include "reference_playback_source_view.h"
#include "reference_playback_player.h"
#include "reference_playback_hls.h"
#include "reference_playback_bridge.h"
#include "reference_playback_subtitles.h"

int main(int argc, char **argv)
{
    if (argc > 1 && !strncmp(argv[1], "subtitle-bridge-", sizeof("subtitle-bridge-") - 1))
        return playback_subtitle_command(argc, argv);
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"probe-", playback_probe_prepare},
        {"facade-probe-", playback_probe_prepare},
        {"source-view-", playback_source_view_prepare},
        {"mp4-seek-", playback_player_prepare},
        {"catalogue-", playback_player_prepare},
        {"delivery-", playback_player_prepare},
        {"select-", playback_player_prepare},
        {"mp4-index-", playback_player_prepare},
        {"ts-pes-open-", playback_player_prepare},
        {"hls-views-", playback_hls_prepare},
        {"access-units-", playback_bridge_prepare},
        {"mp4-bridges-", playback_bridge_prepare},
        {"mkv-discovery-", playback_bridge_prepare},
        {"text-bridge-", playback_subtitles_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
