/* Public DTS parser, core bitstream filter and decoder observations. */
#include <libavcodec/bsf.h>

typedef enum { DTS_CORE, DTS_PACKED, DTS_PARSER } DtsView;
typedef struct {
    DtsView view;
    size_t window, maximum;
    int decoded;
} DtsCase;

static void dts_terminal(ViewWork *work, uint64_t kind, const char *reason)
{
    view_record(work, kind, NULL, 0, NULL, 0);
    if (work->trace)
        printf("%" PRIu64 " %s\n", kind, reason);
}

static void dts_packet(ViewWork *work, const uint64_t *words, size_t count,
                       const uint8_t *data, size_t size)
{
    view_record(work, 1, words, count, data, size);
    if (work->trace) {
        fputs("1", stdout);
        for (size_t i = 0; i < count; ++i)
            printf(" %" PRIu64, words[i]);
        putchar('\n');
    }
}

static int dts_decode_view(AVCodecContext *decoder, AVFrame *frame,
                           AVPacket *packet, ViewWork *work)
{
    int result = avcodec_send_packet(decoder, packet);
    if (result >= 0)
        result = avcodec_receive_frame(decoder, frame);
    if (result < 0)
        return result;
    const uint64_t words[] = {
        frame->ch_layout.nb_channels,
        frame->ch_layout.order == AV_CHANNEL_ORDER_NATIVE ? frame->ch_layout.u.mask : 0,
        decoder->bit_rate, decoder->bits_per_raw_sample,
    };
    view_record(work, 3, words, 4, NULL, 0);
    if (work->trace)
        printf("3 %" PRIu64 " %" PRIu64 " %" PRIu64 " %" PRIu64 "\n",
               words[0], words[1], words[2], words[3]);
    return 0;
}

static int dts_frames_run(const ViewCase *input, ViewWork *work)
{
    const DtsCase *test = input->prepared;
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_DTS);
    AVCodecContext *context = avcodec_alloc_context3(NULL), *decoder = NULL;
    AVBSFContext *filter = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    uint8_t *window = av_malloc(test->window + AV_INPUT_BUFFER_PADDING_SIZE);
    int result = 2, status = 0, decoded = 0;

    if (!parser || !context || !window)
        goto done;
    context->codec_id = AV_CODEC_ID_DTS;
    if (test->view == DTS_CORE) {
        const AVBitStreamFilter *core = av_bsf_get_by_name("dca_core");
        packet = av_packet_alloc();
        if (!core || !packet || av_bsf_alloc(core, &filter) < 0)
            goto done;
        filter->par_in->codec_type = AVMEDIA_TYPE_AUDIO;
        filter->par_in->codec_id = AV_CODEC_ID_DTS;
        if (av_bsf_init(filter) < 0)
            goto done;
    }
    if (test->decoded) {
        const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_DTS);
        decoder = avcodec_alloc_context3(codec);
        frame = av_frame_alloc();
        if (!codec || !decoder || !frame || avcodec_open2(decoder, codec, NULL) < 0)
            goto done;
    }

    size_t at = 0, end = 0;
    for (;;) {
        if (at == end && at < input->size) {
            const size_t remaining = input->size - at;
            const size_t length = remaining < test->window ? remaining : test->window;
            memcpy(window, input->data + at, length);
            memset(window + length, 0, AV_INPUT_BUFFER_PADDING_SIZE);
            end = at + length;
        }
        const int flush = at == input->size;
        uint8_t *data = NULL;
        int size = 0;
        const int used = av_parser_parse2(parser, context, &data, &size,
                                          flush ? NULL : window + (at % test->window),
                                          (int)(end - at), AV_NOPTS_VALUE, AV_NOPTS_VALUE, at);
        if (used < 0 || (size == 0 && used == 0 && !flush)) {
            status = AVERROR_INVALIDDATA;
            break;
        }
        at += used;
        if (size > 0) {
            const size_t bytes = (size_t)size;
            const uint64_t start = (uint64_t)parser->frame_offset;
            if (test->view == DTS_CORE) {
                if (av_new_packet(packet, size) < 0)
                    goto done;
                memcpy(packet->data, data, bytes);
                status = av_bsf_send_packet(filter, packet);
                if (status >= 0)
                    status = av_bsf_receive_packet(filter, packet);
                if (status < 0)
                    break;
                const size_t core = (size_t)packet->size;
                if (test->decoded && !decoded) {
                    status = dts_decode_view(decoder, frame, packet, work);
                    if (status < 0)
                        break;
                    decoded = 1;
                }
                if (core > test->maximum) {
                    status = AVERROR_INVALIDDATA;
                    break;
                }
                const uint64_t words[] = {start, bytes, core, core, core,
                                          context->sample_rate, parser->duration};
                /* The parser bytes and the BSF bytes are both part of the comparison. */
                dts_packet(work, words, 7, data, bytes);
                const uint64_t filtered[] = {core};
                dts_packet(work, filtered, 1, packet->data, core);
                av_packet_unref(packet);
            } else if (test->view == DTS_PACKED) {
                const uint64_t words[] = {start, bytes, context->sample_rate, parser->duration};
                dts_packet(work, words, 4, data, bytes);
            } else {
                const uint64_t words[] = {
                    start, bytes, (uint64_t)(int64_t)parser->duration, context->sample_rate,
                    (uint64_t)(int64_t)context->profile, start, (uint64_t)parser->cur_offset,
                    (uint64_t)parser->next_frame_offset,
                };
                dts_packet(work, words, 8, data, bytes);
            }
        } else if (flush) {
            break;
        }
    }
    if (status == AVERROR(ENOMEM))
        goto done;
    dts_terminal(work, status < 0 ? 8 : 7, status < 0 ? "reference refused" : "end");
    result = 0;
done:
    av_free(window);
    av_frame_free(&frame);
    av_packet_free(&packet);
    av_bsf_free(&filter);
    avcodec_free_context(&decoder);
    avcodec_free_context(&context);
    if (parser)
        av_parser_close(parser);
    return result;
}

static size_t dts_number(const char *arg)
{
    if (!*arg || strspn(arg, "0123456789") != strlen(arg))
        return 0;
    const unsigned long value = strtoul(arg, NULL, 10);
    return value <= 256UL * 1024 * 1024 ? (size_t)value : 0;
}

static int dts_frames_prepare(ViewCase *input)
{
    DtsCase test = {0};
    const char *const *args = (const char *const *)input->argv;
    if (!strcmp(input->prefix, "dts-core-")) {
        if (input->argc != 3 || (strcmp(args[2], "frames") && strcmp(args[2], "decoded")))
            return 2;
        test.view = DTS_CORE;
        test.window = dts_number(args[0]);
        test.maximum = dts_number(args[1]);
        test.decoded = !strcmp(args[2], "decoded");
        input->name = "dts-core-frames";
        if (!test.maximum)
            return 2;
    } else if (!strcmp(input->prefix, "dts-packed-")) {
        if (input->argc != 3)
            return 2;
        test.view = DTS_PACKED;
        test.window = dts_number(args[2]);
        input->name = "dts-packed-frames";
    } else {
        if (input->argc != 2)
            return 2;
        test.view = DTS_PARSER;
        test.window = dts_number(args[1]);
        input->name = "dts-parser-trace";
    }
    if (!test.window)
        return 2;
    if (test.window > input->size)
        test.window = input->size ? input->size : 1;
    DtsCase *stored = malloc(sizeof(*stored));
    if (!stored)
        return 2;
    *stored = test;
    input->prepared = stored;
    input->release = free;
    input->run = dts_frames_run;
    return 0;
}
