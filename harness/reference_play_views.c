/* Public FFmpeg operations for play views. */
#include "reference_play_raw.h"
#include "reference_playback_probe.h"

int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "play-raw"))
        return play_raw_command(argc, argv);
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"play-", play_raw_prepare},
        {"facade-probe-", playback_probe_prepare},
        {"any-ts-au-", play_raw_prepare},
        {"any-seek-", play_raw_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
