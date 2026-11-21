/* Public FFmpeg operations for subtitle views. */
#include "reference_views_protocol.h"
#include "reference_subtitle_views_text.h"
#include "reference_subtitle_views_source.h"

int main(int argc, char **argv)
{
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"text-extract-", subtitle_views_text_prepare},
        {"renderer-context-", subtitle_views_text_prepare},
        {"charset-", subtitle_views_text_prepare},
        {"charset-packets-", subtitle_views_text_prepare},
        {"tx3g-config-", subtitle_views_text_prepare},
        {"carriage-", subtitle_views_text_prepare},
        {"dvb-source-", subtitle_views_source_prepare},
        {"vobsub-lines-", subtitle_views_source_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
