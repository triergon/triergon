/* Read the current writer's output through the public MPEG-TS demuxer. */
#include <fcntl.h>
#include <limits.h>
#include <spawn.h>
#include <sys/wait.h>

extern char **environ;

typedef struct {
    uint8_t *data;
    size_t size;
} TransportMuxOutput;

static int transport_mux_emit(const ViewCase *input, FILE *pictures, FILE *ts)
{
#if defined(__linux__) && defined(__arm__)
    char binary[] = "target/thumbv7neon-unknown-linux-gnueabihf/release/transport-views-worker";
#else
    char binary[] = "target/release/transport-views-worker";
#endif
    char *arguments[14] = {binary, "ts-mux-fold", "/dev/fd/0", "--emit-ts", "/dev/fd/3"};
    for (int i = 0; i < input->argc; ++i)
        arguments[5 + i] = input->argv[i];

    posix_spawn_file_actions_t io;
    if (posix_spawn_file_actions_init(&io))
        return -1;
    pid_t child;
    int error = posix_spawn_file_actions_adddup2(&io, fileno(pictures), STDIN_FILENO) ||
                posix_spawn_file_actions_adddup2(&io, fileno(ts), 3) ||
                posix_spawn_file_actions_addopen(&io, STDOUT_FILENO, "/dev/null", O_WRONLY, 0) ||
                posix_spawn(&child, binary, &io, NULL, arguments, environ);
    posix_spawn_file_actions_destroy(&io);
    if (error)
        return -1;
    int status;
    pid_t result;
    do {
        result = waitpid(child, &status, 0);
    } while (result < 0 && errno == EINTR);
    return result == child && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

static int transport_mux_load(const ViewCase *input, TransportMuxOutput *output)
{
    FILE *pictures = tmpfile();
    FILE *ts = tmpfile();
    int result = -1;
    if (!pictures || !ts ||
        fwrite(input->data, 1, input->size, pictures) != input->size ||
        fflush(pictures) || fseek(pictures, 0, SEEK_SET) ||
        transport_mux_emit(input, pictures, ts) || fseek(ts, 0, SEEK_END))
        goto done;
    const long length = ftell(ts);
    if (length < 0 || length > 16L * 1024 * 1024 || fseek(ts, 0, SEEK_SET))
        goto done;
    if (length) {
        output->size = (size_t)length;
        output->data = malloc(output->size);
        if (!output->data || fread(output->data, 1, output->size, ts) != output->size)
            goto done;
    }
    result = 0;
done:
    if (pictures)
        fclose(pictures);
    if (ts)
        fclose(ts);
    return result;
}

static void transport_mux_release(void *opaque)
{
    TransportMuxOutput *output = opaque;
    free(output->data);
    free(output);
}

static void transport_mux_tag(ViewWork *work, const AVProgram *program,
                              const char *key, uint64_t field)
{
    const AVDictionaryEntry *entry = av_dict_get(program->metadata, key, NULL, 0);
    size_t len = entry ? strlen(entry->value) : 0;
    const uint64_t words[] = {field, entry != NULL, len};
    transport_view_row(work, 5, words, 3, entry ? (const uint8_t *)entry->value : NULL, len);
}

static void transport_mux_catalogue(ViewWork *work, const AVFormatContext *format)
{
    for (unsigned i = 0; i < format->nb_programs; ++i) {
        const AVProgram *program = format->programs[i];
        const uint64_t words[] = {0, (uint64_t)program->program_num, (uint64_t)program->pmt_pid};
        transport_view_row(work, 5, words, 3, NULL, 0);
        for (unsigned j = 0; j < program->nb_stream_indexes; ++j) {
            const AVStream *stream = format->streams[program->stream_index[j]];
            const uint64_t track[] = {1, (uint64_t)stream->id, stream->codecpar->codec_tag};
            transport_view_row(work, 5, track, 3, NULL, 0);
        }
        transport_mux_tag(work, program, "service_provider", 2);
        transport_mux_tag(work, program, "service_name", 3);
    }
}

/* No candidate stream exists for a rejected configuration. Ask the reference
 * muxer to admit the same options and verify its output independently. */
static int transport_mux_config(const ViewCase *input, ViewWork *work)
{
    if (input->argc != 8 || input->size < 21)
        return 2;
    static const char *const keys[] = {
        "mpegts_transport_stream_id", "mpegts_original_network_id",
        "mpegts_service_id", "mpegts_pmt_start_pid", "mpegts_start_pid",
    };
    const int avc = !strcmp(input->argv[5], "27");
    if (!avc && strcmp(input->argv[5], "1") && strcmp(input->argv[5], "2"))
        return 2;
    AVFormatContext *format = NULL;
    AVPacket *packet = NULL;
    AVDictionary *options = NULL;
    uint8_t *bytes = NULL;
    int result = 2;
    if (avformat_alloc_output_context2(&format, NULL, "mpegts", NULL) < 0 ||
        avio_open_dyn_buf(&format->pb) < 0)
        goto done;
    for (size_t i = 0; i < sizeof(keys) / sizeof(*keys); ++i)
        if (av_dict_set(&options, keys[i], input->argv[i], 0) < 0)
            goto done;
    if (av_dict_set(&format->metadata, "service_provider", input->argv[6], 0) < 0 ||
        av_dict_set(&format->metadata, "service_name", input->argv[7], 0) < 0)
        goto done;
    AVStream *stream = avformat_new_stream(format, NULL);
    if (!stream)
        goto done;
    stream->time_base = (AVRational){1, 90000};
    stream->codecpar->codec_type = AVMEDIA_TYPE_VIDEO;
    stream->codecpar->codec_id = avc ? AV_CODEC_ID_H264
        : input->argv[5][0] == '1' ? AV_CODEC_ID_MPEG1VIDEO : AV_CODEC_ID_MPEG2VIDEO;
    stream->codecpar->width = 16;
    stream->codecpar->height = 16;
    int status = avformat_write_header(format, &options);
    if (status < 0) {
        int extent = avio_close_dyn_buf(format->pb, &bytes);
        format->pb = NULL;
        const int name_overflow = strlen(input->argv[6]) > UINT8_MAX ||
                                  strlen(input->argv[7]) > UINT8_MAX;
        if (extent == 0 && (status == AVERROR(ERANGE) || status == AVERROR(EINVAL) ||
                            (name_overflow && status == AVERROR(ENOMEM)))) {
            transport_view_row(work, 8, NULL, 0, NULL, 0);
            result = 0;
        }
        goto done;
    }
    if (av_dict_count(options))
        goto done;
    /* A short Annex B AUD and opaque IDR payload suffice for TS mux admission.
     * The configuration operation does not decode the elementary stream. */
    static const uint8_t avc_picture[] = {0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x65, 0x88, 0x80};
    size_t len = ((size_t)input->data[17] << 24) | ((size_t)input->data[18] << 16) |
                 ((size_t)input->data[19] << 8) | input->data[20];
    if (!len || len > input->size - 21 || len > INT_MAX)
        goto done;
    const uint8_t *picture = avc ? avc_picture : input->data + 21;
    if (avc)
        len = sizeof(avc_picture);
    packet = av_packet_alloc();
    if (!packet || av_new_packet(packet, (int)len) < 0)
        goto done;
    memcpy(packet->data, picture, len);
    packet->pts = 3000;
    packet->dts = 0;
    packet->flags = AV_PKT_FLAG_KEY;
    if (av_write_frame(format, packet) < 0 || av_write_trailer(format) < 0)
        goto done;
    int extent = avio_close_dyn_buf(format->pb, &bytes);
    format->pb = NULL;
    if (extent < 188 || extent % 188)
        goto done;
    for (int offset = 0; offset < extent; offset += 188)
        if (bytes[offset] != 0x47)
            goto done;
    transport_view_row(work, 9, NULL, 0, NULL, 0);
    result = 0;
done:
    av_packet_free(&packet);
    av_dict_free(&options);
    if (format && format->pb) {
        avio_close_dyn_buf(format->pb, &bytes);
        format->pb = NULL;
    }
    av_free(bytes);
    avformat_free_context(format);
    return result;
}

static int transport_mux_random_access(const TransportMuxOutput *output,
                                       int64_t position, int pid)
{
    if (position < 0 || (uint64_t)position >= output->size ||
        position % 188 || output->size - (size_t)position < 188)
        return -1;
    const uint8_t *ts = output->data + position;
    if (ts[0] != 0x47 || !(ts[1] & 0x40) ||
        ((ts[1] & 0x1f) * 256 + ts[2]) != pid || !(ts[3] & 0x10))
        return -1;
    if ((ts[3] & 0x20) && ts[4] > 182)
        return -1;
    return (ts[3] & 0x20) && ts[4] && (ts[5] & 0x40);
}

static int transport_mux_read(const ViewCase *input, ViewWork *work)
{
    const TransportMuxOutput *output = input->prepared;
    if (!output->size)
        return transport_mux_config(input, work);
    const ViewCase ts = {.data = output->data, .size = output->size};
    TransportViewInput source;
    int status = transport_view_open(&source, &ts, 4096, "mpegts");
    AVPacket *packet = av_packet_alloc();
    if (status >= 0 && !packet)
        status = AVERROR(ENOMEM);
    if (status >= 0) {
        while ((status = av_read_frame(source.format, packet)) >= 0) {
            const AVStream *stream = source.format->streams[packet->stream_index];
            int random_access = transport_mux_random_access(output, packet->pos, stream->id);
            if (random_access < 0) {
                status = AVERROR_INVALIDDATA;
                break;
            }
            unsigned flags = (unsigned)packet->flags & ~(unsigned)AV_PKT_FLAG_KEY;
            flags |= (unsigned)random_access;
            const uint64_t words[] = {
                (uint64_t)stream->id,
                packet->pts != AV_NOPTS_VALUE, packet->pts == AV_NOPTS_VALUE ? 0 : (uint64_t)packet->pts,
                packet->dts != AV_NOPTS_VALUE, packet->dts == AV_NOPTS_VALUE ? 0 : (uint64_t)packet->dts,
                flags, (uint64_t)packet->pos, (uint64_t)packet->size,
            };
            transport_view_row(work, 1, words, 8, packet->data, (size_t)packet->size);
            av_packet_unref(packet);
        }
    }
    if (status == AVERROR_EOF) {
        transport_mux_catalogue(work, source.format);
        transport_view_row(work, 7, NULL, 0, NULL, 0);
    }
    av_packet_free(&packet);
    transport_view_close(&source);
    return status == AVERROR_EOF ? 0 : 2;
}

static int transport_mux_prepare(ViewCase *input)
{
    if (input->argc != 8 && (input->argc != 1 || strcmp(input->argv[0], "default")))
        return 2;
    TransportMuxOutput *output = calloc(1, sizeof(*output));
    if (!output)
        return 2;
    if (transport_mux_load(input, output)) {
        transport_mux_release(output);
        fputs("cannot obtain MPEG-TS output from transport-views-worker\n", stderr);
        return 2;
    }
    input->prepared = output;
    input->release = transport_mux_release;
    input->name = "ts-mux-readback";
    input->run = transport_mux_read;
    return 0;
}
