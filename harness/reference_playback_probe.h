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

/* H.222.0 carrier sync at byte zero, or after the four-byte M2TS prefix, at three
 * positions; a tie has no width. */
static size_t playback_probe_width(const uint8_t *data, size_t size)
{
    int ts = size > 376 && data[0] == 0x47 && data[188] == 0x47 && data[376] == 0x47;
    int m2ts = size > 388 && data[4] == 0x47 && data[196] == 0x47 && data[388] == 0x47;
    return ts == m2ts ? 0 : ts ? 188 : 192;
}

/* FFmpeg names MPEG-TS without a packet spacing. For that verdict, the spacing the sync
 * bytes of the first three packets inside the input follow: 188 bytes from byte zero
 * (H.222.0 2.4.3.2), or 192 bytes after the four-byte M2TS prefix; 0 if both or neither. */
static uint64_t playback_probe_spacing(const uint8_t *data, size_t size)
{
    int fits[2] = {1, 1};
    for (size_t carrier = 0; carrier < 2; ++carrier) {
        const size_t start = carrier ? 4 : 0, step = carrier ? 192 : 188;
        for (size_t at = start; at < size && at <= start + 2 * step; at += step)
            fits[carrier] &= data[at] == 0x47;
    }
    return fits[0] == fits[1] ? 0 : fits[0] ? 188 : 192;
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

/* The opener's probe over the input as a whole stream, read in windows, then every
 * byte read back through the same context and checked against the input. */
static int playback_probe_avio(PlaybackProbeInput *source, PlaybackProbeMode mode,
                               const AVInputFormat **format, int trace)
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
    /* The facade's adapters probe at most 64 KiB; the others use the default size. */
    const unsigned maximum = mode >= PROBE_BLOCKING ? 65536 : 0;
    int score = av_probe_input_buffer2(io, format, "", NULL, 0, maximum);
    if (trace)
        fprintf(stderr, "probe consumed %zu bytes before handoff\n", source->offset);
    if (score >= 0 && *format && playback_probe_verdict((*format)->name, mode)) {
        /* Verify both the buffered bytes and all bytes following that buffer. */
        uint8_t replay[4096];
        for (size_t at = 0; at < source->size;) {
            size_t remaining = source->size - at;
            int requested = remaining < sizeof(replay) ? (int)remaining : (int)sizeof(replay);
            int count = avio_read(io, replay, requested);
            if (count <= 0 || memcmp(replay, source->data + at, (size_t)count)) {
                score = AVERROR(EIO);
                break;
            }
            at += (size_t)count;
        }
        if (score >= 0 && avio_read(io, replay, 1) != AVERROR_EOF)
            score = AVERROR(EIO);
    }
    av_freep(&io->buffer);
    avio_context_free(&io);
    return score;
}

/* An ISO BMFF movie read from a sequential source: the MOV demuxer over a context that
 * cannot seek, without parsers, timestamp fill-in or edit lists, so that its packets are
 * the samples and its times the media clock, as the facade's reader states them. Folds
 * `5 id numerator denominator` for each stream's track_ID and time base, `4 id size pts
 * dts` with the payload for each packet, and `6 status`: 0 where the demuxer read to the
 * end of the source, 1 where it refused the input or a read failed. */
static int playback_probe_movie(const ViewCase *input, const PlaybackProbeCase *probe,
                                const AVInputFormat *format, ViewWork *work)
{
    PlaybackProbeInput source = {input->data, probe->length, 0, probe->window};
    const int buffer_size = 4096;
    uint8_t *buffer = av_malloc(buffer_size);
    AVIOContext *io = buffer ? avio_alloc_context(buffer, buffer_size, 0, &source,
                                                  playback_probe_read, NULL, NULL) : NULL;
    AVFormatContext *movie = avformat_alloc_context();
    AVPacket *packet = av_packet_alloc();
    AVDictionary *options = NULL;
    int result = 2;
    if (!io || !movie || !packet || av_dict_set(&options, "ignore_editlist", "1", 0) < 0)
        goto done;
    movie->pb = io;
    movie->flags |= AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    /* A refused input frees the context. */
    int status = avformat_open_input(&movie, "", format, &options);
    if (status >= 0) {
        for (unsigned i = 0; i < movie->nb_streams; ++i) {
            const AVStream *stream = movie->streams[i];
            const uint64_t words[] = {(uint64_t)stream->id, (uint64_t)stream->time_base.num,
                                      (uint64_t)stream->time_base.den};
            view_record(work, 5, words, 3, NULL, 0);
            if (work->trace)
                printf("5 %" PRIu64 " %" PRIu64 " %" PRIu64 "\n", words[0], words[1], words[2]);
        }
        while ((status = av_read_frame(movie, packet)) >= 0) {
            const uint64_t words[] = {(uint64_t)movie->streams[packet->stream_index]->id,
                                      (uint64_t)packet->size, (uint64_t)packet->pts,
                                      (uint64_t)packet->dts};
            view_record(work, 4, words, 4, packet->data, (size_t)packet->size);
            if (work->trace)
                printf("4 %" PRIu64 " %" PRIu64 " %" PRIu64 " %" PRIu64 "\n", words[0],
                       words[1], words[2], words[3]);
            av_packet_unref(packet);
        }
        if (status == AVERROR_EOF)
            status = 0;
    }
    const uint64_t ended = status < 0;
    view_record(work, 6, &ended, 1, NULL, 0);
    if (work->trace) {
        printf("6 %" PRIu64 "\n", ended);
        if (status < 0)
            fprintf(stderr, "movie read ended: %s\n", av_err2str(status));
    }
    result = 0;
done:
    av_dict_free(&options);
    av_packet_free(&packet);
    avformat_close_input(&movie);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    } else {
        av_free(buffer);
    }
    return result;
}

/* Bytes mode folds `3 verdict width retry`: for the whole input the opener's selection
 * (retry 0); for a shorter prefix the best-scoring format of av_probe_input_format3 and
 * whether its score is at most AVPROBE_SCORE_RETRY, at which the public header
 * recommends a retry with a larger probe buffer. Every other mode opens the prefix as a
 * whole stream and folds `3 verdict width replay [length [md5]]`; a sequential source
 * the opener names ISO BMFF is then read as a movie. */
static int playback_probe_run(const ViewCase *input, ViewWork *work)
{
    const PlaybackProbeCase *probe = input->prepared;
    const AVInputFormat *format = NULL;
    int score = 0;
    const int prefix = probe->mode == PROBE_BYTES && probe->length < input->size;
    if (prefix) {
        uint8_t *buffer = av_mallocz(probe->length + AVPROBE_PADDING_SIZE);
        if (!buffer)
            return 2;
        memcpy(buffer, input->data, probe->length);
        AVProbeData data = {.filename = "", .buf = buffer, .buf_size = (int)probe->length};
        format = av_probe_input_format3(&data, 1, &score);
        av_free(buffer);
    } else {
        PlaybackProbeInput source = {input->data, probe->length, 0, probe->window};
        score = playback_probe_avio(&source, probe->mode, &format, work->trace);
        if (score < 0 && score != AVERROR_INVALIDDATA && score != AVERROR_EOF)
            return 2;
    }
    const char *name = format && score >= 0 ? format->name : "";
    const uint64_t verdict = playback_probe_verdict(name, probe->mode);
    const int adapter = probe->mode >= PROBE_BLOCKING;
    /* The adapters fold no width. Where no transport is selected, the playback modes
     * fold the same three-sync prefix fact the playback candidate states. */
    uint64_t width = 0;
    if (!adapter && verdict == 8)
        width = playback_probe_spacing(input->data, probe->length);
    else if (probe->mode < PROBE_BYTES)
        width = playback_probe_width(input->data, probe->length);
    uint64_t words[4] = {verdict, width};
    size_t count = 3;
    uint8_t hash[16];
    size_t hash_size = 0;
    if (probe->mode == PROBE_BYTES) {
        words[2] = prefix && score <= AVPROBE_SCORE_RETRY;
    } else if (verdict && (adapter || verdict < 9)) {
        /* playback_probe_avio has read every byte back through the opener. */
        words[2] = 1;
        words[3] = probe->length;
        count = 4;
        if (!adapter) {
            av_md5_sum(hash, input->data, probe->length);
            hash_size = sizeof(hash);
        }
    }
    view_record(work, 3, words, count, hash, hash_size);
    if (work->trace) {
        printf("3");
        for (size_t i = 0; i < count; ++i)
            printf(" %" PRIu64, words[i]);
        if (hash_size) {
            putchar(' ');
            for (size_t i = 0; i < hash_size; ++i)
                printf("%02x", hash[i]);
        }
        putchar('\n');
        fprintf(stderr, "probe %s name=%s score=%d\n", prefix ? "prefix" : "opener", name,
                score);
    }
    if (probe->mode == PROBE_SEQUENTIAL && verdict == 6)
        return playback_probe_movie(input, probe, format, work);
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
