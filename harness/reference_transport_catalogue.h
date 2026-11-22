/* Program and stream catalogue exposed by libavformat after draining MPEG-TS. */
#include <errno.h>
#include <libavutil/dict.h>

typedef struct {
    const uint8_t *data;
    size_t size, offset, window;
    AVIOContext *io;
    AVFormatContext *format;
} TransportViewInput;

static int transport_view_read(void *opaque, uint8_t *buffer, int count)
{
    TransportViewInput *source = opaque;
    size_t bytes = source->size - source->offset;
    if (!bytes)
        return AVERROR_EOF;
    if (bytes > (size_t)count)
        bytes = (size_t)count;
    if (bytes > source->window)
        bytes = source->window;
    memcpy(buffer, source->data + source->offset, bytes);
    source->offset += bytes;
    return (int)bytes;
}

static int64_t transport_view_seek(void *opaque, int64_t offset, int whence)
{
    TransportViewInput *source = opaque;
    if (whence == AVSEEK_SIZE)
        return (int64_t)source->size;
    int64_t base;
    switch (whence & ~AVSEEK_FORCE) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (int64_t)source->offset; break;
    case SEEK_END: base = (int64_t)source->size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > (int64_t)source->size - base)
        return AVERROR(EINVAL);
    source->offset = (size_t)(base + offset);
    return (int64_t)source->offset;
}

static void transport_view_close(TransportViewInput *source)
{
    avformat_close_input(&source->format);
    if (source->io)
        av_freep(&source->io->buffer);
    avio_context_free(&source->io);
}

static int transport_view_open(TransportViewInput *source, const ViewCase *input,
                               size_t window, const char *name)
{
    *source = (TransportViewInput){.data = input->data, .size = input->size, .window = window};
    const AVInputFormat *demuxer = av_find_input_format(name);
    if (!demuxer)
        return AVERROR_DEMUXER_NOT_FOUND;
    uint8_t *buffer = av_malloc(4096);
    if (!buffer)
        return AVERROR(ENOMEM);
    source->io = avio_alloc_context(buffer, 4096, 0, source, transport_view_read, NULL,
                                   transport_view_seek);
    if (!source->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    source->format = avformat_alloc_context();
    if (!source->format)
        return AVERROR(ENOMEM);
    source->format->pb = source->io;
    source->format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    return avformat_open_input(&source->format, NULL, demuxer, NULL);
}

static void transport_view_row(ViewWork *work, uint64_t kind, const uint64_t *words,
                               size_t count, const uint8_t *payload, size_t bytes)
{
    view_record(work, kind, words, count, payload, bytes);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i != count; ++i)
        printf(" %" PRIu64, words[i]);
    if (bytes) {
        putchar(' ');
        for (size_t i = 0; i != bytes; ++i)
            printf("%02x", payload[i]);
    }
    putchar('\n');
}

static void transport_catalogue_block(ViewWork *work, uint64_t field, const char *data, size_t len)
{
    const uint64_t words[] = {field, len};
    transport_view_row(work, 5, words, 2, (const uint8_t *)data, len);
}

static void transport_catalogue_stream(ViewWork *work, unsigned program, const AVStream *stream)
{
    const AVCodecParameters *codec = stream->codecpar;
    uint64_t hearing = !!(stream->disposition & AV_DISPOSITION_HEARING_IMPAIRED);
    uint64_t visual = !!(stream->disposition & AV_DISPOSITION_VISUAL_IMPAIRED);
    const uint64_t words[] = {1, program, (uint64_t)stream->id, codec->codec_tag, hearing, visual};
    transport_view_row(work, 5, words, 6, NULL, 0);
    const char *name = avcodec_get_name(codec->codec_id);
    if (codec->codec_id == AV_CODEC_ID_MP1 || codec->codec_id == AV_CODEC_ID_MP2 ||
        codec->codec_id == AV_CODEC_ID_MP3)
        name = "mpeg-audio";
    const AVDictionaryEntry *lang = av_dict_get(stream->metadata, "language", NULL, 0);
    transport_catalogue_block(work, 2, name, strlen(name));
    transport_catalogue_block(work, 3, lang ? lang->value : NULL, lang ? strlen(lang->value) : 0);
    if (codec->codec_id == AV_CODEC_ID_DVB_SUBTITLE)
        transport_catalogue_block(work, 4, (const char *)codec->extradata, (size_t)codec->extradata_size);
}

static int transport_catalogue_run(const ViewCase *input, ViewWork *work)
{
    TransportViewInput source;
    int status = transport_view_open(&source, input, 4096, "mpegts");
    AVPacket *packet = NULL;
    if (status >= 0) {
        packet = av_packet_alloc();
        if (!packet)
            status = AVERROR(ENOMEM);
        else {
            while ((status = av_read_frame(source.format, packet)) >= 0)
                av_packet_unref(packet);
        }
    }
    if (status == AVERROR_EOF && source.format) {
        const AVFormatContext *format = source.format;
        for (unsigned i = 0; i != format->nb_programs; ++i) {
            const AVProgram *program = format->programs[i];
            const uint64_t words[] = {0, (uint64_t)program->program_num, (uint64_t)program->pmt_pid,
                                      (uint64_t)program->pcr_pid, (uint64_t)program->pmt_version};
            transport_view_row(work, 5, words, 5, NULL, 0);
            for (unsigned j = 0; j != program->nb_stream_indexes; ++j)
                transport_catalogue_stream(work, (unsigned)program->program_num,
                                           format->streams[program->stream_index[j]]);
        }
        for (unsigned index = 0; index != format->nb_streams; ++index) {
            int referenced = 0;
            for (unsigned i = 0; i != format->nb_programs; ++i) {
                const AVProgram *program = format->programs[i];
                for (unsigned j = 0; j != program->nb_stream_indexes; ++j)
                    referenced |= program->stream_index[j] == index;
            }
            if (!referenced)
                transport_catalogue_stream(work, 0, format->streams[index]);
        }
        transport_view_row(work, 7, NULL, 0, NULL, 0);
    } else {
        transport_view_row(work, 8, NULL, 0, NULL, 0);
        fprintf(stderr, "catalogue reference stopped with %d\n", status);
    }
    av_packet_free(&packet);
    transport_view_close(&source);
    return status == AVERROR(ENOMEM) || status == AVERROR_DEMUXER_NOT_FOUND ? 2 : 0;
}

static int transport_catalogue_prepare(ViewCase *input)
{
    if (input->argc != 4 || !strcmp(input->prefix, "ts-video-descriptors-"))
        return 2;
    input->name = !strcmp(input->prefix, "ts-psi-") ? "ts-psi-readers" : "ts-catalogue";
    input->run = transport_catalogue_run;
    return 0;
}
