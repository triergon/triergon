/* Public FFmpeg text and attached-font views. */
#include <errno.h>
#include <iconv.h>
#include <libavutil/avstring.h>
#include <libavutil/error.h>
#include <libavutil/intreadwrite.h>

static void subtitle_view_row(ViewWork *work, uint64_t kind, const uint64_t *words,
                              size_t count, const uint8_t *payload, size_t size)
{
    view_record(work, kind, words, count, payload, size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, words[i]);
    if (size) {
        putchar(' ');
        for (size_t i = 0; i < size; ++i)
            printf("%02x", payload[i]);
    }
    putchar('\n');
}

#define SUBTITLE_ROW(work, kind, words, data, size) \
    subtitle_view_row(work, kind, words, sizeof(words) / sizeof(*(words)), data, size)

static void subtitle_view_refusal(ViewWork *work)
{
    subtitle_view_row(work, 8, NULL, 0, NULL, 0);
}

static int subtitle_view_tx3g(const ViewCase *input, ViewWork *work)
{
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_MOV_TEXT);
    AVCodecContext *context = avcodec_alloc_context3(codec);
    if (!codec || !context || input->size > INT_MAX) {
        avcodec_free_context(&context);
        return 2;
    }
    context->extradata = av_mallocz(input->size + AV_INPUT_BUFFER_PADDING_SIZE);
    if (!context->extradata) {
        avcodec_free_context(&context);
        return 2;
    }
    memcpy(context->extradata, input->data, input->size);
    context->extradata_size = (int)input->size;

    int status = avcodec_open2(context, codec, NULL);
    const char *line = status < 0 || !context->subtitle_header
        ? NULL : strstr((const char *)context->subtitle_header, "\nStyle: Default,");
    if (!line) {
        subtitle_view_refusal(work);
    } else {
        const char *font = line + strlen("\nStyle: Default,");
        const char *end = strchr(font, ',');
        unsigned size, primary, background, align;
        int bold, italic, underline;
        if (!end || sscanf(end + 1,
            "%u,&H%x,%*[^,],%*[^,],&H%x,%d,%d,%d,%*[^,],%*[^,],%*[^,],"
            "%*[^,],%*[^,],%*[^,],%*[^,],%*[^,],%u",
            &size, &primary, &background, &bold, &italic, &underline, &align) != 7) {
            avcodec_free_context(&context);
            return 2;
        }
        const size_t length = (size_t)(end - font);
        const uint64_t words[] = {size, primary, background, bold != 0, italic != 0,
                                  underline != 0, align, length};
        SUBTITLE_ROW(work, 3, words, (const uint8_t *)font, length);
    }
    avcodec_free_context(&context);
    return 0;
}

typedef struct {
    const uint8_t *data;
    size_t size, offset;
} SubtitleViewInput;

static int subtitle_view_read(void *opaque, uint8_t *buffer, int capacity)
{
    SubtitleViewInput *input = opaque;
    size_t count = input->size - input->offset;
    if (!count)
        return AVERROR_EOF;
    if (count > (size_t)capacity)
        count = capacity;
    memcpy(buffer, input->data + input->offset, count);
    input->offset += count;
    return (int)count;
}

static int64_t subtitle_view_seek(void *opaque, int64_t offset, int whence)
{
    SubtitleViewInput *input = opaque;
    if (whence == AVSEEK_SIZE)
        return (int64_t)input->size;
    int64_t base;
    switch (whence & ~AVSEEK_FORCE) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (int64_t)input->offset; break;
    case SEEK_END: base = (int64_t)input->size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > (int64_t)input->size - base)
        return AVERROR(EINVAL);
    input->offset = (size_t)(offset + base);
    return (int64_t)input->offset;
}

static AVIOContext *subtitle_view_io(SubtitleViewInput *input)
{
    const int buffer_size = 32 * 1024;
    uint8_t *buffer = av_malloc(buffer_size);
    if (!buffer)
        return NULL;
    AVIOContext *io = avio_alloc_context(buffer, buffer_size, 0, input,
                                        subtitle_view_read, NULL, subtitle_view_seek);
    if (!io)
        av_free(buffer);
    return io;
}

static void subtitle_view_close_io(AVIOContext **io)
{
    if (*io) {
        av_freep(&(*io)->buffer);
        avio_context_free(io);
    }
}

typedef struct {
    SubtitleViewInput source;
    AVIOContext *io;
    AVFormatContext *format;
} SubtitleViewDemux;

static int subtitle_view_open(SubtitleViewDemux *demux, const ViewCase *input,
                              const char *name, AVDictionary **options)
{
    demux->source = (SubtitleViewInput){input->data, input->size, 0};
    demux->io = subtitle_view_io(&demux->source);
    demux->format = avformat_alloc_context();
    if (!demux->io || !demux->format)
        return AVERROR(ENOMEM);
    demux->format->pb = demux->io;
    demux->format->flags |= AVFMT_FLAG_CUSTOM_IO;
    return avformat_open_input(&demux->format, name, NULL, options);
}

static void subtitle_view_close(SubtitleViewDemux *demux)
{
    avformat_close_input(&demux->format);
    subtitle_view_close_io(&demux->io);
}

static const char *subtitle_view_tag(const AVStream *stream, const char *key)
{
    const AVDictionaryEntry *tag = av_dict_get(stream->metadata, key, NULL, 0);
    return tag ? tag->value : "";
}

static int subtitle_view_font(const char *mime)
{
    static const char *const types[] = {
        "application/x-truetype-font", "application/x-font-ttf", "application/x-font-otf",
        "application/x-font-opentype", "application/vnd.ms-opentype",
        "font/ttf", "font/otf", "font/collection",
    };
    for (size_t i = 0; i < sizeof(types) / sizeof(*types); ++i)
        if (!strcmp(types[i], mime))
            return 1;
    return 0;
}

static int subtitle_view_context(const ViewCase *input, ViewWork *work)
{
    SubtitleViewDemux demux = {0};
    if (subtitle_view_open(&demux, input, "input.mkv", NULL) < 0) {
        subtitle_view_refusal(work);
        subtitle_view_close(&demux);
        return 0;
    }
    const AVStream *track = NULL;
    for (unsigned i = 0; i < demux.format->nb_streams; ++i) {
        const AVStream *stream = demux.format->streams[i];
        if (stream->codecpar->codec_id == AV_CODEC_ID_ASS ||
            stream->codecpar->codec_id == AV_CODEC_ID_SSA) {
            track = stream;
            break;
        }
    }
    if (!track) {
        subtitle_view_refusal(work);
    } else {
        const uint64_t head[] = {0, (uint64_t)track->codecpar->extradata_size};
        SUBTITLE_ROW(work, 2, head, track->codecpar->extradata, head[1]);
        for (unsigned i = 0; i < demux.format->nb_streams; ++i) {
            const AVStream *stream = demux.format->streams[i];
            if (stream->codecpar->codec_type != AVMEDIA_TYPE_ATTACHMENT ||
                !subtitle_view_font(subtitle_view_tag(stream, "mimetype")))
                continue;
            const char *name = subtitle_view_tag(stream, "filename");
            const uint64_t filename[] = {1, strlen(name)};
            SUBTITLE_ROW(work, 2, filename, (const uint8_t *)name, filename[1]);
            const uint64_t file[] = {2, (uint64_t)stream->codecpar->extradata_size};
            SUBTITLE_ROW(work, 2, file, stream->codecpar->extradata, file[1]);
        }
        subtitle_view_row(work, 7, NULL, 0, NULL, 0);
    }
    subtitle_view_close(&demux);
    return 0;
}

/* iconv supplies the failure byte offset and the bytes beyond an ASS NUL.
 * For successful nonempty text the decoder must also expose the same visible
 * prefix. This uses two public interfaces of the configured receiver build. */
static int subtitle_charset_decode(AVCodecContext *context, AVPacket *packet,
                                   const uint8_t *input, size_t length, AVSubtitle *output)
{
    av_packet_unref(packet);
    if (length > INT_MAX || av_new_packet(packet, (int)length) < 0)
        return AVERROR(ENOMEM);
    memcpy(packet->data, input, length);
    packet->pts = 0;
    packet->duration = 1000;
    int got = 0;
    int status = avcodec_decode_subtitle2(context, output, &got, packet);
    return status < 0 ? status : got && output->num_rects && output->rects[0]->ass ? 1 : 0;
}

static int subtitle_charset_stream(ViewWork *work, AVCodecContext *context, AVPacket *packet,
                                   iconv_t converter, const uint8_t *data, size_t size,
                                   uint64_t index)
{
    char *output = av_malloc(size * 4 + 16);
    if (!output)
        return 2;
    char *source = (char *)data;
    char *end = output;
    size_t unread = size, space = size * 4 + 16;
    (void)iconv(converter, NULL, NULL, NULL, NULL);
    if (iconv(converter, &source, &unread, &end, &space) == (size_t)-1) {
        if (errno != EINVAL && errno != EILSEQ) {
            av_free(output);
            return 2;
        }
        const uint64_t failed[] = {index, size - unread};
        SUBTITLE_ROW(work, 8, failed, NULL, 0);
    } else {
        AVSubtitle subtitle = {0};
        size_t length = (size_t)(end - output);
        const uint8_t *payload = (const uint8_t *)output;
        const char *nul = memchr(output, 0, length);
        const size_t visible = nul ? (size_t)(nul - output) : length;
        const int status = visible ? subtitle_charset_decode(context, packet, data, size, &subtitle) : 0;
        if (visible && status <= 0) {
            const uint64_t failed[] = {index, 0};
            SUBTITLE_ROW(work, 8, failed, NULL, 0);
        } else {
            if (status == 1) {
                const char *ass = subtitle.rects[0]->ass;
                const size_t actual = strlen(ass);
                if (actual != visible || memcmp(ass, output, visible)) {
                    payload = (const uint8_t *)ass;
                    length = actual;
                }
            }
            const uint64_t words[] = {index, length};
            SUBTITLE_ROW(work, 3, words, payload, length);
        }
        avsubtitle_free(&subtitle);
    }
    av_free(output);
    return 0;
}

static int subtitle_view_charset(const ViewCase *input, ViewWork *work)
{
    int packet_view = !strcmp(input->prefix, "charset-packets-");
    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_ASS);
    AVCodecContext *context = avcodec_alloc_context3(codec);
    AVPacket *packet = av_packet_alloc();
    int result = 0;
    if (!context || !codec || !packet) {
        result = 2;
        goto done;
    }
    context->sub_charenc = av_strdup(input->argv[1]);
    context->sub_charenc_mode = FF_SUB_CHARENC_MODE_PRE_DECODER;
    context->pkt_timebase = (AVRational){1, 1000};
    if (avcodec_open2(context, codec, NULL) < 0) {
        subtitle_view_refusal(work);
        goto done;
    }
    if (packet_view) {
        AVSubtitle subtitle = {0};
        if (subtitle_charset_decode(context, packet, input->data, input->size, &subtitle) == 1) {
            const char *ass = subtitle.rects[0]->ass;
            const uint64_t words[] = {strlen(ass)};
            SUBTITLE_ROW(work, 3, words, (const uint8_t *)ass, words[0]);
        } else {
            subtitle_view_refusal(work);
        }
        avsubtitle_free(&subtitle);
    } else {
        iconv_t converter = iconv_open("UTF-8", input->argv[1]);
        if (converter == (iconv_t)-1) {
            result = 2;
            goto done;
        }
        const int matrix = !strcmp(input->argv[2], "bytes") ? 1 :
                           !strcmp(input->argv[2], "pairs") ? 2 : 0;
        if (!matrix) {
            result = subtitle_charset_stream(work, context, packet, converter, input->data, input->size, 0);
        } else {
            const unsigned count = matrix == 1 ? 256 : 65536;
            for (unsigned index = 0; !result && index < count; ++index) {
                const uint8_t bytes[] = {index >> 8, index & 255};
                result = subtitle_charset_stream(work, context, packet, converter,
                                                 bytes + (2 - matrix), matrix, index);
            }
        }
        iconv_close(converter);
    }
done:
    av_packet_free(&packet);
    avcodec_free_context(&context);
    return result;
}

static int subtitle_view_utf8(const uint8_t *data, size_t size)
{
    if (!size)
        return 1;
    const uint8_t *end = data + size;
    int32_t scalar;
    while (data < end)
        if (av_utf8_decode(&scalar, &data, end, AV_UTF8_FLAG_ACCEPT_NON_CHARACTERS) < 0)
            return 0;
    return 1;
}

static int subtitle_view_cue(ViewWork *work, const AVStream *stream,
                             const AVPacket *packet, const char *mode, unsigned ordinal)
{
    const uint8_t *data = packet->data;
    size_t size = packet->size;
    const uint8_t *settings = NULL, *identifier = NULL;
    size_t settings_size = 0, identifier_size = 0, comments = 0;
    uint8_t *decoded = NULL;
    int valid = 1;
    if (!strcmp(mode, "tx3g")) {
        if (size < 2 || AV_RB16(data) > size - 2) {
            valid = 0;
        } else {
            size_t text_size = AV_RB16(data);
            const uint8_t *box = data + text_size + 2;
            const uint8_t *end = data + size;
            while (box < end) {
                size_t available = (size_t)(end - box);
                if (available < 8 || AV_RB32(box) < 8 || AV_RB32(box) > available) {
                    valid = 0;
                    break;
                }
                box += AV_RB32(box);
            }
            data += 2;
            size = text_size;
            if (valid && size >= 2 && AV_RB16(data) == 0xfeff) {
                iconv_t converter = iconv_open("UTF-8", "UTF-16BE");
                size_t space = size * 2, initial_space = space, unread = size - 2;
                decoded = av_malloc(space);
                if (converter == (iconv_t)-1 || !decoded) {
                    if (converter != (iconv_t)-1)
                        iconv_close(converter);
                    av_free(decoded);
                    return 2;
                }
                char *source = (char *)data + 2, *target = (char *)decoded;
                valid = iconv(converter, &source, &unread, &target, &space) != (size_t)-1;
                iconv_close(converter);
                data = decoded;
                size = initial_space - space;
            }
        }
    } else if (!strcmp(mode, "vtt")) {
        identifier = av_packet_get_side_data(packet, AV_PKT_DATA_WEBVTT_IDENTIFIER, &identifier_size);
        settings = av_packet_get_side_data(packet, AV_PKT_DATA_WEBVTT_SETTINGS, &settings_size);
        size_t extra_size = 0;
        const uint8_t *extra = av_packet_get_side_data(packet, AV_PKT_DATA_MATROSKA_BLOCKADDITIONAL,
                                                       &extra_size);
        if (extra && extra_size >= 8 && AV_RB64(extra) == 1) {
            extra += 8;
            extra_size -= 8;
            const uint8_t *first = memchr(extra, '\n', extra_size);
            const uint8_t *second = first ? memchr(first + 1, '\n',
                extra_size - (size_t)(first + 1 - extra)) : NULL;
            if (!second || !subtitle_view_utf8(extra, extra_size)) {
                valid = 0;
            } else {
                settings = extra;
                settings_size = (size_t)(first - extra);
                identifier = first + 1;
                identifier_size = (size_t)(second - identifier);
                comments = extra_size - (size_t)(second + 1 - extra);
            }
        }
    }
    if (valid && strcmp(mode, "ass"))
        valid = subtitle_view_utf8(data, size);
    if (!valid) {
        const uint64_t words[] = {ordinal};
        SUBTITLE_ROW(work, 8, words, NULL, 0);
    } else {
        const uint64_t words[] = {
            ordinal, stream->time_base.num, stream->time_base.den,
            packet->pts != AV_NOPTS_VALUE, packet->pts == AV_NOPTS_VALUE ? 0 : packet->pts,
            packet->duration, size, comments
        };
        SUBTITLE_ROW(work, 1, words, data, size);
        if (identifier_size) {
            const uint64_t id[] = {0, identifier_size};
            SUBTITLE_ROW(work, 3, id, identifier, identifier_size);
        }
        if (settings_size) {
            const uint64_t opts[] = {1, settings_size};
            SUBTITLE_ROW(work, 3, opts, settings, settings_size);
        }
    }
    av_free(decoded);
    return 0;
}

static int subtitle_view_extraction(const ViewCase *input, ViewWork *work)
{
    SubtitleViewDemux demux = {0};
    const char *mode = input->argv[1];
    const char *file = !strcmp(input->argv[0], "mov") ? "input.mp4" : "input.mkv";
    if (subtitle_view_open(&demux, input, file, NULL) < 0) {
        subtitle_view_refusal(work);
        subtitle_view_close(&demux);
        return 0;
    }
    enum AVCodecID match = AV_CODEC_ID_NONE;
    if (!strcmp(mode, "ass"))
        match = AV_CODEC_ID_ASS;
    else if (!strcmp(mode, "utf8"))
        match = AV_CODEC_ID_SUBRIP;
    else if (!strcmp(mode, "tx3g"))
        match = AV_CODEC_ID_MOV_TEXT;
    const AVStream *track = NULL;
    for (unsigned i = 0; i < demux.format->nb_streams; ++i) {
        const AVStream *stream = demux.format->streams[i];
        if (stream->codecpar->codec_type == AVMEDIA_TYPE_SUBTITLE &&
            (stream->codecpar->codec_id == match ||
             (match == AV_CODEC_ID_NONE && stream->codecpar->codec_id == AV_CODEC_ID_WEBVTT))) {
            track = stream;
            break;
        }
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        subtitle_view_close(&demux);
        return 2;
    }
    int result = 0, status = AVERROR_INVALIDDATA;
    unsigned ordinal = 0;
    while (track && !result && (status = av_read_frame(demux.format, packet)) >= 0) {
        if (packet->stream_index == track->index)
            result = subtitle_view_cue(work, track, packet, mode, ordinal++);
        av_packet_unref(packet);
    }
    if (status == AVERROR_EOF)
        subtitle_view_row(work, 7, NULL, 0, NULL, 0);
    else
        subtitle_view_refusal(work);
    av_packet_free(&packet);
    subtitle_view_close(&demux);
    return result;
}

static int subtitle_view_carriage(const ViewCase *input, ViewWork *work)
{
    SubtitleViewDemux demux = {0};
    const char *mode = input->argv[0];
    const char *name = !strcmp(mode, "srt") ? "input.srt" :
                       !strcmp(mode, "vtt") ? "input.vtt" :
                       !strcmp(mode, "mov") ? "input.mp4" : "input.mkv";
    int status = subtitle_view_open(&demux, input, name, NULL);
    const uint64_t open[] = {0, status};
    SUBTITLE_ROW(work, 3, open, NULL, 0);
    if (status < 0) {
        subtitle_view_close(&demux);
        return 0;
    }
    if (!strcmp(input->argv[1], "streaminfo")) {
        status = avformat_find_stream_info(demux.format, NULL);
        const uint64_t discover[] = {1, status};
        SUBTITLE_ROW(work, 3, discover, NULL, 0);
    }
    const AVStream *track = NULL;
    for (unsigned i = 0; i < demux.format->nb_streams; ++i) {
        const AVStream *stream = demux.format->streams[i];
        if (stream->codecpar->codec_type == AVMEDIA_TYPE_SUBTITLE) {
            track = stream;
            break;
        }
    }
    if (track) {
        const AVCodecParameters *p = track->codecpar;
        const uint64_t words[] = {p->codec_id, track->time_base.num, track->time_base.den,
                                  (uint64_t)p->extradata_size};
        SUBTITLE_ROW(work, 2, words, p->extradata, words[3]);
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        subtitle_view_close(&demux);
        return 2;
    }
    uint64_t ordinal = 0;
    while ((status = av_read_frame(demux.format, packet)) >= 0) {
        if (track && packet->stream_index == track->index) {
            const uint64_t words[] = {status, ordinal++, packet->pos, packet->pts, packet->dts,
                                      packet->duration, packet->flags, (uint64_t)packet->size};
            SUBTITLE_ROW(work, 1, words, packet->data, words[7]);
            for (int i = 0; i < packet->side_data_elems; ++i) {
                const AVPacketSideData *side = &packet->side_data[i];
                const uint64_t fields[] = {2, side->type, side->size};
                SUBTITLE_ROW(work, 3, fields, side->data, side->size);
            }
        }
        av_packet_unref(packet);
    }
    const uint64_t end[] = {status};
    SUBTITLE_ROW(work, 7, end, NULL, 0);
    av_packet_free(&packet);
    subtitle_view_close(&demux);
    return 0;
}

static int subtitle_views_text_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "tx3g-config-") && input->argc == 1 &&
        !strcmp(input->argv[0], "ass-header")) {
        input->name = "tx3g-config";
        input->run = subtitle_view_tx3g;
        return 0;
    }
    if (!strcmp(input->prefix, "renderer-context-") && input->argc == 0) {
        input->name = "renderer-context";
        input->run = subtitle_view_context;
        return 0;
    }
    if ((!strcmp(input->prefix, "charset-") || !strcmp(input->prefix, "charset-packets-")) &&
        input->argc == 3) {
        input->name = !strcmp(input->prefix, "charset-") ? "charset-streams" : "charset-packets";
        input->run = subtitle_view_charset;
        return 0;
    }
    if (!strcmp(input->prefix, "text-extract-") && input->argc == 2) {
        input->name = "text-extraction";
        input->run = subtitle_view_extraction;
        return 0;
    }
    if (!strcmp(input->prefix, "carriage-") && input->argc == 2) {
        input->name = "carriage-trace";
        input->run = subtitle_view_carriage;
        return 0;
    }
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
    return 2;
}
