/* Public FFmpeg views worker. */
#include "reference_views_protocol.h"
#include "reference_nal_units.h"
#include "reference_obu.h"
#include "reference_adts.h"
#include "reference_config_views.h"
#include "reference_dts_frames.h"

int main(int argc, char **argv)
{
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"nal-units-", nal_units_prepare},
        {"nal-config-", nal_units_prepare},
        {"hvcc-annexb-", nal_units_prepare},
        {"obu-", obu_prepare},
        {"adts-", adts_prepare},
        {"config-", config_views_prepare},
        {"dts-core-", dts_frames_prepare},
        {"dts-packed-", dts_frames_prepare},
        {"dts-parser-", dts_frames_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
