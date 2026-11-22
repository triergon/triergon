/* Read the current writer's output through the public MPEG-TS demuxer. */
#include <fcntl.h>
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

static int transport_mux_read(const ViewCase *input, ViewWork *work)
{
    const TransportMuxOutput *output = input->prepared;
    // A refused mux configuration supplies no TS and so no reference records.
    if (!output->size)
        return 0;
    const ViewCase ts = {.data = output->data, .size = output->size};
    TransportViewInput source;
    int status = transport_view_open(&source, &ts, 4096, "mpegts");
    AVPacket *packet = av_packet_alloc();
    if (status >= 0 && !packet)
        status = AVERROR(ENOMEM);
    if (status >= 0) {
        while ((status = av_read_frame(source.format, packet)) >= 0) {
            const AVStream *stream = source.format->streams[packet->stream_index];
            const uint64_t words[] = {
                (uint64_t)stream->id,
                packet->pts != AV_NOPTS_VALUE, packet->pts == AV_NOPTS_VALUE ? 0 : (uint64_t)packet->pts,
                packet->dts != AV_NOPTS_VALUE, packet->dts == AV_NOPTS_VALUE ? 0 : (uint64_t)packet->dts,
                (uint64_t)packet->flags, (uint64_t)packet->pos, (uint64_t)packet->size,
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
