/* Public FFmpeg views worker. */
#include "reference_views_protocol.h"
#include "reference_isobmff_packets.h"
#include "reference_isobmff_index.h"
#include "reference_isobmff_trace.h"

int main(int argc, char **argv)
{
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"qt-pcm-", isobmff_packets_prepare},
        {"mov-pcm-", isobmff_packets_prepare},
        {"mov-recover-", isobmff_packets_prepare},
        {"dts-entry-", isobmff_packets_prepare},
        {"mov-encoded-", isobmff_packets_prepare},
        {"mov-track-seek-", isobmff_index_prepare},
        {"source-seek-", isobmff_index_prepare},
        {"mov-trace-", isobmff_trace_prepare},
        {"index-landings-", isobmff_trace_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
