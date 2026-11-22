/* Public FFmpeg operations for transport views. */
#include "reference_views_protocol.h"
#include "reference_transport_catalogue.h"
#include "reference_transport_packets.h"
#include "reference_transport_source.h"
#include "reference_transport_facts.h"
#include "reference_transport_reader.h"
#include "reference_transport_mux.h"

int main(int argc, char **argv)
{
    const ViewDispatch operations[] = {
        {"views-smoke-", view_smoke_prepare},
        {"ts-catalogue-", transport_catalogue_prepare},
        {"ts-psi-", transport_catalogue_prepare},
        {"ts-video-descriptors-", transport_catalogue_prepare},
        {"ts-packets-", transport_packets_prepare},
        {"ts-source-", transport_source_prepare},
        {"ts-pes-assembler-", transport_source_prepare},
        {"ts-observer-", transport_source_prepare},
        {"ts-facts-", transport_facts_prepare},
        {"ts-recovery-", transport_facts_prepare},
        {"ts-reader-views-", transport_reader_prepare},
        {"ts-reader-container-", transport_reader_prepare},
        {"ts-epoch-index-seek-", transport_reader_prepare},
        {"ts-bounded-time-", transport_reader_prepare},
        {"ts-mux-", transport_mux_prepare},
    };
    return view_main(argc, argv, operations, sizeof(operations) / sizeof(*operations));
}
