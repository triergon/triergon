/* NAL framing and configuration through public FFmpeg bitstream filters. */
#include <errno.h>
#include <stdarg.h>
#include <stdlib.h>
#include <libavutil/log.h>
#include <libavcodec/bsf.h>
#include <libavutil/opt.h>
#include <libavutil/mem.h>

typedef struct {
    const uint8_t *data;
    size_t size;
} NalView;

typedef struct {
    enum AVCodecID codec;
    NalView config, packets[64];
    unsigned packet_count, width;
} NalScript;

static int nal_take(NalView *input, size_t count, NalView *part)
{
    if (count > input->size)
        return -1;
    *part = (NalView){input->data, count};
    input->data += count;
    input->size -= count;
    return 0;
}

static uint32_t nal_be(const uint8_t *p, unsigned count)
{
    uint32_t result = 0;
    for (unsigned i = 0; i < count; ++i)
        result = (result << 8) | p[i];
    return result;
}

static enum AVCodecID nal_codec(const char *name)
{
    if (!strcmp(name, "avc"))
        return AV_CODEC_ID_H264;
    if (!strcmp(name, "hevc"))
        return AV_CODEC_ID_HEVC;
    return AV_CODEC_ID_NONE;
}

static int nal_script_load(NalView input, NalScript *script)
{
    NalView part;
    if (nal_take(&input, 13, &part) || memcmp(part.data, "SMBSF001", 8))
        return -1;
    switch (part.data[8]) {
    case 1: script->codec = AV_CODEC_ID_H264; break;
    case 2: script->codec = AV_CODEC_ID_HEVC; break;
    default: return -1;
    }
    unsigned width_at = script->codec == AV_CODEC_ID_H264 ? 4 : 21;
    if (nal_take(&input, nal_be(part.data + 9, 4), &script->config) ||
        script->config.size <= width_at || nal_take(&input, 4, &part))
        return -1;
    script->width = (script->config.data[width_at] & 3) + 1;
    unsigned events = nal_be(part.data, 4);
    if (events > 64)
        return -1;
    for (unsigned i = 0; i < events; ++i) {
        if (nal_take(&input, 1, &part) || part.data[0] > 2)
            return -1;
        if (part.data[0] != 0)
            continue;
        if (nal_take(&input, 48, &part))
            return -1;
        uint32_t size = nal_be(part.data + 40, 4), extra = nal_be(part.data + 44, 4);
        if (nal_take(&input, size, &script->packets[script->packet_count++]) ||
            (extra != UINT32_MAX && nal_take(&input, extra, &part)))
            return -1;
    }
    return input.size ? -1 : 0;
}

static void nal_record(ViewWork *work, uint64_t kind, const uint64_t *words,
                       size_t count, NalView payload)
{
    view_record(work, kind, words, count, payload.data, payload.size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, words[i]);
    if (payload.size) {
        putchar(' ');
        for (size_t i = 0; i < payload.size; ++i)
            printf("%02x", payload.data[i]);
    }
    putchar('\n');
}

static void nal_refusal(ViewWork *work, const uint64_t *words, size_t count, int status)
{
    if (work->trace)
        printf("# FFmpeg return %d\n", status);
    nal_record(work, 8, words, count, (NalView){0});
}

static const char *nal_bsf_name(enum AVCodecID codec)
{
    return codec == AV_CODEC_ID_H264 ? "h264_mp4toannexb" : "hevc_mp4toannexb";
}

static int nal_bsf_open(const char *name, enum AVCodecID codec, NalView config, AVBSFContext **bsf)
{
    const AVBitStreamFilter *filter = av_bsf_get_by_name(name);
    if (!filter)
        return AVERROR(ENOSYS);
    int result = av_bsf_alloc(filter, bsf);
    if (result < 0)
        return result;
    (*bsf)->par_in->codec_id = codec;
    (*bsf)->par_in->codec_type = AVMEDIA_TYPE_VIDEO;
    (*bsf)->time_base_in = (AVRational){1, 90000};
    if (config.size) {
        (*bsf)->par_in->extradata = av_mallocz(config.size + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!(*bsf)->par_in->extradata)
            return AVERROR(ENOMEM);
        memcpy((*bsf)->par_in->extradata, config.data, config.size);
        (*bsf)->par_in->extradata_size = (int)config.size;
    }
    return av_bsf_init(*bsf);
}

static int nal_send(AVBSFContext *bsf, AVPacket *packet, NalView sample)
{
    int result = av_new_packet(packet, (int)sample.size);
    if (result < 0)
        return result;
    if (sample.size)
        memcpy(packet->data, sample.data, sample.size);
    result = av_bsf_send_packet(bsf, packet);
    av_packet_unref(packet);
    return result;
}

static size_t nal_start(NalView bytes, int four, size_t *length)
{
    for (size_t i = four ? 3 : 2; i < bytes.size; ++i) {
        const uint8_t *p = bytes.data + i;
        if (p[0] != 1 || p[-1] || p[-2])
            continue;
        *length = i >= 3 && !p[-3] ? 4 : 3;
        if (!four || *length == 4)
            return i + 1 - *length;
    }
    return bytes.size;
}

/* Return each delimited unit. A prefix followed by another prefix or EOF has
 * an empty unit, which is distinct from an empty result. */
static int nal_annexb_next(NalView *bytes, int four, NalView *unit)
{
    if (!bytes->size)
        return 0;
    size_t prefix;
    size_t start = nal_start(*bytes, four, &prefix);
    if (start != 0)
        return -1;
    bytes->data += prefix;
    bytes->size -= prefix;
    size_t end = nal_start(*bytes, four, &prefix);
    return nal_take(bytes, end, unit) ? -1 : 1;
}

static unsigned nal_type(enum AVCodecID codec, NalView unit)
{
    if (!unit.size)
        return codec == AV_CODEC_ID_H264 ? 32 : 64;
    return codec == AV_CODEC_ID_H264 ? unit.data[0] & 31 : (unit.data[0] >> 1) & 63;
}

static int nal_sample_unit(const NalScript *script, AVPacket *packet, NalView framed)
{
    AVBSFContext *filter = NULL;
    int status = nal_bsf_open(nal_bsf_name(script->codec), script->codec,
                              script->config, &filter);
    if (status >= 0)
        status = nal_send(filter, packet, framed);
    if (status >= 0)
        status = av_bsf_receive_packet(filter, packet);
    av_bsf_free(&filter);
    return status;
}

static int nal_units_run(const ViewCase *input, ViewWork *work)
{
    const NalScript *script = input->prepared;
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return 2;

    int result = 0;
    for (unsigned sample = 0; sample < script->packet_count; ++sample) {
        const NalView source = script->packets[sample];
        NalView remaining = source;
        uint64_t index = 0, sample_word = sample;
        int status = 0;
        while (remaining.size) {
            /* Submit one declared unit at a time. The sample's physical length
             * fields delimit the input even if opaque bytes contain 00 00 01.
             * A fresh filter cannot inject another unit from earlier samples. */
            NalView length, payload;
            const uint8_t *start = remaining.data;
            if (nal_take(&remaining, script->width, &length) ||
                nal_take(&remaining, nal_be(length.data, script->width), &payload)) {
                status = AVERROR_INVALIDDATA;
                break;
            }
            NalView framed = {start, script->width + payload.size};
            if (!payload.size && remaining.size) {
                /* The converter gets the entire invalid suffix. Do not reinterpret
                 * its bytes as separately convertible NAL units after a zero length. */
                framed.size += remaining.size;
                remaining.size = 0;
            }
            status = nal_sample_unit(script, packet, framed);
            if (status == AVERROR(EAGAIN) && !payload.size)
                continue;
            if (status < 0)
                break;
            NalView converted = {packet->data, (size_t)packet->size};
            if (converted.size) {
                size_t prefix;
                if (nal_start(converted, 0, &prefix) != 0) {
                    const uint64_t size = converted.size;
                    nal_record(work, 3, &size, 1, converted);
                } else {
                    NalView unit = {converted.data + prefix, converted.size - prefix};
                    unsigned first = 0, second = 0;
                    if (unit.size && script->codec == AV_CODEC_ID_H264)
                        first = (unit.data[0] >> 5) & 3;
                    if (unit.size >= 2 && script->codec == AV_CODEC_ID_HEVC) {
                        first = ((unit.data[0] & 1) << 5) | (unit.data[1] >> 3);
                        second = unit.data[1] & 7;
                    }
                    const uint64_t offset = (uint64_t)(payload.data - source.data);
                    const uint64_t words[] = {sample, index++, offset, nal_type(script->codec, unit),
                                              first, second, unit.size};
                    nal_record(work, 1, words, 7, unit);
                }
            }
            av_packet_unref(packet);
        }
        if (status == AVERROR_INVALIDDATA) {
            nal_refusal(work, &sample_word, 1, status);
        } else if (status == 0 || status == AVERROR(EAGAIN)) {
            const uint64_t words[] = {sample, index};
            nal_record(work, 7, words, 2, (NalView){0});
        } else {
            result = 2;
            break;
        }
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    return result;
}

static void nal_config_units(ViewWork *work, enum AVCodecID codec, NalView bytes, int four,
                             int extracted, int decoder)
{
    size_t count = 0;
    NalView unit;
    int state;
    while ((state = nal_annexb_next(&bytes, four, &unit)) > 0) {
        // H.264 clause B.2 and H.265 clause B.3 place 00 00 00 at a NAL
        // boundary. Preserve one or two terminal zero bytes at EOF.
        if (extracted) {
            size_t end = unit.size;
            while (end && !unit.data[end - 1])
                --end;
            if (bytes.size || unit.size - end >= 3)
                unit.size = end;
        }
        uint64_t words[] = {nal_type(codec, unit), unit.size};
        nal_record(work, 1, words, 2, unit);
        ++count;
    }
    if (state < 0) {
        uint64_t length = bytes.size;
        nal_record(work, 3, &length, 1, bytes);
    }
    uint64_t end[] = {count, 0};
    nal_record(work, 7, end, decoder ? 2 : 1, (NalView){0});
}

static int nal_declared_extents_fit(const char *layout, size_t size)
{
    // Columns after the type and completeness hold only byte offsets.
    unsigned column = 0;
    while (*layout) {
        const char *end = layout + strcspn(layout, ",;");
        if (column >= 2) {
            char *parsed;
            const unsigned long offset = strtoul(layout, &parsed, 10);
            if (parsed != end || parsed == layout || offset > size)
                return 0;
        }
        column = *end == ';' ? 0 : column + 1;
        layout = *end ? end + 1 : end;
    }
    return 1;
}

static int nal_layout_field(const char **layout, size_t maximum, size_t *value)
{
    const char *start = *layout;
    const size_t digits = strspn(start, "0123456789");
    if (!digits || (start[digits] != ',' && start[digits] != ';'))
        return 0;
    for (*value = 0; start < *layout + digits; ++start) {
        const size_t digit = (size_t)(*start - '0');
        if (digit > maximum || *value > (maximum - digit) / 10)
            return 0;
        *value = *value * 10 + digit;
    }
    *layout = start + 1;
    return 1;
}

/* Independently apply the caller's array and length-field ranges. The fixed
 * unit headers follow H.264 §7.3.1 and H.265 §7.3.1.2. */
static int nal_layout_rows(ViewWork *work, enum AVCodecID codec, NalView source,
                           const char *layout, size_t sample_width)
{
    const int hevc = codec == AV_CODEC_ID_HEVC;
    size_t previous_array_end = 0, arrays = 0, count = 0;
    const uint64_t width_word = sample_width;
    if (work)
        nal_record(work, 6, &width_word, 1, (NalView){0});
    while (*layout) {
        size_t declared_type, completeness, start, end;
        if (++arrays > 256 ||
            !nal_layout_field(&layout, hevc ? 63 : 31, &declared_type) || layout[-1] != ',')
            return 0;
        if (layout[0] == '-' && layout[1] == ',') {
            completeness = 2;
            layout += 2;
        } else if (!nal_layout_field(&layout, 1, &completeness) || layout[-1] != ',') {
            return 0;
        }
        if (!nal_layout_field(&layout, source.size, &start) || layout[-1] != ',' ||
            !nal_layout_field(&layout, source.size, &end) || start < previous_array_end || end < start)
            return 0;
        const uint64_t array_words[] = {declared_type, completeness, start, end};
        if (work)
            nal_record(work, 4, array_words, 4, (NalView){source.data + start, end - start});
        size_t previous_nal_end = start;
        while (layout[-1] == ',') {
            size_t length_start, payload, nal_end;
            if (++count > 4096 ||
                !nal_layout_field(&layout, end, &length_start) || layout[-1] != ',' ||
                !nal_layout_field(&layout, end, &payload) || layout[-1] != ',' ||
                !nal_layout_field(&layout, end, &nal_end) ||
                length_start < previous_nal_end || payload <= length_start ||
                payload - length_start > 4 || nal_end < payload ||
                nal_end - payload < (size_t)(hevc ? 2 : 1))
                return 0;
            NalView unit = {source.data + payload, nal_end - payload};
            const unsigned width = (unsigned)(payload - length_start);
            if (nal_be(source.data + length_start, width) != unit.size ||
                (unit.data[0] & 0x80) || (hevc && !(unit.data[1] & 7)))
                return 0;
            const unsigned type = nal_type(codec, unit);
            if (work) {
                const uint64_t range_words[] = {length_start, payload, nal_end, type != declared_type};
                const uint64_t unit_words[] = {type, unit.size};
                nal_record(work, 5, range_words, 4, (NalView){source.data + length_start, width});
                nal_record(work, 1, unit_words, 2, unit);
            }
            previous_nal_end = nal_end;
        }
        previous_array_end = end;
    }
    const uint64_t end_word = count;
    if (work)
        nal_record(work, 7, &end_word, 1, (NalView){0});
    return 1;
}

static int nal_layout_run(const ViewCase *input, ViewWork *work)
{
    const enum AVCodecID codec = nal_codec(input->argv[1]);
    const NalView source = {input->data, input->size};
    const char *layout = input->argv[3];
    const size_t width = (size_t)(input->argv[2][0] - '0');
    const uint64_t size = source.size;
    if (size && size <= (1U << 20)) {
        nal_record(work, 2, &size, 1, source);
        if (nal_layout_rows(NULL, codec, source, layout, width))
            return nal_layout_rows(work, codec, source, layout, width) ? 0 : 2;
    }
    nal_refusal(work, NULL, 0, AVERROR_INVALIDDATA);
    return 0;
}

/* Extraction requires a decoder configuration. An isolated VPS can instead
 * be observed through the record writer without asking the reference to
 * interpret its RBSP or supplying other parameter sets. */
static int nal_single_vps(NalView source, AVBSFContext **conversion)
{
    NalView vps;
    if (nal_annexb_next(&source, 0, &vps) != 1 || source.size ||
        vps.size < 2 || vps.size > UINT16_MAX || nal_type(AV_CODEC_ID_HEVC, vps) != 32)
        return AVERROR_INVALIDDATA;
    const uint8_t prefix[] = {
        1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 30, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8,
        0, 0, 0x0f, 1, 32, 0, 1, (uint8_t)(vps.size >> 8), (uint8_t)vps.size,
    };
    uint8_t *record = av_malloc(sizeof(prefix) + vps.size);
    if (!record)
        return AVERROR(ENOMEM);
    memcpy(record, prefix, sizeof(prefix));
    memcpy(record + sizeof(prefix), vps.data, vps.size);
    const int status = nal_bsf_open(nal_bsf_name(AV_CODEC_ID_HEVC), AV_CODEC_ID_HEVC,
                                   (NalView){record, sizeof(prefix) + vps.size}, conversion);
    av_free(record);
    return status;
}

static int nal_config_run(const ViewCase *input, ViewWork *work)
{
    enum AVCodecID codec = nal_codec(input->argv[1]);
    NalView source = {input->data, input->size}, converted = {0};
    int declared = !strcmp(input->argv[0], "declared");
    AVBSFContext *bsf = NULL;
    AVPacket *packet = NULL;
    int status;
    if (declared) {
        if (source.size) {
            uint64_t size = source.size;
            nal_record(work, 2, &size, 1, source);
        }
        status = nal_declared_extents_fit(input->argv[3], source.size)
                 ? nal_bsf_open(nal_bsf_name(codec), codec, source, &bsf)
                 : AVERROR_INVALIDDATA;
        if (status >= 0) {
            converted = (NalView){bsf->par_out->extradata, (size_t)bsf->par_out->extradata_size};
            if (!source.size && !converted.size)
                status = AVERROR_INVALIDDATA;
            else if (converted.size == source.size &&
                     !memcmp(converted.data, source.data, source.size))
                converted = (NalView){0};
        }
    } else {
        status = nal_bsf_open("extract_extradata", codec, (NalView){0}, &bsf);
        if (status >= 0)
            status = av_opt_set_int(bsf->priv_data, "remove", 1, 0);
        if (status >= 0) {
            packet = av_packet_alloc();
            status = packet ? nal_send(bsf, packet, source) : AVERROR(ENOMEM);
        }
        if (status >= 0)
            status = av_bsf_receive_packet(bsf, packet);
        if (status >= 0) {
            converted.data = av_packet_get_side_data(packet, AV_PKT_DATA_NEW_EXTRADATA, &converted.size);
            /* A parameter-set configuration consumes the input. The filter
             * publishes any other NALs in its remaining packet. */
            NalView remaining = {packet->data, (size_t)packet->size};
            size_t prefix;
            const size_t first = nal_start(remaining, 0, &prefix);
            if (first < remaining.size && first + prefix < remaining.size) {
                status = AVERROR_INVALIDDATA;
            }
        }
        if (codec == AV_CODEC_ID_HEVC &&
            (status == AVERROR_INVALIDDATA || status == AVERROR(EAGAIN) ||
             (status >= 0 && !converted.size))) {
            av_bsf_free(&bsf);
            status = nal_single_vps(source, &bsf);
            if (status >= 0)
                converted = (NalView){bsf->par_out->extradata, (size_t)bsf->par_out->extradata_size};
        }
    }
    int refusal = status == AVERROR_INVALIDDATA ||
        (!declared && (status == AVERROR(EAGAIN) || (status >= 0 && !converted.size)));
    if (refusal)
        nal_refusal(work, NULL, 0, status);
    else if (status >= 0)
        nal_config_units(work, codec, converted, declared && codec == AV_CODEC_ID_HEVC, !declared, 0);
    av_packet_free(&packet);
    av_bsf_free(&bsf);
    return refusal || status >= 0 ? 0 : 2;
}

/* The filter publicly logs the rejected array type with its return code.
 * A single worker handles one operation, so this callback needs no shared state. */
static int nal_rejected_array;

static void nal_log_array(void *context, int level, const char *format, va_list args)
{
    (void)context;
    if (level > AV_LOG_ERROR)
        return;
    char line[128];
    int type;
    if (vsnprintf(line, sizeof(line), format, args) > 0 &&
        sscanf(line, "Invalid NAL unit type in extradata: %d", &type) == 1 &&
        type >= 0 && type <= 63)
        nal_rejected_array = type;
}

static int hvcc_annexb_run(const ViewCase *input, ViewWork *work)
{
    NalView source = {input->data, input->size};
    int decoder = !strcmp(input->argv[0], "decoder");
    AVBSFContext *bsf = NULL;
    nal_rejected_array = -1;
    int status = nal_bsf_open(nal_bsf_name(AV_CODEC_ID_HEVC), AV_CODEC_ID_HEVC, source, &bsf);
    if (status == AVERROR_INVALIDDATA) {
        int type = decoder ? nal_rejected_array : -1;
        uint64_t word = type;
        nal_refusal(work, &word, type >= 0, status);
    } else if (status >= 0) {
        NalView converted = {bsf->par_out->extradata, (size_t)bsf->par_out->extradata_size};
        /* Successful filter initialization can leave its extradata untouched
         * (including an absent record). Only a conversion is hvcC writer
         * output. Neither an empty nor a nonempty passthrough converted bytes. */
        if (converted.size == source.size &&
            (!source.size || !memcmp(converted.data, source.data, source.size)))
            nal_refusal(work, NULL, 0, AVERROR_INVALIDDATA);
        else
            nal_config_units(work, AV_CODEC_ID_HEVC, converted, 1, 0, decoder);
    }
    av_bsf_free(&bsf);
    return status == AVERROR_INVALIDDATA || status >= 0 ? 0 : 2;
}

static int nal_units_prepare(ViewCase *input)
{
    av_log_set_level(AV_LOG_QUIET);
    if (!strcmp(input->prefix, "nal-units-")) {
        if (input->argc != 2 || !nal_codec(input->argv[0]) ||
            (strcmp(input->argv[1], "1") && strcmp(input->argv[1], "7") && strcmp(input->argv[1], "4096")))
            return 2;
        NalScript *script = calloc(1, sizeof(*script));
        input->prepared = script;
        input->release = free;
        if (!script || nal_script_load((NalView){input->data, input->size}, script) ||
            script->codec != nal_codec(input->argv[0]))
            return 2;
        input->name = "nal-units";
        input->run = nal_units_run;
    } else if (!strcmp(input->prefix, "nal-config-")) {
        const int layout = input->argc == 4 && !strcmp(input->argv[0], "layout");
        if ((input->argc != 2 || strcmp(input->argv[0], "annexb")) &&
            (input->argc != 4 || (strcmp(input->argv[0], "declared") && !layout)))
            return 2;
        if (!nal_codec(input->argv[1]))
            return 2;
        if (layout && (strspn(input->argv[2], "1234") != 1 || strlen(input->argv[2]) != 1))
            return 2;
        input->name = "nal-config";
        input->run = layout ? nal_layout_run : nal_config_run;
    } else {
        if (input->argc != 1 || (strcmp(input->argv[0], "all") && strcmp(input->argv[0], "decoder")))
            return 2;
        if (!strcmp(input->argv[0], "decoder")) {
            av_log_set_level(AV_LOG_ERROR);
            av_log_set_callback(nal_log_array);
        }
        input->name = "hvcc-annexb";
        input->run = hvcc_annexb_run;
    }
    return 0;
}
