/* Public FFmpeg operations for matroska views. */
#include "reference_views_protocol.h"
#include "reference_matroska_side_data.h"
#include "reference_matroska_metadata.h"

int main(int argc, char **argv)
{
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"mkv-side-data-", matroska_side_data_prepare},
        {"mkv-codec-state-", matroska_side_data_prepare},
        {"mkv-track-extensions-", matroska_side_data_prepare},
        {"mkv-attachments-", matroska_metadata_prepare},
        {"mkv-metadata-", matroska_metadata_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
