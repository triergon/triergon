/* Probe verdicts using only the public libavformat probing interfaces. */
#include <errno.h>
#include <libavutil/md5.h>

typedef enum {
    PROBE_PLAYBACK, PROBE_RAW, PROBE_BYTES, PROBE_BLOCKING, PROBE_SEQUENTIAL
} PlaybackProbeMode;

typedef struct {
    size_t length, window;
    PlaybackProbeMode mode;
} PlaybackProbeCase;

typedef struct {
    const uint8_t *data;
    size_t size, offset, window;
} PlaybackProbeInput;

static int playback_probe_size(const char *text, size_t maximum, size_t *value)
{
    if (!*text || strspn(text, "0123456789") != strlen(text))
        return 0;
    unsigned long long number = strtoull(text, NULL, 10);
    if (!number || number > maximum)
        return 0;
    *value = number;
    return 1;
}

static uint64_t playback_probe_verdict(const char *name, PlaybackProbeMode mode)
{
    if (!strcmp(name, "matroska,webm"))
        return 5;
    if (!strncmp(name, "mov,", 4))
        return 6;
    if (!strcmp(name, "mpegts"))
        return 8;
    if (mode == PROBE_RAW) {
        if (!strcmp(name, "h264"))
            return 9;
        if (!strcmp(name, "hevc"))
            return 10;
    }
    if (mode >= PROBE_BYTES) {
        if (!strcmp(name, "flac"))
            return 1;
        if (!strcmp(name, "mp3"))
            return 2;
        if (!strcmp(name, "aac"))
            return 3;
        if (!strcmp(name, "ac3") || !strcmp(name, "eac3"))
            return 4;
        if (!strcmp(name, "ogg"))
            return 7;
    }
    return 0;
}

/* H.222.0 carrier sync at byte zero, or after the four-byte M2TS prefix.
 * The three-sync fixture rule has no width verdict for a tie. */
static size_t playback_probe_width(const uint8_t *data, size_t size)
{
    int ts = size > 376 && data[0] == 0x47 && data[188] == 0x47 && data[376] == 0x47;
    int m2ts = size > 388 && data[4] == 0x47 && data[196] == 0x47 && data[388] == 0x47;
    return ts == m2ts ? 0 : ts ? 188 : 192;
}

static int playback_probe_read(void *opaque, uint8_t *destination, int length)
{
    PlaybackProbeInput *source = opaque;
    size_t count = source->size - source->offset;
    if (!count)
        return AVERROR_EOF;
    if (count > (size_t)length)
        count = length;
    if (count > source->window)
        count = source->window;
    memcpy(destination, source->data + source->offset, count);
    source->offset += count;
    return (int)count;
}

static int64_t playback_probe_seek(void *opaque, int64_t offset, int whence)
{
    PlaybackProbeInput *source = opaque;
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

static int playback_probe_avio(PlaybackProbeInput *source, PlaybackProbeMode mode,
                               const AVInputFormat **format)
{
    int buffer_size = 4096;
    uint8_t *buffer = av_malloc(buffer_size);
    if (!buffer)
        return AVERROR(ENOMEM);
    AVIOContext *io = avio_alloc_context(buffer, buffer_size, 0, source,
                                         playback_probe_read, NULL,
                                         mode == PROBE_BLOCKING ? playback_probe_seek : NULL);
    if (!io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    int score = av_probe_input_buffer2(io, format, "", NULL, 0, 65536);
    size_t consumed = source->offset;
    if (score >= 0) {
        /* The public AVIO must return its buffered prefix again after probing. */
        uint8_t replay[4096];
        for (size_t at = 0; at < consumed;) {
            size_t remaining = consumed - at;
            int requested = remaining < sizeof(replay) ? (int)remaining : (int)sizeof(replay);
            int count = avio_read(io, replay, requested);
            if (count <= 0 || memcmp(replay, source->data + at, (size_t)count)) {
                score = AVERROR(EIO);
                break;
            }
            at += (size_t)count;
        }
    }
    source->offset = consumed;
    av_freep(&io->buffer);
    avio_context_free(&io);
    return score;
}

static int playback_probe_run(const ViewCase *input, ViewWork *work)
{
    const PlaybackProbeCase *probe = input->prepared;
    const AVInputFormat *format = NULL;
    int score = 0;
    size_t replay_length = 0;
    if (probe->mode >= PROBE_BLOCKING) {
        PlaybackProbeInput source = {input->data, probe->length, 0, probe->window};
        score = playback_probe_avio(&source, probe->mode, &format);
        if (score < 0 && score != AVERROR_INVALIDDATA && score != AVERROR_EOF)
            return 2;
        replay_length = source.offset;
    } else {
        uint8_t *buffer = av_mallocz(probe->length + AVPROBE_PADDING_SIZE);
        if (!buffer)
            return 2;
        memcpy(buffer, input->data, probe->length);
        AVProbeData data = {.filename = "", .buf = buffer, .buf_size = (int)probe->length};
        format = av_probe_input_format3(&data, 1, &score);
        av_free(buffer);
        if (score <= AVPROBE_SCORE_RETRY)
            format = NULL;
    }
    const char *name = format && score >= 0 ? format->name : "";
    const uint64_t verdict = playback_probe_verdict(name, probe->mode);
    const int adapter = probe->mode >= PROBE_BLOCKING;
    const uint64_t width = !adapter ? playback_probe_width(input->data, probe->length) : 0;
    const int replay = verdict && (adapter || (probe->mode < PROBE_BYTES && verdict < 9));
    uint8_t hash[16];
    size_t hash_size = 0;
    if (replay && !adapter) {
        replay_length = probe->length;
        av_md5_sum(hash, input->data, replay_length);
        hash_size = sizeof(hash);
    }
    const uint64_t words[] = {verdict, width, (uint64_t)replay, replay_length};
    view_record(work, 3, words, replay ? 4 : 3, hash, hash_size);
    if (work->trace) {
        printf("3 %" PRIu64 " %" PRIu64 " %d", verdict, width, replay);
        if (replay)
            printf(" %zu", replay_length);
        if (hash_size) {
            putchar(' ');
            for (size_t i = 0; i < hash_size; ++i)
                printf("%02x", hash[i]);
        }
        putchar('\n');
        fprintf(stderr, "probe name=%s score=%d\n", name, score);
    }
    return 0;
}

static int playback_probe_prepare(ViewCase *input)
{
    if (input->argc != 3)
        return 2;
    PlaybackProbeMode mode;
    const char *name = input->argv[0];
    const int facade = !strcmp(input->prefix, "facade-probe-");
    if (!facade && !strcmp(name, "playback"))
        mode = PROBE_PLAYBACK;
    else if (!facade && !strcmp(name, "raw"))
        mode = PROBE_RAW;
    else if (facade && !strcmp(name, "bytes"))
        mode = PROBE_BYTES;
    else if (facade && !strcmp(name, "blocking"))
        mode = PROBE_BLOCKING;
    else if (facade && !strcmp(name, "sequential"))
        mode = PROBE_SEQUENTIAL;
    else
        return 2;
    size_t length = input->size, window;
    if (strcmp(input->argv[1], "whole")) {
        if (!playback_probe_size(input->argv[1], 256 * 1024 * 1024, &length))
            return 2;
        if (length > input->size)
            length = input->size;
    }
    if (!playback_probe_size(input->argv[2], 65536, &window))
        return 2;
    PlaybackProbeCase *probe = malloc(sizeof(*probe));
    if (!probe)
        return 2;
    *probe = (PlaybackProbeCase){length, window, mode};
    input->prepared = probe;
    input->release = free;
    input->name = facade ? "facade-probe" : "playback-probe";
    input->run = playback_probe_run;
    return 0;
}
