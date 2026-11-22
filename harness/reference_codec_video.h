/* FFmpeg's packet parser, shared by the codec views. */
#include <errno.h>
#include <libavutil/mem.h>

typedef int (*CodecPacketView)(AVCodecContext *, const AVCodecParserContext *,
                               const uint8_t *, size_t, void *);

typedef struct {
    size_t window;
    uint8_t bytes[];
} CodecParserInput;

static int codec_parser_prepare(ViewCase *input, size_t window)
{
    if (!window)
        return 2;
    const size_t windows = input->size / window + (input->size % window != 0);
    CodecParserInput *prepared = calloc(1, sizeof(*prepared) + input->size +
                                         windows * AV_INPUT_BUFFER_PADDING_SIZE);
    if (!prepared)
        return 2;
    prepared->window = window;
    for (size_t at = 0, pad = 0; at < input->size; pad += AV_INPUT_BUFFER_PADDING_SIZE) {
        const size_t count = input->size - at < window ? input->size - at : window;
        memcpy(prepared->bytes + at + pad, input->data + at, count);
        at += count;
    }
    input->prepared = prepared;
    input->release = free;
    return 0;
}

static int codec_parse_packets(const ViewCase *input, AVCodecContext *context,
                               enum AVCodecID id, CodecPacketView packet_view, void *opaque)
{
    const CodecParserInput *prepared = input->prepared;
    AVCodecParserContext *parser = av_parser_init(id);
    if (!parser)
        return 2;
    int status = 2;
    for (size_t base = 0, pad = 0; base < input->size; pad += AV_INPUT_BUFFER_PADDING_SIZE) {
        const size_t available = input->size - base;
        const size_t count = available < prepared->window ? available : prepared->window;
        const uint8_t *buffer = prepared->bytes + base + pad;
        size_t at = 0;
        unsigned unconsumed = 0;
        while (at < count) {
            uint8_t *packet;
            int size;
            const int used = av_parser_parse2(parser, context, &packet, &size, buffer + at,
                                              (int)(count - at), AV_NOPTS_VALUE, AV_NOPTS_VALUE,
                                              (int64_t)(base + at));
            if (used < 0 || (size_t)used > count - at || (!used && (!size || ++unconsumed > 16)))
                goto done;
            if (used)
                unconsumed = 0;
            if (size && packet_view(context, parser, packet, (size_t)size, opaque))
                goto done;
            at += (size_t)used;
        }
        base += count;
    }

    uint8_t *packet;
    int size;
    if (av_parser_parse2(parser, context, &packet, &size, NULL, 0,
                         AV_NOPTS_VALUE, AV_NOPTS_VALUE, (int64_t)input->size) < 0)
        goto done;
    if (size && packet_view(context, parser, packet, (size_t)size, opaque))
        goto done;
    status = 0;

done:
    av_parser_close(parser);
    return status;
}

static int codec_hevc_unit(AVCodecContext *context, const AVCodecParserContext *parser,
                           const uint8_t *data, size_t size, void *opaque)
{
    (void)context;
    (void)parser;
    ViewWork *work = opaque;
    const uint64_t words[] = {size};
    view_record(work, 1, words, 1, data, size);
    if (work->trace) {
        uint64_t digest = 0;
        fold_bytes(&digest, data, size);
        printf("1 %zu %" PRIu64 "\n", size, digest);
    }
    return 0;
}

static int codec_hevc_au_run(const ViewCase *input, ViewWork *work)
{
    AVCodecContext *context = avcodec_alloc_context3(avcodec_find_decoder(AV_CODEC_ID_HEVC));
    if (!context)
        return 2;
    const int status = codec_parse_packets(input, context, AV_CODEC_ID_HEVC, codec_hevc_unit, work);
    avcodec_free_context(&context);
    return status;
}


#include <libavutil/imgutils.h>
#include <libavutil/md5.h>
#include <libavutil/pixdesc.h>

typedef struct {
    AVPacket *packet;
    AVFrame *frame;
    struct AVMD5 *md5;
    ViewWork *work;
    int64_t pts;
    int properties;
    int refused;
} CodecVideo;

static int codec_video_frame(CodecVideo *state, enum AVCodecID id)
{
    AVFrame *frame = state->frame;
    const AVPixFmtDescriptor *format = av_pix_fmt_desc_get(frame->format);
    int strides[4];
    if (!format || av_image_fill_linesizes(strides, frame->format, frame->width) < 0)
        return 2;
    uint8_t digest[4 * 16];
    size_t planes = 0;
    for (unsigned i = 0; i < 4 && strides[i]; ++i) {
        const int chroma = (i == 1 || i == 2) && !(format->flags & AV_PIX_FMT_FLAG_RGB);
        const unsigned shift = chroma ? format->log2_chroma_h : 0;
        const int height = (frame->height + (1 << shift) - 1) >> shift;
        if (!frame->data[i])
            return 2;
        av_md5_init(state->md5);
        for (int y = 0; y < height; ++y)
            av_md5_update(state->md5, frame->data[i] + (ptrdiff_t)y * frame->linesize[i],
                          (size_t)strides[i]);
        av_md5_final(state->md5, digest + 16 * planes++);
    }
    uint64_t words[12] = {(uint64_t)frame->width, (uint64_t)frame->height,
                          (uint64_t)format->comp[0].depth, planes};
    size_t count = 4;
    if (state->properties) {
        int num = frame->sample_aspect_ratio.num, den = frame->sample_aspect_ratio.den;
        if (num <= 0 || den <= 0)
            num = den = 0;
        else
            av_reduce(&num, &den, num, den, INT_MAX);
        words[count++] = (uint64_t)num;
        words[count++] = (uint64_t)den;
        words[count++] = (uint64_t)frame->color_primaries;
        words[count++] = (uint64_t)frame->color_trc;
        words[count++] = (uint64_t)frame->colorspace;
        words[count++] = id == AV_CODEC_ID_H264 ? (uint64_t)(frame->color_range == AVCOL_RANGE_JPEG)
                                                : (uint64_t)frame->color_range;
        words[count++] = id == AV_CODEC_ID_H264 && (frame->flags & AV_FRAME_FLAG_INTERLACED);
    } else {
        words[count++] = frame->pts != AV_NOPTS_VALUE;
        if (frame->pts != AV_NOPTS_VALUE)
            words[count++] = (uint64_t)frame->pts;
    }
    words[count++] = 16 * planes;
    const unsigned kind = state->properties ? 4 : 1;
    view_record(state->work, kind, words, count, digest, 16 * planes);
    if (state->work->trace) {
        printf("%u", kind);
        for (size_t i = 0; i < count; ++i)
            printf(" %" PRIu64, words[i]);
        putchar(' ');
        for (size_t i = 0; i < 16 * planes; ++i)
            printf("%02x", digest[i]);
        putchar('\n');
    }
    return 0;
}

static int codec_video_receive(AVCodecContext *context, CodecVideo *state)
{
    for (;;) {
        const int status = avcodec_receive_frame(context, state->frame);
        if (status == AVERROR_EOF || status == AVERROR(EAGAIN))
            return 0;
        if (status == AVERROR_INVALIDDATA) {
            state->refused = 1;
            return 0;
        }
        if (status < 0)
            return 2;
        if (codec_video_frame(state, context->codec_id))
            return 2;
        av_frame_unref(state->frame);
    }
}

static int codec_video_packet(AVCodecContext *context, const AVCodecParserContext *parser,
                              const uint8_t *data, size_t size, void *opaque)
{
    (void)parser;
    CodecVideo *state = opaque;
    state->packet->data = (uint8_t *)data;
    state->packet->size = (int)size;
    state->packet->pts = state->pts++;
    const int status = avcodec_send_packet(context, state->packet);
    if (status == AVERROR_INVALIDDATA)
        state->refused = 1;
    else if (status < 0)
        return 2;
    return codec_video_receive(context, state);
}

static int codec_video_decode(const ViewCase *input, ViewWork *work)
{
    const enum AVCodecID id = !strcmp(input->argv[0], "h264") ? AV_CODEC_ID_H264 : AV_CODEC_ID_HEVC;
    const AVCodec *codec = avcodec_find_decoder(id);
    AVCodecContext *context = avcodec_alloc_context3(codec);
    CodecVideo state = {.packet = av_packet_alloc(), .frame = av_frame_alloc(),
                        .md5 = av_md5_alloc(), .work = work, .properties = input->argc == 1};
    int status = 2;
    if (!codec || !context || !state.packet || !state.frame || !state.md5)
        goto done;
    context->thread_count = 1;
    if (avcodec_open2(context, codec, NULL) < 0)
        goto done;
    status = codec_parse_packets(input, context, id, codec_video_packet, &state);
    if (status == 0) {
        const int end = avcodec_send_packet(context, NULL);
        if (end < 0 && end != AVERROR_EOF && end != AVERROR_INVALIDDATA)
            status = 2;
        else
            status = codec_video_receive(context, &state);
    }
    if (!status && state.refused && !work->packets) {
        view_record(work, 8, NULL, 0, NULL, 0);
        if (work->trace)
            puts("8");
    }

done:
    av_free(state.md5);
    av_frame_free(&state.frame);
    av_packet_free(&state.packet);
    avcodec_free_context(&context);
    return status;
}


#include <libavcodec/bsf.h>
#include <stdarg.h>

/* Field tags are the order of the public StreamFeatures members. Set rows
 * contain a single value, except coded dimensions, which are a pair. */
enum {
    H264_PROFILE, H264_CONSTRAINTS, H264_CHROMA, H264_DEPTH, H264_SIZE,
    H264_CABAC, H264_FRAME_ONLY, H264_MBAFF_SPS, H264_FIELD, H264_MBAFF_PIC,
    H264_SLICE, H264_FMO, H264_REDUNDANT_PPS, H264_REDUNDANT_SLICE, H264_EXPLICIT,
    H264_IMPLICIT, H264_TRANSFORM, H264_SCALING, H264_POC, H264_CONSTRAINED,
    H264_SPATIAL, H264_TEMPORAL, H264_MODIFIED, H264_MMCO, H264_LONG_TERM,
    H264_MULTI_SLICE, H264_NAL, H264_GAPS, H264_LOSSLESS, H264_CROP,
    H264_MAX_SLICES, H264_RESTRICTION, H264_ERRORS, H264_FIELDS,
};

typedef struct {
    unsigned first, second;
} H264Value;

typedef struct {
    H264Value *items;
    size_t count, capacity;
} H264Values;

typedef struct {
    H264Values rows[H264_FIELDS];
    unsigned present[H264_FIELDS];
    unsigned sps_id, pps_id, pps_sps[256], sps_mbaff[32], pps_weighted_bipred[256];
    unsigned profile, constraints, chroma, luma, chroma_depth, width, height, frame_only;
    unsigned mbaff, reorder, buffering, restriction;
    unsigned slice_type, field, first_mb, direct, slices, max_slices;
    unsigned bipred, sps_from_pps;
    enum { SYNTAX_OTHER, SYNTAX_SPS, SYNTAX_PPS, SYNTAX_SLICE } unit;
    int failed;
} H264Syntax;

static H264Syntax *codec_syntax_log;

static void h264_add(H264Syntax *syntax, unsigned field, unsigned first, unsigned second)
{
    H264Values *set = &syntax->rows[field];
    for (size_t i = 0; i < set->count; ++i)
        if (set->items[i].first == first && set->items[i].second == second)
            return;
    if (set->count == set->capacity) {
        size_t capacity = set->capacity ? 2 * set->capacity : 8;
        H264Value *items = av_realloc_array(set->items, capacity, sizeof(*items));
        if (!items) {
            syntax->failed = 1;
            return;
        }
        set->items = items;
        set->capacity = capacity;
    }
    set->items[set->count++] = (H264Value){first, second};
}

static void h264_finish_syntax(H264Syntax *s)
{
    if (s->unit == SYNTAX_SPS) {
        h264_add(s, H264_PROFILE, s->profile, 0);
        h264_add(s, H264_CONSTRAINTS, s->constraints, 0);
        h264_add(s, H264_CHROMA, s->chroma, 0);
        h264_add(s, H264_DEPTH, 8 + (s->luma > s->chroma_depth ? s->luma : s->chroma_depth), 0);
        h264_add(s, H264_SIZE, 16 * (s->width + 1), 16 * (s->height + 1) * (2 - s->frame_only));
        h264_add(s, H264_FRAME_ONLY, s->frame_only, 0);
        s->present[H264_MBAFF_SPS] |= s->mbaff;
        if (s->sps_id < 32)
            s->sps_mbaff[s->sps_id] = s->mbaff;
        if (s->restriction) {
            s->rows[H264_RESTRICTION].count = 0;
            h264_add(s, H264_RESTRICTION, s->reorder, s->buffering);
        }
    } else if (s->unit == SYNTAX_PPS && s->pps_id < 256) {
        s->pps_sps[s->pps_id] = s->sps_from_pps;
        s->pps_weighted_bipred[s->pps_id] = s->bipred;
    } else if (s->unit == SYNTAX_SLICE && s->slice_type < 10 && s->pps_id < 256) {
        const unsigned slice = s->slice_type % 5;
        /* P, B, I, SP, SI -> B, I, P, SI, SP */
        const unsigned names[] = {2, 0, 1, 4, 3};
        h264_add(s, H264_SLICE, names[slice], 0);
        s->present[H264_FIELD] |= s->field;
        const unsigned sps_id = s->pps_sps[s->pps_id];
        if (sps_id < 32)
            s->present[H264_MBAFF_PIC] |= !s->field && s->sps_mbaff[sps_id];
        if (slice == 1) {
            s->present[s->direct ? H264_SPATIAL : H264_TEMPORAL] = 1;
            s->present[H264_IMPLICIT] |= s->pps_weighted_bipred[s->pps_id] == 2;
        }
        if (!s->first_mb)
            s->slices = 0;
        ++s->slices;
        if (s->slices > s->max_slices)
            s->max_slices = s->slices;
    }
    s->unit = SYNTAX_OTHER;
}

static void h264_trace_value(H264Syntax *s, const char *name, unsigned value)
{
    if (!strcmp(name, "nal_unit_type")) {
        h264_add(s, H264_NAL, value, 0);
        return;
    }
#define FIELD(KEY, TARGET) if (!strcmp(name, KEY)) { TARGET = value; return; }
#define FLAG(KEY, TAG) if (!strcmp(name, KEY)) { s->present[TAG] |= value != 0; return; }
    if (s->unit == SYNTAX_SPS) {
        if (strlen(name) == strlen("constraint_set0_flag") &&
            !strncmp(name, "constraint_set", 14) && name[14] >= '0' && name[14] <= '5') {
            s->constraints |= value << (7 - (name[14] - '0'));
            return;
        }
        FIELD("seq_parameter_set_id", s->sps_id)
        FIELD("profile_idc", s->profile)
        FIELD("chroma_format_idc", s->chroma)
        FIELD("bit_depth_luma_minus8", s->luma)
        FIELD("bit_depth_chroma_minus8", s->chroma_depth)
        FIELD("pic_width_in_mbs_minus1", s->width)
        FIELD("pic_height_in_map_units_minus1", s->height)
        FIELD("frame_mbs_only_flag", s->frame_only)
        FIELD("mb_adaptive_frame_field_flag", s->mbaff)
        FIELD("bitstream_restriction_flag", s->restriction)
        FIELD("max_num_reorder_frames", s->reorder)
        FIELD("max_dec_frame_buffering", s->buffering)
        FLAG("seq_scaling_matrix_present_flag", H264_SCALING)
        FLAG("gaps_in_frame_num_allowed_flag", H264_GAPS)
        FLAG("qpprime_y_zero_transform_bypass_flag", H264_LOSSLESS)
        if (!strncmp(name, "frame_crop_", 11))
            s->present[H264_CROP] |= value != 0;
        if (!strcmp(name, "pic_order_cnt_type"))
            h264_add(s, H264_POC, value, 0);
    } else if (s->unit == SYNTAX_PPS) {
        FIELD("pic_parameter_set_id", s->pps_id)
        FIELD("seq_parameter_set_id", s->sps_from_pps)
        FIELD("weighted_bipred_idc", s->bipred)
        FLAG("num_slice_groups_minus1", H264_FMO)
        FLAG("redundant_pic_cnt_present_flag", H264_REDUNDANT_PPS)
        FLAG("transform_8x8_mode_flag", H264_TRANSFORM)
        FLAG("pic_scaling_matrix_present_flag", H264_SCALING)
        FLAG("constrained_intra_pred_flag", H264_CONSTRAINED)
        if (!strcmp(name, "entropy_coding_mode_flag"))
            h264_add(s, H264_CABAC, value, 0);
    } else if (s->unit == SYNTAX_SLICE) {
        FIELD("pic_parameter_set_id", s->pps_id)
        FIELD("slice_type", s->slice_type)
        FIELD("field_pic_flag", s->field)
        FIELD("first_mb_in_slice", s->first_mb)
        FIELD("direct_spatial_mv_pred_flag", s->direct)
        FLAG("redundant_pic_cnt", H264_REDUNDANT_SLICE)
        FLAG("ref_pic_list_modification_flag_l0", H264_MODIFIED)
        FLAG("ref_pic_list_modification_flag_l1", H264_MODIFIED)
        FLAG("long_term_reference_flag", H264_LONG_TERM)
        if (!strcmp(name, "luma_log2_weight_denom"))
            s->present[H264_EXPLICIT] = 1;
        if (!strncmp(name, "memory_management_control_operation", 35) && value)
            h264_add(s, H264_MMCO, value, 0);
    }
#undef FLAG
#undef FIELD
}

static void h264_log(void *context, int level, const char *format, va_list args)
{
    (void)context;
    H264Syntax *s = codec_syntax_log;
    if (!s || level > AV_LOG_TRACE)
        return;
    if (level <= AV_LOG_ERROR) {
        s->present[H264_ERRORS] = 1;
        return;
    }
    char line[1024];
    vsnprintf(line, sizeof(line), format, args);
    if (!strcmp(line, "Sequence Parameter Set\n")) {
        h264_finish_syntax(s);
        s->unit = SYNTAX_SPS;
        s->profile = s->constraints = s->luma = s->chroma_depth = 0;
        s->width = s->height = s->frame_only = s->mbaff = 0;
        s->restriction = s->reorder = s->buffering = 0;
        s->chroma = 1;
    } else if (!strcmp(line, "Picture Parameter Set\n")) {
        h264_finish_syntax(s);
        s->unit = SYNTAX_PPS;
        s->bipred = s->sps_from_pps = s->pps_id = 0;
    } else if (!strcmp(line, "Slice Header\n")) {
        h264_finish_syntax(s);
        s->unit = SYNTAX_SLICE;
        s->first_mb = s->field = s->direct = 0;
        s->slice_type = s->pps_id = UINT_MAX;
    } else {
        unsigned bit, value;
        char name[128], spelling[257];
        if (sscanf(line, "%u %127s %256s = %u", &bit, name, spelling, &value) == 4)
            h264_trace_value(s, name, value);
        else if (!strncmp(line, "Packet:", 7) || !strncmp(line, "Supplemental", 12) ||
                 !strncmp(line, "Access Unit Delimiter", 21))
            h264_finish_syntax(s);
    }
}

static int h264_order(const void *left, const void *right)
{
    const H264Value *a = left, *b = right;
    if (a->first != b->first)
        return a->first < b->first ? -1 : 1;
    return (a->second > b->second) - (a->second < b->second);
}

static void h264_emit(H264Syntax *s, ViewWork *work)
{
    s->present[H264_MULTI_SLICE] = s->max_slices > 1;
    s->present[H264_MAX_SLICES] = s->max_slices;
    for (unsigned tag = 0; tag < H264_FIELDS; ++tag) {
        H264Values *values = &s->rows[tag];
        switch (tag) {
        case H264_PROFILE: case H264_CONSTRAINTS: case H264_CHROMA: case H264_DEPTH:
        case H264_SIZE: case H264_CABAC: case H264_FRAME_ONLY: case H264_SLICE: case H264_POC:
        case H264_MMCO: case H264_NAL: case H264_RESTRICTION:
            if (values->count > 1)
                qsort(values->items, values->count, sizeof(*values->items), h264_order);
            break;
        default:
            h264_add(s, tag, s->present[tag], 0);
            break;
        }
        uint64_t words[] = {tag, values->count};
        view_record(work, 3, words, 2, NULL, 0);
        if (work->trace)
            printf("3 %u %zu\n", tag, values->count);
        for (size_t i = 0; i < values->count; ++i) {
            const uint64_t row[] = {tag, values->items[i].first, values->items[i].second};
            view_record(work, 3, row, 3, NULL, 0);
            if (work->trace)
                printf("3 %u %u %u\n", tag, values->items[i].first, values->items[i].second);
        }
    }
}

static int codec_h264_inspect(const ViewCase *input, ViewWork *work)
{
    ReaderInput reader = {0};
    H264Syntax syntax = {0};
    AVBSFContext *bsf = NULL;
    AVPacket *packet = av_packet_alloc();
    const AVBitStreamFilter *filter = av_bsf_get_by_name("trace_headers");
    const int log_level = av_log_get_level();
    int status = 2;
    if (!packet || !filter || reader_open(&reader, input, "h264") < 0 ||
        av_bsf_alloc(filter, &bsf) < 0)
        goto done;
    AVStream *stream = reader.format->streams[0];
    if (avcodec_parameters_copy(bsf->par_in, stream->codecpar) < 0)
        goto done;
    bsf->time_base_in = stream->time_base;
    codec_syntax_log = &syntax;
    av_log_set_level(AV_LOG_TRACE);
    av_log_set_callback(h264_log);
    const int init = av_bsf_init(bsf);
    if (init < 0 && init != AVERROR_INVALIDDATA)
        goto done;
    if (init < 0)
        syntax.present[H264_ERRORS] = 1;
    else {
        int read;
        while ((read = av_read_frame(reader.format, packet)) >= 0 && !syntax.failed) {
            int filter_status = av_bsf_send_packet(bsf, packet);
            if (filter_status >= 0)
                filter_status = av_bsf_receive_packet(bsf, packet);
            h264_finish_syntax(&syntax);
            av_packet_unref(packet);
            if (filter_status == AVERROR(ENOMEM))
                goto done;
            if (filter_status < 0)
                syntax.present[H264_ERRORS] = 1;
        }
        if (read < 0 && read != AVERROR_EOF)
            syntax.present[H264_ERRORS] = 1;
    }
    h264_finish_syntax(&syntax);
    h264_emit(&syntax, work);
    status = syntax.failed ? 2 : 0;

done:
    av_log_set_callback(av_log_default_callback);
    av_log_set_level(log_level);
    codec_syntax_log = NULL;
    for (unsigned i = 0; i < H264_FIELDS; ++i)
        av_free(syntax.rows[i].items);
    av_packet_free(&packet);
    av_bsf_free(&bsf);
    reader_close(&reader);
    return status;
}

static int codec_video_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "hevc-access-units-") && input->argc == 1 &&
        (!strcmp(input->argv[0], "whole") || !strcmp(input->argv[0], "4096"))) {
        input->name = "hevc-access-units";
        input->run = codec_hevc_au_run;
        return codec_parser_prepare(input, !strcmp(input->argv[0], "whole") ? input->size + 1 : 4096);
    }
    const int properties = !strcmp(input->prefix, "picture-props-");
    const int nal = !strcmp(input->prefix, "video-nal-");
    if ((properties && input->argc == 1) || (nal && input->argc == 2)) {
        const int h264 = !strcmp(input->argv[0], "h264");
        if ((!h264 && strcmp(input->argv[0], "hevc")) ||
            (nal && strcmp(input->argv[1], "nal") &&
             (!h264 || strcmp(input->argv[1], "nals-split"))))
            return 2;
        input->name = properties ? "picture-properties" : "video-nal-input";
        input->run = codec_video_decode;
        return codec_parser_prepare(input, input->size + 1);
    }
    if (!strcmp(input->prefix, "h264-inspect-") && !input->argc) {
        input->name = "h264-stream-inspection";
        input->run = codec_h264_inspect;
        return 0;
    }
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
    return 2;
}
