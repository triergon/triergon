/* ISO BMFF open and index reference using only the public FFmpeg API.
 * Ordered traces establish parity; the fold records measured work.
 * This executable is separate from the Rust libraries.
 */
#define _POSIX_C_SOURCE 200809L
#define _FILE_OFFSET_BITS 64
#include "reference_isobmff_hash.h"
#include <errno.h>
#include <inttypes.h>
#include <libavcodec/packet.h>
#include <libavformat/avformat.h>
#include <libavutil/dict.h>
#include <libavutil/error.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>
#define CONTRACT_SHA256 "0a924396626871e84b2f91ec612a0c60b09258f0f016afe0753cf7c75fdecf8a"

#define DOMAIN UINT64_C(0x4d4f5633)
#define PACKET_CAP UINT64_C(1000000)
#define BYTE_CAP UINT64_C(268435456)
#define SINGLE_CAP (16 * 1024 * 1024)
typedef struct {
    uint64_t packets, bytes, digest;
} Stats;
typedef struct {
    uint8_t *bytes;
    FILE *file;
    int64_t length, position;
    size_t maximum;
    int file_io;
} Source;
typedef struct {
    AVPacket *packet;
    int64_t pid;
} Hold;
typedef struct {
    size_t maximum, retention;
    int file_io;
} Options;
typedef struct {
    int enabled, after_eof;
    int32_t track;
    int64_t ticks;
    uint32_t count;
} SeekRequest;
static void fold(uint64_t *h, uint64_t v) {
    *h = *h * 31 + v;
}
static void bytes_fold(uint64_t *h, const uint8_t *p, size_t n) {
    /* Exact same full-byte byte polynomial and eight-byte expansion as Rust. */
    while (n >= 8) {
        uint64_t a = (uint64_t)p[0] * 29791 + (uint64_t)p[1] * 961 + (uint64_t)p[2] * 31 + p[3];
        uint64_t b = (uint64_t)p[4] * 29791 + (uint64_t)p[5] * 961 + (uint64_t)p[6] * 31 + p[7];
        *h = *h * UINT64_C(852891037441) + a * 923521 + b;
        p += 8;
        n -= 8;
    }
    for (size_t i = 0; i < n; i++)
        fold(h, p[i]);
}
static void framed(uint64_t *h, const uint8_t *p, size_t n) {
    fold(h, n);
    bytes_fold(h, p, n);
}
static void text_fold(uint64_t *h, const char *s) {
    framed(h, (const uint8_t *)s, strlen(s));
}
static void hex(const uint8_t *p, size_t n) {
    putchar('"');
    for (size_t i = 0; i < n; i++)
        printf("%02x", p[i]);
    putchar('"');
}
static void quote(const char *s) {
    putchar('"');
    for (; *s; s++) {
        unsigned char c = (unsigned char)*s;
        if (c == '"' || c == '\\') {
            putchar('\\');
            putchar(c);
        } else if (c < 32)
            printf("\\u%04x", c);
        else
            putchar(c);
    }
    putchar('"');
}
static void nullable(int64_t t) {
    if (t == AV_NOPTS_VALUE)
        printf("null");
    else
        printf("%" PRId64, t);
}
static void dict_trace(const AVDictionary *d) {
    const AVDictionaryEntry *e = NULL;
    int first = 1;
    putchar('[');
    while ((e = av_dict_iterate(d, e))) {
        if (!first) {
            putchar(',');
        }
        first = 0;
        printf("{\"key_hex\":");
        hex((const uint8_t *)e->key, strlen(e->key));
        printf(",\"value_hex\":");
        hex((const uint8_t *)e->value, strlen(e->value));
        putchar('}');
    }
    putchar(']');
}
static void dict_fold(uint64_t *h, const AVDictionary *d) {
    fold(h, (uint64_t)av_dict_count(d));
    const AVDictionaryEntry *e = NULL;
    while ((e = av_dict_iterate(d, e))) {
        text_fold(h, e->key);
        text_fold(h, e->value);
    }
}
static int read_source(void *opaque, uint8_t *output, int requested) {
    Source *s = opaque;
    if (requested <= 0)
        return AVERROR(EINVAL);
    if (s->position == s->length)
        return AVERROR_EOF;
    if (s->position < 0 || s->position > s->length)
        return AVERROR(EIO);
    size_t n = (size_t)requested;
    int64_t left = s->length - s->position;
    if ((uint64_t)n > (uint64_t)left)
        n = (size_t)left;
    if (n > s->maximum)
        n = s->maximum;
    if (s->file_io) {
        size_t got = fread(output, 1, n, s->file);
        if (got == 0)
            return ferror(s->file) ? AVERROR(EIO) : AVERROR_EOF;
        n = got;
    } else
        memcpy(output, s->bytes + (size_t)s->position, n);
    s->position += (int64_t)n;
    return (int)n;
}
static int64_t seek_source(void *opaque, int64_t offset, int whence) {
    Source *s = opaque;
    if (whence == AVSEEK_SIZE) {
        return s->length;
    }
    whence &= ~AVSEEK_FORCE;
    int64_t base = whence == SEEK_SET   ? 0
                   : whence == SEEK_CUR ? s->position
                   : whence == SEEK_END ? s->length
                                        : -1;
    if (base < 0 || offset < -base || offset > s->length - base)
        return AVERROR(EINVAL);
    int64_t at = base + offset;
    if (s->file_io && fseeko(s->file, (off_t)at, SEEK_SET) != 0)
        return AVERROR(EIO);
    s->position = at;
    return at;
}
static int packet_fold(Stats *s, const AVPacket *p, int64_t pid) {
    if (p->size < 0 || p->size > SINGLE_CAP || s->packets >= PACKET_CAP ||
        s->bytes > BYTE_CAP - (uint64_t)p->size)
        return AVERROR(EFBIG);
    fold(&s->digest, 0x50);
    fold(&s->digest, (uint64_t)p->stream_index);
    fold(&s->digest, (uint64_t)pid);
    fold(&s->digest, p->pts != AV_NOPTS_VALUE);
    fold(&s->digest, (uint64_t)p->pts);
    fold(&s->digest, p->dts != AV_NOPTS_VALUE);
    fold(&s->digest, (uint64_t)p->dts);
    fold(&s->digest, (uint64_t)p->duration);
    fold(&s->digest, (uint64_t)p->pos);
    fold(&s->digest, (uint32_t)p->flags);
    framed(&s->digest, p->data, (size_t)p->size);
    fold(&s->digest, (uint64_t)p->side_data_elems);
    for (int i = 0; i < p->side_data_elems; i++) {
        fold(&s->digest, (uint64_t)p->side_data[i].type);
        framed(&s->digest, p->side_data[i].data, p->side_data[i].size);
    }
    s->packets++;
    s->bytes += (uint64_t)p->size;
    return 0;
}
static void packet_trace(const AVPacket *p, int64_t pid) {
    printf("{\"stream\":%d,\"pid\":%" PRId64 ",\"pts_raw\":%" PRId64 ",\"dts_raw\":%" PRId64
           ",\"pts\":",
           p->stream_index, pid, p->pts, p->dts);
    nullable(p->pts);
    printf(",\"dts\":");
    nullable(p->dts);
    printf(",\"duration\":%" PRId64 ",\"position\":%" PRId64 ",\"flags\":%u,\"payload\":",
           p->duration, p->pos, (unsigned)p->flags);
    hex(p->data, (size_t)p->size);
    printf(",\"side_data\":[");
    for (int i = 0; i < p->side_data_elems; i++) {
        if (i) {
            putchar(',');
        }
        printf("{\"type\":%d,\"data\":", p->side_data[i].type);
        hex(p->side_data[i].data, p->side_data[i].size);
        putchar('}');
    }
    printf("]}");
}
static int catalogue(AVFormatContext *f, Stats *stats, int trace) {
    fold(&stats->digest, 0x43);
    fold(&stats->digest, f->nb_streams);
    if (trace)
        printf("],\"streams\":[");
    for (unsigned i = 0; i < f->nb_streams; i++) {
        const AVStream *s = f->streams[i];
        const AVCodecParameters *p = s->codecpar;
        const char *codec = avcodec_get_name(p->codec_id);
        if (p->extradata_size < 0)
            return AVERROR_INVALIDDATA;
        fold(&stats->digest, 0x53);
        fold(&stats->digest, i);
        fold(&stats->digest, (uint64_t)(int64_t)s->id);
        text_fold(&stats->digest, codec);
        fold(&stats->digest, (uint64_t)p->codec_type);
        fold(&stats->digest, (uint64_t)s->time_base.num);
        fold(&stats->digest, (uint64_t)s->time_base.den);
        framed(&stats->digest, p->extradata, (size_t)p->extradata_size);
        dict_fold(&stats->digest, s->metadata);
        if (trace) {
            if (i) {
                putchar(',');
            }
            printf("{\"stream\":%u,\"pid\":%d,\"codec\":", i, s->id);
            quote(codec);
            printf(",\"type\":%d,\"time_base\":[%d,%d],\"extradata\":", p->codec_type,
                   s->time_base.num, s->time_base.den);
            hex(p->extradata, (size_t)p->extradata_size);
            printf(",\"tags\":");
            dict_trace(s->metadata);
            putchar('}');
        }
    }
    fold(&stats->digest, 0x47);
    fold(&stats->digest, f->nb_programs);
    if (trace)
        printf("],\"programs\":[");
    for (unsigned i = 0; i < f->nb_programs; i++) {
        const AVProgram *p = f->programs[i];
        fold(&stats->digest, (uint64_t)(int64_t)p->id);
        fold(&stats->digest, (uint64_t)(int64_t)p->program_num);
        fold(&stats->digest, (uint64_t)(int64_t)p->pmt_pid);
        fold(&stats->digest, (uint64_t)(int64_t)p->pcr_pid);
        dict_fold(&stats->digest, p->metadata);
        fold(&stats->digest, p->nb_stream_indexes);
        for (unsigned j = 0; j < p->nb_stream_indexes; j++)
            fold(&stats->digest, p->stream_index[j]);
        if (trace) {
            if (i) {
                putchar(',');
            }
            printf("{\"id\":%d,\"program_num\":%d,\"pmt_pid\":%d,\"pcr_pid\":%d,\"tags\":", p->id,
                   p->program_num, p->pmt_pid, p->pcr_pid);
            dict_trace(p->metadata);
            printf(",\"streams\":[");
            for (unsigned j = 0; j < p->nb_stream_indexes; j++) {
                if (j) {
                    putchar(',');
                }
                printf("%u", p->stream_index[j]);
            }
            printf("]}");
        }
    }
    if (trace) {
        putchar(']');
    }
    return 0;
}
static int index_snapshot(AVFormatContext *f, Stats *stats, int trace) {
    uint64_t total = 0;
    fold(&stats->digest, 0x49);
    fold(&stats->digest, f->nb_streams);
    if (trace) {
        printf(",\"native_indexes\":[");
    }
    for (unsigned i = 0; i < f->nb_streams; i++) {
        AVStream *s = f->streams[i];
        int count = avformat_index_get_entries_count(s);
        if (count < 0 || (uint64_t)count > PACKET_CAP - total) {
            return AVERROR(EFBIG);
        }
        total += (uint64_t)count;
        fold(&stats->digest, i);
        fold(&stats->digest, (uint64_t)count);
        if (trace) {
            if (i) {
                putchar(',');
            }
            printf("{\"stream\":%u,\"pid\":%d,\"entries\":[", i, s->id);
        }
        for (int j = 0; j < count; j++) {
            const AVIndexEntry *e = avformat_index_get_entry(s, j);
            if (!e) {
                return AVERROR_INVALIDDATA;
            }
            fold(&stats->digest, (uint64_t)e->pos);
            fold(&stats->digest, (uint64_t)e->timestamp);
            fold(&stats->digest, (uint64_t)e->flags);
            fold(&stats->digest, (uint64_t)e->size);
            fold(&stats->digest, (uint64_t)e->min_distance);
            if (trace) {
                if (j) {
                    putchar(',');
                }
                printf("{\"ordinal\":%d,\"position\":%" PRId64 ",\"timestamp\":%" PRId64
                       ",\"flags\":%d,\"size\":%d,\"min_distance\":%d}",
                       j, e->pos, e->timestamp, e->flags, e->size, e->min_distance);
            }
        }
        if (trace) {
            printf("]}");
        }
    }
    if (trace) {
        putchar(']');
    }
    return 0;
}
static int operation(Source *s, Options o, Stats *stats, int trace, const SeekRequest *seek,
                     int indexes) {
    int result = 0, read_result = AVERROR_EOF, first = 1, seek_result = 0;
    size_t next = 0;
    AVFormatContext *format = NULL;
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    Hold *held = NULL;
    Stats drain = {0};
    uint64_t read_calls = 0;
    if (seek_source(s, 0, SEEK_SET) < 0)
        return AVERROR(EIO);
    fold(&stats->digest, DOMAIN);
    fold(&stats->digest, o.retention);
    fold(&stats->digest, o.maximum);
    fold(&stats->digest, (uint64_t)o.file_io);
    format = avformat_alloc_context();
    uint8_t *buffer = av_malloc(32768);
    if (!format || !buffer) {
        av_free(buffer);
        result = AVERROR(ENOMEM);
        goto done;
    }
    io = avio_alloc_context(buffer, 32768, 0, s, read_source, NULL, seek_source);
    if (!io) {
        av_free(buffer);
        result = AVERROR(ENOMEM);
        goto done;
    }
    io->seekable = AVIO_SEEKABLE_NORMAL;
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    const AVInputFormat *mov = av_find_input_format("mov");
    if (!mov) {
        result = AVERROR_DEMUXER_NOT_FOUND;
        goto done;
    }
    result = avformat_open_input(&format, NULL, mov, NULL);
    if (result < 0)
        goto done;
    if (format->nb_streams > 64) {
        result = AVERROR(EFBIG);
        goto done;
    }
    packet = av_packet_alloc();
    if (!packet) {
        result = AVERROR(ENOMEM);
        goto done;
    }
    if (o.retention) {
        held = calloc(o.retention, sizeof(*held));
        if (!held) {
            result = AVERROR(ENOMEM);
            goto done;
        }
    }
    if (seek && seek->enabled) {
        read_result = 0; /* No read after seek yet; never invent EOF at a limit. */
        int stream = -1;
        for (unsigned i = 0; i < format->nb_streams; i++)
            if (format->streams[i]->id == seek->track) {
                if (stream >= 0) {
                    result = AVERROR_INVALIDDATA;
                    goto done;
                }
                stream = (int)i;
            }
        if (stream < 0) {
            result = AVERROR_STREAM_NOT_FOUND;
            goto done;
        }
        if (seek->after_eof) {
            int rc;
            while ((rc = av_read_frame(format, packet)) >= 0) {
                if (packet->stream_index < 0 ||
                    (unsigned)packet->stream_index >= format->nb_streams) {
                    result = AVERROR_INVALIDDATA;
                    goto done;
                }
                result = packet_fold(&drain, packet, format->streams[packet->stream_index]->id);
                av_packet_unref(packet);
                if (result < 0)
                    goto done;
            }
            if (rc != AVERROR_EOF) {
                result = rc;
                goto done;
            }
            fold(&stats->digest, 0x44);
            fold(&stats->digest, drain.packets);
            fold(&stats->digest, drain.bytes);
            fold(&stats->digest, drain.digest);
        }
        /* Public documented call: no private flush, retry or packet filtering. */
        seek_result = av_seek_frame(format, stream, seek->ticks, AVSEEK_FLAG_BACKWARD);
        fold(&stats->digest, 0x51);
        fold(&stats->digest, (uint64_t)(int64_t)seek_result);
    }
    if (trace) {
        printf("{\"schema\":33,\"operation\":\"%s\"", seek && seek->enabled
                                                          ? "mov-native-seek-consumer-v33"
                                                      : indexes ? "mov-native-index-consumer-v33"
                                                                : "mov-native-open-consumer-v33");
        if (seek && seek->enabled)
            printf(",\"track_id\":%d,\"native_ticks\":%" PRId64
                   ",\"after_eof\":%s,\"seek_result\":%d,\"native_drain\":{\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 "}",
                   seek->track, seek->ticks, seek->after_eof ? "true" : "false", seek_result,
                   drain.packets, drain.bytes, drain.digest);
        printf(",\"packets\":[");
    }
    if (seek_result >= 0)
        while (!(seek && seek->enabled && stats->packets >= seek->count)) {
            read_calls++;
            read_result = av_read_frame(format, packet);
            if (read_result < 0)
                break;
            if (packet->stream_index < 0 || (unsigned)packet->stream_index >= format->nb_streams) {
                result = AVERROR_INVALIDDATA;
                goto done;
            }
            int64_t pid = format->streams[packet->stream_index]->id;
            result = packet_fold(stats, packet, pid);
            if (result < 0)
                goto done;
            if (trace) {
                if (!first) {
                    putchar(',');
                }
                first = 0;
                packet_trace(packet, pid);
            }
            if (o.retention) {
                Hold *h = &held[next];
                if (!h->packet)
                    h->packet = av_packet_alloc();
                if (!h->packet) {
                    result = AVERROR(ENOMEM);
                    goto done;
                }
                av_packet_unref(h->packet);
                av_packet_move_ref(h->packet, packet);
                h->pid = pid;
                next = (next + 1) % o.retention;
            } else
                av_packet_unref(packet);
        }
    if (!seek || !seek->enabled) {
        if (read_result != AVERROR_EOF) {
            result = read_result;
            goto done;
        }
    }
    result = catalogue(format, stats, trace);
    if (result < 0)
        goto done;
    if (indexes) {
        result = index_snapshot(format, stats, trace);
        if (result < 0) {
            goto done;
        }
    }
    fold(&stats->digest, 0x45);
    fold(&stats->digest, (uint64_t)(int64_t)read_result);
    fold(&stats->digest, read_result == AVERROR_EOF);
    if (trace)
        printf(",\"read_result\":%d,\"confirmed_eof\":%s", read_result,
               read_result == AVERROR_EOF ? "true" : "false");
    if (trace && seek && seek->enabled) {
        printf(",\"read_calls_after_seek\":%" PRIu64 ",\"delivery_limit_reached\":%s", read_calls,
               seek_result >= 0 && stats->packets >= seek->count && read_result >= 0 ? "true"
                                                                                     : "false");
    }
    /* Close demux/input before touching retained packet data again. */
    av_packet_free(&packet);
    avformat_close_input(&format);
    av_freep(&io->buffer);
    avio_context_free(&io);
    fold(&stats->digest, 0x52);
    fold(&stats->digest, o.retention);
    for (size_t i = 0; i < o.retention; i++) {
        fold(&stats->digest, i);
        fold(&stats->digest, held[i].packet != NULL);
        if (held[i].packet) {
            Stats retained = {0, 0, stats->digest};
            result = packet_fold(&retained, held[i].packet, held[i].pid);
            if (result < 0)
                goto done;
            stats->digest = retained.digest;
        }
    }
    if (trace)
        printf(",\"work\":{\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
               "}}\n",
               stats->packets, stats->bytes, stats->digest);
done:
    if (held) {
        for (size_t i = 0; i < o.retention; i++)
            av_packet_free(&held[i].packet);
        free(held);
    }
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return result;
}
static int number(const char *s, uint64_t cap, uint64_t *value) {
    if (!s || !*s || *s == '-') {
        return -1;
    }
    char *end = NULL;
    errno = 0;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno || !end || *end || (uint64_t)v > cap) {
        return -1;
    }
    *value = (uint64_t)v;
    return 0;
}
static int signed_number(const char *s, int64_t *value) {
    if (!s || !*s) {
        return -1;
    }
    char *end = NULL;
    errno = 0;
    long long v = strtoll(s, &end, 10);
    if (errno || !end || *end) {
        return -1;
    }
    *value = (int64_t)v;
    return 0;
}
static int source_open(Source *s, const char *path, Options o) {
    memset(s, 0, sizeof(*s));
    s->maximum = o.maximum;
    s->file_io = o.file_io;
    s->file = fopen(path, "rb");
    if (!s->file)
        return AVERROR(EIO);
    if (fseeko(s->file, 0, SEEK_END) != 0) {
        return AVERROR(EIO);
    }
    off_t end = ftello(s->file);
    if (end < 0) {
        return AVERROR(EIO);
    }
    s->length = (int64_t)end;
    if ((off_t)s->length != end) {
        return AVERROR(EFBIG);
    }
    rewind(s->file);
    if (!o.file_io) {
        if (s->length > 64 * 1024 * 1024) {
            return AVERROR(EFBIG);
        }
        size_t n = (size_t)s->length;
        s->bytes = malloc(n ? n : 1);
        if (!s->bytes)
            return AVERROR(ENOMEM);
        if (fread(s->bytes, 1, n, s->file) != n) {
            return AVERROR(EIO);
        }
        fclose(s->file);
        s->file = NULL;
    }
    return 0;
}
/* For file input, pread reads the underlying file without allocating FILE's
 * operation buffer or changing its cursor before the measured window. */
static int source_proof(Source *s, InputProof *proof) {
    Sha256 hash;
    sha_init(&hash);
    if (s->length < 0) {
        return AVERROR(EIO);
    }
    if (!s->file_io) {
        if (sha_update(&hash, s->bytes, (size_t)s->length) < 0) {
            return AVERROR(EOVERFLOW);
        }
    } else {
        uint8_t block[65536];
        uint64_t at = 0;
        while (at < (uint64_t)s->length) {
            size_t maximum = (uint64_t)s->length - at > sizeof(block)
                                 ? sizeof(block)
                                 : (size_t)((uint64_t)s->length - at);
            ssize_t count = pread(fileno(s->file), block, maximum, (off_t)at);
            if (count <= 0) {
                return AVERROR(EIO);
            }
            if (sha_update(&hash, block, (size_t)count) < 0) {
                return AVERROR(EOVERFLOW);
            }
            at += (uint64_t)count;
        }
    }
    proof->bytes = hash.bytes;
    sha_finish(&hash, proof->sha256);
    return 0;
}
static uint64_t nanoseconds(struct timespec t) {
    return (uint64_t)t.tv_sec * UINT64_C(1000000000) + (uint64_t)t.tv_nsec;
}
static void stats_print(Stats s, uint64_t n, uint64_t elapsed, const InputProof *proof) {
    printf("{\"schema\":1,\"iterations\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64
           ",\"packets\":%" PRIu64 ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64
           ",\"input_bytes\":%" PRIu64 ",\"input_sha256\":\"",
           n, elapsed, s.packets, s.bytes, s.digest, proof->bytes);
    proof_hex(stdout, proof);
    printf("\",\"contract_sha256\":\"%s\"}\n", CONTRACT_SHA256);
}
int main(int argc, char **argv) {
    if ((argc == 2 || (argc == 3 && !strcmp(argv[2], "-"))) && !strcmp(argv[1], "identity")) {
        printf("{\"schema\":33,\"worker\":\"ffmpeg-isobmff-open-index\",\"avformat_version\":%u,"
               "\"configuration\":",
               avformat_version());
        quote(avformat_configuration());
        printf(",\"qualification\":false}\n");
        return 0;
    }
    if (argc < 3)
        return 2;
    const char *mode = argv[1];
    int bench = !strcmp(mode, "open-bench") || !strcmp(mode, "native-index-bench") ||
                !strcmp(mode, "seek-bench");
    int seek_mode =
        !strcmp(mode, "seek-trace") || !strcmp(mode, "seek-bench") || !strcmp(mode, "seek-memory");
    int memory = !strcmp(mode, "open-memory") || !strcmp(mode, "open-memory-baseline") ||
                 !strcmp(mode, "native-index-memory") || !strcmp(mode, "seek-memory");
    int baseline = !strcmp(mode, "baseline") || !strcmp(mode, "open-memory-baseline");
    int indexes = !strcmp(mode, "native-index-trace") || !strcmp(mode, "native-index-bench") ||
                  !strcmp(mode, "native-index-memory");
    int proof_mode = !strcmp(mode, "input-proof");
    int trace = !strcmp(mode, "open-trace") || !strcmp(mode, "seek-trace") ||
                !strcmp(mode, "native-index-trace");
    if (!bench && !memory && !baseline && !trace && !proof_mode)
        return 2;
    uint64_t iterations = 1, v = 0;
    int offset = seek_mode ? (bench ? 8 : 7) : (bench ? 4 : 3);
    if (bench && (argc < 4 || number(argv[3], 1000000, &iterations) < 0 || iterations == 0))
        return 2;
    Options o = {65536, 0, 0};
    SeekRequest request = {0};
    if (seek_mode) {
        int64_t track, ticks;
        uint64_t count, after;
        int at = bench ? 4 : 3;
        if (argc < at + 4 || signed_number(argv[at], &track) < 0 || track < INT32_MIN ||
            track > INT32_MAX || signed_number(argv[at + 1], &ticks) < 0 ||
            number(argv[at + 2], 64, &count) < 0 || number(argv[at + 3], 1, &after) < 0)
            return 2;
        request = (SeekRequest){1, (int)after, (int32_t)track, ticks, (uint32_t)count};
    }
    if (argc > offset) {
        if (number(argv[offset], 65536, &v) < 0 || v == 0) {
            return 2;
        }
        o.maximum = (size_t)v;
    }
    if (argc > offset + 1) {
        if (number(argv[offset + 1], 64, &v) < 0) {
            return 2;
        }
        o.retention = (size_t)v;
    }
    if (argc > offset + 2) {
        if (!strcmp(argv[offset + 2], "file"))
            o.file_io = 1;
        else if (strcmp(argv[offset + 2], "memory"))
            return 2;
    }
    /* Same optional work-quantum argument as Rust; native API is blocking. */
    if (argc > offset + 3 && (number(argv[offset + 3], UINT64_MAX, &v) < 0 || v == 0))
        return 2;
    if (argc > offset + 4)
        return 2;
    Source source;
    int rc = source_open(&source, argv[2], o);
    if (rc < 0) {
        if (source.file) {
            fclose(source.file);
        }
        free(source.bytes);
        goto failed;
    }
    InputProof proof;
    rc = source_proof(&source, &proof);
    if (rc < 0) {
        if (source.file) {
            fclose(source.file);
        }
        free(source.bytes);
        goto failed;
    }
    fputs("TRIERGON_INPUT_V1 ", stderr);
    proof_report(stderr, &proof);
    fputc('\n', stderr);
    fflush(stderr);
    if (proof_mode) {
        proof_report(stdout, &proof);
        putchar('\n');
        if (source.file) {
            fclose(source.file);
        }
        free(source.bytes);
        return 0;
    }
    if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) {
        if (source.file) {
            fclose(source.file);
        }
        free(source.bytes);
        return 74;
    }
    Stats total = {0};
    struct timespec start = {0}, end = {0};
    if (bench && clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        rc = AVERROR(EIO);
        goto complete;
    }
    if (!baseline)
        for (uint64_t i = 0; i < iterations; i++) {
            Stats one = {0};
            rc = operation(&source, o, &one, trace, seek_mode ? &request : NULL, indexes);
            if (rc < 0)
                break;
            if (UINT64_MAX - total.packets < one.packets || UINT64_MAX - total.bytes < one.bytes) {
                rc = AVERROR(EOVERFLOW);
                break;
            }
            total.packets += one.packets;
            total.bytes += one.bytes;
            fold(&total.digest, one.digest);
        }
    if (bench && clock_gettime(CLOCK_MONOTONIC, &end) != 0)
        rc = AVERROR(EIO);
complete:
    if (memory && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16)
        rc = AVERROR(EIO);
    if (rc >= 0 && !trace) {
        if (baseline && !memory)
            printf("{\"schema\":1,\"baseline\":true}\n");
        else
            stats_print(total, iterations, bench ? nanoseconds(end) - nanoseconds(start) : 0,
                        &proof);
    }
    if (source.file) {
        fclose(source.file);
    }
    free(source.bytes);
    if (rc >= 0) {
        return 0;
    }
failed: {
    char message[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(rc, message, sizeof(message));
    fprintf(stderr, "mov-consumer: %s (%d)\n", message, rc);
}
    return 1;
}
