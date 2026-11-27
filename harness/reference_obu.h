/* AV1 units returned by the public filter_units bitstream filter. */
#include <libavcodec/bsf.h>
#include <libavutil/intreadwrite.h>
#include <libavutil/opt.h>

typedef struct {
    const uint8_t *data;
    uint32_t size;
} ObuTemporalUnit;

typedef struct {
    int single;
    size_t count;
    ObuTemporalUnit units[];
} ObuCase;

static void stream_record(ViewWork *work, uint64_t kind, const uint64_t *words,
                          size_t count, const uint8_t *bytes, size_t size)
{
    view_record(work, kind, words, count, bytes, size);
    if (work->trace) {
        printf("%" PRIu64, kind);
        for (size_t i = 0; i < count; ++i)
            printf(" %" PRIu64, words[i]);
        if (size) {
            putchar(' ');
            for (size_t i = 0; i < size; ++i)
                printf("%02x", bytes[i]);
        }
        putchar('\n');
    }
}

static int obu_leb(const uint8_t **data, size_t *remaining, size_t *length)
{
    uint64_t value = 0;
    for (unsigned i = 0; i < 8 && *remaining; ++i) {
        unsigned byte = *(*data)++;
        --*remaining;
        value |= (uint64_t)(byte & 127) << (7 * i);
        if (!(byte & 128)) {
            if (value > *remaining || value > UINT32_MAX)
                return AVERROR_INVALIDDATA;
            *length = (size_t)value;
            return 0;
        }
    }
    return AVERROR_INVALIDDATA;
}

/* Find the complete low-overhead prefix before a truncated unit. The filter is
 * packet-atomic, so submit the complete prefix separately to observe its output. */
static size_t obu_complete_prefix(const uint8_t *data, size_t size)
{
    size_t remaining = size;
    while (remaining) {
        const size_t start = size - remaining;
        const uint8_t header = *data++;
        --remaining;
        if (!(header & 2))
            return 0;
        if (header & 4) {
            if (!remaining)
                return start;
            ++data;
            --remaining;
        }
        size_t length;
        if (obu_leb(&data, &remaining, &length) < 0)
            return start;
        data += length;
        remaining -= length;
    }
    return 0;
}

/* Read the OBU envelope independently of the payload syntax (AV1 §§5.2–5.3). */
static int obu_output(ViewWork *work, uint64_t index,
                      const uint8_t *data, size_t size, int complete, int low_overhead)
{
    uint64_t count = 0;
    while (size) {
        uint8_t header = *data++;
        --size;
        int has_extension = (header & 4) != 0, sized = (header & 2) != 0;
        if ((has_extension && !size) || (low_overhead && !sized))
            return AVERROR_INVALIDDATA;
        uint8_t extension = 0;
        if (has_extension) {
            extension = *data++;
            --size;
        }
        size_t length = size;
        if (sized && obu_leb(&data, &size, &length) < 0)
            return AVERROR_INVALIDDATA;
        const unsigned type = (header >> 3) & 15;
        const uint64_t words[] = {
            index, type, has_extension, extension >> 5,
            (extension >> 3) & 3, sized, length
        };
        stream_record(work, 1, words, 7, data, length);
        ++count;
        data += length;
        size -= length;
    }
    if (complete) {
        const uint64_t end[] = {index, count};
        stream_record(work, 7, end, 2, NULL, 0);
    }
    return 0;
}

static int obu_wire_run(const ViewCase *input, ViewWork *work)
{
    const ObuCase *source = input->prepared;
    for (size_t i = 0; i < source->count; ++i) {
        const ObuTemporalUnit *unit = &source->units[i];
        if (obu_output(work, i, unit->data, unit->size, 1, 1) < 0) {
            const uint64_t index = i;
            stream_record(work, 8, &index, 1, NULL, 0);
        }
    }
    return 0;
}

static int obu_filter(AVBSFContext *filter, AVPacket *packet,
                      const uint8_t *data, size_t size)
{
    int status = av_new_packet(packet, (int)size);
    if (status < 0)
        return status;
    memcpy(packet->data, data, size);
    status = av_bsf_send_packet(filter, packet);
    return status < 0 ? status : av_bsf_receive_packet(filter, packet);
}

static int obu_run(const ViewCase *input, ViewWork *work)
{
    const ObuCase *source = input->prepared;
    AVBSFContext *filter = NULL;
    AVPacket *packet = av_packet_alloc();
    int result = 2;
    const AVBitStreamFilter *implementation = av_bsf_get_by_name("filter_units");
    if (!implementation || !packet || av_bsf_alloc(implementation, &filter) < 0)
        goto done;
    filter->par_in->codec_type = AVMEDIA_TYPE_VIDEO;
    filter->par_in->codec_id = AV_CODEC_ID_AV1;
    if (av_opt_set(filter->priv_data, "remove_types", input->argv[2], 0) < 0 ||
        av_bsf_init(filter) < 0)
        goto done;

    for (size_t i = 0; i < source->count; ++i) {
        const ObuTemporalUnit *unit = &source->units[i];
        uint64_t index = i;
        if (!unit->size && !source->single) {
            const uint64_t end[] = {i, 0};
            stream_record(work, 7, end, 2, NULL, 0);
            continue;
        }
        int status = unit->size ? obu_filter(filter, packet, unit->data, unit->size)
                                : AVERROR_INVALIDDATA;
        if (status == AVERROR(ENOMEM))
            goto done;
        if (status >= 0)
            status = obu_output(work, i, packet->data, (size_t)packet->size, 1, 0);
        if (status < 0) {
            if (work->trace)
                fprintf(stderr, "OBU unit %zu: %d\n", i, status);
            if (status == AVERROR_INVALIDDATA && !source->single) {
                const size_t prefix = obu_complete_prefix(unit->data, unit->size);
                if (prefix) {
                    av_packet_unref(packet);
                    av_bsf_flush(filter);
                    const int recovered = obu_filter(filter, packet, unit->data, prefix);
                    if (recovered == AVERROR(ENOMEM))
                        goto done;
                    if (recovered >= 0 &&
                        obu_output(work, i, packet->data, (size_t)packet->size, 0, 0) < 0)
                        goto done;
                }
            }
            stream_record(work, 8, &index, 1, NULL, 0);
        }
        av_packet_unref(packet);
    }
    result = 0;
done:
    av_packet_free(&packet);
    av_bsf_free(&filter);
    return result;
}

static int obu_prepare(ViewCase *input)
{
    if (input->argc != 3 ||
        (strcmp(input->argv[0], "low-overhead") && strcmp(input->argv[0], "single")) ||
        (strcmp(input->argv[1], "1") && strcmp(input->argv[1], "7") &&
         strcmp(input->argv[1], "4096")) ||
        (strcmp(input->argv[2], "0") && strcmp(input->argv[2], "14") &&
         (strcmp(input->argv[2], "wire") || strcmp(input->argv[0], "low-overhead"))) ||
        input->size < 32 || memcmp(input->data, "DKIF\0\0\x20\0AV01", 12))
        return 2;
    size_t count = 0;
    for (size_t pos = 32; pos < input->size; ++count) {
        if (input->size - pos < 12)
            return 2;
        uint32_t size = AV_RL32(input->data + pos);
        pos += 12;
        if (size > input->size - pos)
            return 2;
        pos += size;
    }
    if (count > (SIZE_MAX - sizeof(ObuCase)) / sizeof(ObuTemporalUnit))
        return 2;
    ObuCase *source = malloc(sizeof(*source) + count * sizeof(*source->units));
    if (!source)
        return 2;
    source->single = !strcmp(input->argv[0], "single");
    source->count = count;
    size_t pos = 32;
    for (size_t i = 0; i < count; ++i) {
        uint32_t size = AV_RL32(input->data + pos);
        pos += 12;
        source->units[i] = (ObuTemporalUnit){input->data + pos, size};
        pos += size;
    }
    input->prepared = source;
    input->release = free;
    input->name = "av1-obu-reading";
    input->run = !strcmp(input->argv[2], "wire") ? obu_wire_run : obu_run;
    return 0;
}
