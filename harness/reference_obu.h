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

/* Interpret the filter's returned wire bytes using the AV1 OBU header layout. */
static int obu_output(ViewWork *work, uint64_t index,
                      const uint8_t *data, size_t size)
{
    uint64_t count = 0;
    while (size) {
        uint8_t header = *data++;
        --size;
        int has_extension = (header & 4) != 0, sized = (header & 2) != 0;
        if (has_extension && !size)
            return AVERROR_INVALIDDATA;
        uint8_t extension = 0;
        if (has_extension) {
            extension = *data++;
            --size;
        }
        size_t length = size;
        if (sized && obu_leb(&data, &size, &length) < 0)
            return AVERROR_INVALIDDATA;
        const uint64_t words[] = {
            index, (header >> 3) & 15, has_extension, extension >> 5,
            (extension >> 3) & 3, sized, length
        };
        stream_record(work, 1, words, 7, data, length);
        ++count;
        data += length;
        size -= length;
    }
    const uint64_t end[] = {index, count};
    stream_record(work, 7, end, 2, NULL, 0);
    return 0;
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
        int status = AVERROR_INVALIDDATA;
        if (unit->size) {
            if (av_new_packet(packet, (int)unit->size) < 0)
                goto done;
            memcpy(packet->data, unit->data, unit->size);
            status = av_bsf_send_packet(filter, packet);
            if (status >= 0)
                status = av_bsf_receive_packet(filter, packet);
        }
        if (status == AVERROR(ENOMEM))
            goto done;
        if (status >= 0)
            status = obu_output(work, i, packet->data, (size_t)packet->size);
        if (status < 0) {
            if (work->trace)
                fprintf(stderr, "OBU unit %zu: %d\n", i, status);
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
        (strcmp(input->argv[2], "0") && strcmp(input->argv[2], "14")) ||
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
    input->run = obu_run;
    return 0;
}
