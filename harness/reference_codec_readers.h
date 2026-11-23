/* Public libavformat reader operations over preloaded inputs. */
#include <errno.h>
#include <limits.h>
#include <libavutil/intreadwrite.h>

typedef struct {
    const uint8_t *data;
    size_t size, offset;
} ReaderSource;

typedef struct {
    AVFormatContext *format;
    AVIOContext *io;
    ReaderSource source;
} ReaderInput;

static int reader_read(void *opaque, uint8_t *buf, int count)
{
    ReaderSource *source = opaque;
    size_t remain = source->size - source->offset;
    if (remain == 0)
        return AVERROR_EOF;
    if ((size_t)count > remain)
        count = (int)remain;
    memcpy(buf, source->data + source->offset, (size_t)count);
    source->offset += (size_t)count;
    return count;
}

static int64_t reader_seek(void *opaque, int64_t offset, int whence)
{
    ReaderSource *source = opaque;
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

static void reader_close(ReaderInput *reader)
{
    avformat_close_input(&reader->format);
    if (reader->io) {
        av_freep(&reader->io->buffer);
        avio_context_free(&reader->io);
    }
}

static int reader_open(ReaderInput *reader, const ViewCase *input, const char *format)
{
    reader->source = (ReaderSource){input->data, input->size, 0};
    uint8_t *buffer = av_malloc(32768);
    if (!buffer)
        return AVERROR(ENOMEM);
    reader->io = avio_alloc_context(buffer, 32768, 0, &reader->source, reader_read, NULL, reader_seek);
    if (!reader->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    reader->format = avformat_alloc_context();
    if (!reader->format)
        return AVERROR(ENOMEM);
    reader->format->pb = reader->io;
    reader->format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_BITEXACT;
    const AVInputFormat *demuxer = av_find_input_format(format);
    if (!demuxer)
        return AVERROR_DEMUXER_NOT_FOUND;
    return avformat_open_input(&reader->format, NULL, demuxer, NULL);
}

static void reader_record(ViewWork *work, uint64_t kind, const uint64_t *words,
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

static int reader_refusal(ViewWork *work)
{
    reader_record(work, 8, NULL, 0, NULL, 0);
    return 0;
}

typedef struct {
    AVProbeData data;
    int elementary, whole;
    uint8_t padded[];
} ReaderProbe;

static uint64_t reader_probe_verdict(const char *name, int elementary)
{
    if (elementary) {
        if (!strcmp(name, "flac")) return 1;
        if (!strcmp(name, "mp3")) return 2;
        if (!strcmp(name, "aac")) return 3;
        if (!strcmp(name, "ac3") || !strcmp(name, "eac3")) return 4;
    } else {
        if (!strcmp(name, "matroska,webm")) return 5;
        if (!strcmp(name, "mov,mp4,m4a,3gp,3g2,mj2")) return 6;
        if (!strcmp(name, "ogg")) return 7;
        if (!strcmp(name, "mpegts")) return 8;
    }
    return 0;
}

/* The format the opener selects from a whole input: av_probe_input_buffer2 over a
 * non-seekable stream that ends with the input, at the default probe size. */
static int reader_probe_open(const ReaderProbe *probe, const AVInputFormat **format)
{
    ReaderSource source = {probe->padded, (size_t)probe->data.buf_size, 0};
    uint8_t *buffer = av_malloc(4096);
    if (!buffer)
        return AVERROR(ENOMEM);
    AVIOContext *io = avio_alloc_context(buffer, 4096, 0, &source, reader_read, NULL, NULL);
    if (!io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    int score = av_probe_input_buffer2(io, format, "", NULL, 0, 0);
    av_freep(&io->buffer);
    avio_context_free(&io);
    return score;
}

/* A whole input folds the opener's selection. A shorter prefix folds the best-scoring
 * format and whether its score is at most AVPROBE_SCORE_RETRY, at which the public
 * header recommends a retry with a larger probe buffer. */
static int reader_probe(const ViewCase *input, ViewWork *work)
{
    const ReaderProbe *probe = input->prepared;
    const AVInputFormat *format = NULL;
    int score = 0;
    uint64_t retry = 0;
    if (probe->whole) {
        score = reader_probe_open(probe, &format);
        if (score < 0 && score != AVERROR_INVALIDDATA && score != AVERROR_EOF)
            return 2;
    } else {
        format = av_probe_input_format3(&probe->data, 1, &score);
        retry = score <= AVPROBE_SCORE_RETRY;
    }
    const char *name = format && score >= 0 ? format->name : "";
    const uint64_t words[] = {reader_probe_verdict(name, probe->elementary), retry};
    reader_record(work, 3, words, 2, NULL, 0);
    if (work->trace)
        fprintf(stderr, "probe %s name=%s score=%d\n", probe->whole ? "opener" : "prefix",
                name, score);
    return 0;
}

static int reader_probe_prepare(ViewCase *input)
{
    if (input->argc != 2)
        return 2;
    int elementary = !strcmp(input->argv[0], "elementary");
    if (!elementary && strcmp(input->argv[0], "container"))
        return 2;
    size_t length = input->size;
    if (strcmp(input->argv[1], "whole")) {
        const char *text = input->argv[1];
        if (!*text || strspn(text, "0123456789") != strlen(text))
            return 2;
        unsigned long value = strtoul(text, NULL, 10);
        if (value < length)
            length = value;
    }
    if (length > INT_MAX)
        return 2;
    ReaderProbe *probe = calloc(1, sizeof(*probe) + length + AVPROBE_PADDING_SIZE);
    if (!probe)
        return 2;
    memcpy(probe->padded, input->data, length);
    probe->data = (AVProbeData){.filename = "", .buf = probe->padded, .buf_size = (int)length};
    probe->elementary = elementary;
    probe->whole = length == input->size;
    input->name = "media-probe";
    input->prepared = probe;
    input->release = free;
    input->run = reader_probe;
    return 0;
}

static uint32_t reader_ogg_crc(const uint8_t *page, size_t size)
{
    uint32_t crc = 0;
    for (size_t i = 0; i < size; ++i) {
        crc ^= (uint32_t)(i < 22 || i >= 26 ? page[i] : 0) << 24;
        for (int b = 0; b < 8; ++b)
            crc = (crc << 1) ^ ((crc >> 31) ? UINT32_C(0x04c11db7) : 0);
    }
    return crc;
}

static int reader_page_records(ViewWork *work, const uint8_t *data, size_t size)
{
    while (size) {
        if (size < 27 || memcmp(data, "OggS\0", 5))
            return reader_refusal(work);
        size_t segments = data[26], length = 27 + segments;
        if (length > size)
            return reader_refusal(work);
        for (size_t i = 0; i < segments; ++i)
            length += data[27 + i];
        if (length > size)
            return reader_refusal(work);
        uint32_t checksum = AV_RL32(data + 22);
        const uint64_t fields[] = {data[4], data[5], AV_RL64(data + 6), AV_RL32(data + 14),
            AV_RL32(data + 18), checksum, checksum == reader_ogg_crc(data, length), segments};
        reader_record(work, 3, fields, sizeof(fields) / sizeof(*fields), data + 27, segments);
        uint64_t bytes = length;
        reader_record(work, 1, &bytes, 1, data, length);
        data += length;
        size -= length;
    }
    return 0;
}

static int reader_pages(const ViewCase *input, ViewWork *work)
{
    return reader_page_records(work, input->data, input->size);
}

static uint64_t reader_codec(enum AVCodecID codec)
{
    switch (codec) {
    case AV_CODEC_ID_FLAC: return 1;
    case AV_CODEC_ID_MP3: return 2;
    case AV_CODEC_ID_AAC: return 3;
    case AV_CODEC_ID_AC3: return 4;
    case AV_CODEC_ID_EAC3: return 5;
    case AV_CODEC_ID_MP1: return 6;
    case AV_CODEC_ID_MP2: return 7;
    case AV_CODEC_ID_VORBIS: return 8;
    case AV_CODEC_ID_OPUS: return 9;
    case AV_CODEC_ID_THEORA: return 10;
    case AV_CODEC_ID_SPEEX: return 11;
    default: return 0;
    }
}

static int reader_tags(ViewWork *work, const AVDictionary *dict, int elementary);

static int reader_id3_packet(const AVPacket *packet)
{
    const uint8_t *data = packet->data;
    if (packet->size < 10 || memcmp(data, "ID3", 3) ||
        ((data[6] | data[7] | data[8] | data[9]) & 0x80))
        return 0;
    const unsigned size = ((unsigned)data[6] << 21) | ((unsigned)data[7] << 14) |
                          ((unsigned)data[8] << 7) | data[9];
    const unsigned footer = data[3] == 4 && (data[5] & 0x10) ? 10 : 0;
    return (unsigned)packet->size == 10 + size + footer;
}

static int reader_elementary_metadata(const ViewCase *input, ViewWork *work)
{
    ReaderInput reader = {0};
    int status = reader_open(&reader, input, input->argv[0]);
    AVPacket *pkt = av_packet_alloc();
    if (!pkt) {
        reader_close(&reader);
        return 2;
    }
    if (status >= 0) {
        do {
            av_packet_unref(pkt);
            status = av_read_frame(reader.format, pkt);
        } while (status >= 0 && (reader.format->streams[pkt->stream_index]->codecpar->codec_type != AVMEDIA_TYPE_AUDIO ||
                                reader_id3_packet(pkt)));
    }
    int result = 0;
    if (status >= 0) {
        AVStream *stream = reader.format->streams[pkt->stream_index];
        const AVCodecParameters *params = stream->codecpar;
        uint64_t codec = reader_codec(params->codec_id);
        size_t size = 0;
        const uint8_t *skip = av_packet_get_side_data(pkt, AV_PKT_DATA_SKIP_SAMPLES, &size);
        uint64_t duration = UINT64_MAX;
        unsigned rate = codec == 3 ? 0 : (unsigned)params->sample_rate;
        unsigned channels = codec == 3 || codec == 5 ? 0 : (unsigned)params->ch_layout.nb_channels;
        if ((codec == 1 || codec == 2 || codec == 6 || codec == 7) && stream->duration != AV_NOPTS_VALUE && params->sample_rate)
            duration = av_rescale_q(stream->duration, stream->time_base, (AVRational){1, params->sample_rate});
        if (codec == 1 && params->extradata_size == 34) {
            /* RFC 9639 section 8.2: this view describes the stream declarations
             * in STREAMINFO. Packet parsing may replace the context's fields. */
            const uint64_t info = AV_RB64(params->extradata + 10);
            const uint64_t total_samples = info & ((UINT64_C(1) << 36) - 1);
            rate = (unsigned)(info >> 44);
            channels = (unsigned)((info >> 41) & 7u) + 1;
            duration = total_samples ? total_samples : UINT64_MAX;
        }
        const uint64_t fields[] = {
            codec,
            rate,
            channels,
            duration,
            size >= 4 ? AV_RL32(skip) : 0,
            (uint64_t)params->extradata_size
        };
        reader_record(work, 2, fields, sizeof(fields) / sizeof(*fields), params->extradata, (size_t)params->extradata_size);
        result = reader_tags(work, reader.format->metadata, 1);
    } else if (status != AVERROR(ENOMEM)) {
        reader_refusal(work);
    }
    av_packet_free(&pkt);
    reader_close(&reader);
    return status == AVERROR(ENOMEM) ? 2 : result;
}

typedef struct {
    char *key;
    const char *value;
} ReaderTag;

static int reader_tag_order(const void *left, const void *right)
{
    const ReaderTag *a = left, *b = right;
    int order = strcmp(a->key, b->key);
    return order ? order : strcmp(a->value, b->value);
}

static int reader_tag_selected(const char *key)
{
    static const char *const common[] = {"title", "artist", "album_artist", "album",
        "date", "track", "disc", "composer", "copyright", "encoded_by", "publisher",
        "encoder", "comment"};
    for (size_t i = 0; i < sizeof(common) / sizeof(*common); ++i)
        if (!strcmp(key, common[i]))
            return 1;
    return 0;
}

static int reader_tags(ViewWork *work, const AVDictionary *dict, int elementary)
{
    int count = av_dict_count(dict);
    ReaderTag *tags = av_calloc(count ? (size_t)count : 1, sizeof(*tags));
    if (!tags)
        return 2;
    const AVDictionaryEntry *entry = NULL;
    int used = 0, result = 0;
    while ((entry = av_dict_iterate(dict, entry))) {
        char *key = av_strdup(entry->key);
        if (!key) {
            result = 2;
            break;
        }
        for (char *p = key; *p; ++p)
            if (*p >= 'A' && *p <= 'Z')
                *p += 'a' - 'A';
        if (elementary && !reader_tag_selected(key)) {
            av_free(key);
            continue;
        }
        tags[used++] = (ReaderTag){key, entry->value};
    }
    qsort(tags, (size_t)used, sizeof(*tags), reader_tag_order);
    for (int i = 0; !result && i < used; ++i) {
        size_t key = strlen(tags[i].key), value = strlen(tags[i].value);
        uint8_t *joined = av_malloc(key + value + 1);
        if (!joined) {
            result = 2;
            break;
        }
        memcpy(joined, tags[i].key, key);
        memcpy(joined + key, tags[i].value, value);
        const uint64_t fields[] = {key, value};
        reader_record(work, 5, fields, 2, joined, key + value);
        av_free(joined);
    }
    for (int i = 0; i < used; ++i)
        av_free(tags[i].key);
    av_free(tags);
    if (!result) {
        uint64_t entries = (uint64_t)used;
        reader_record(work, 7, &entries, 1, NULL, 0);
    }
    return result;
}

static int reader_laced_length(const uint8_t **data, size_t *size, size_t *length)
{
    *length = 0;
    while (*size) {
        uint8_t part = **data;
        ++*data;
        --*size;
        *length += part;
        if (part != 255)
            return 1;
    }
    return 0;
}

static int reader_config(uint64_t codec, const uint8_t *data, size_t size,
                         const uint8_t **first, size_t *first_size,
                         const uint8_t **setup, size_t *setup_size)
{
    size_t identification, comment;
    if (codec == 8 && size && *data == 2) {
        ++data;
        --size;
        if (!reader_laced_length(&data, &size, &identification) ||
            !reader_laced_length(&data, &size, &comment) ||
            identification + comment > size)
            return 0;
        *first = data;
        *first_size = identification;
        *setup = data + identification + comment;
        *setup_size = size - identification - comment;
    } else if (codec == 10 && size >= 6) {
        identification = AV_RB16(data);
        if (identification > size - 6)
            return 0;
        comment = AV_RB16(data + 2 + identification);
        if (comment > size - 6 - identification)
            return 0;
        *first = data + 2;
        *first_size = identification;
        *setup_size = AV_RB16(data + 4 + identification + comment);
        if (*setup_size != size - 6 - identification - comment)
            return 0;
        *setup = data + 6 + identification + comment;
    }
    return 1;
}

static int reader_image_order(const void *left, const void *right)
{
    const AVPacket *const *a = left, *const *b = right;
    size_t size = (size_t)((*a)->size < (*b)->size ? (*a)->size : (*b)->size);
    int order = memcmp((*a)->data, (*b)->data, size);
    return order ? order : ((*a)->size > (*b)->size) - ((*a)->size < (*b)->size);
}

static int reader_ogg_images(const AVFormatContext *format, ViewWork *work)
{
    const AVPacket **images = av_calloc(format->nb_streams, sizeof(*images));
    if (!images && format->nb_streams)
        return 2;
    size_t count = 0;
    for (unsigned i = 0; i < format->nb_streams; ++i) {
        const AVStream *stream = format->streams[i];
        if ((stream->disposition & AV_DISPOSITION_ATTACHED_PIC) && stream->attached_pic.size > 0)
            images[count++] = &stream->attached_pic;
    }
    if (count)
        qsort(images, count, sizeof(*images), reader_image_order);
    for (size_t i = 0; i < count; ++i) {
        uint64_t size = (uint64_t)images[i]->size;
        reader_record(work, 6, &size, 1, images[i]->data, (size_t)size);
    }
    av_free(images);
    return 0;
}

static int reader_ogg_tracks(const ViewCase *input, ViewWork *work)
{
    ReaderInput reader = {0};
    int status = reader_open(&reader, input, "ogg");
    if (status < 0) {
        reader_close(&reader);
        return status == AVERROR(ENOMEM) ? 2 : reader_refusal(work);
    }
    int result = 0;
    unsigned streams = 0;
    for (unsigned i = 0; !result && i < reader.format->nb_streams; ++i) {
        AVStream *stream = reader.format->streams[i];
        const AVCodecParameters *params = stream->codecpar;
        if (stream->disposition & AV_DISPOSITION_ATTACHED_PIC)
            continue;
        ++streams;
        uint64_t codec = reader_codec(params->codec_id);
        const char *tag;
        switch (codec) {
        case 1: tag = "\x7f" "FLAC"; break;
        case 8: tag = "\x01" "vorbis"; break;
        case 9: tag = "OpusHead"; break;
        case 10: tag = "\x80" "theora"; break;
        case 11: tag = "Speex   "; break;
        default: tag = "";
        }
        const uint8_t *first = params->extradata, *setup = NULL;
        size_t first_size = (size_t)params->extradata_size, setup_size = 0;
        if (!reader_config(codec, first, first_size, &first, &first_size, &setup, &setup_size))
            return reader_close(&reader), reader_refusal(work);
        uint64_t rate = (uint64_t)params->sample_rate;
        uint64_t channels = (uint64_t)params->ch_layout.nb_channels;
        if (codec == 1 && first_size >= 18) {
            uint32_t bits = AV_RB32(first + 10);
            rate = bits >> 12;
            channels = ((bits >> 9) & 7) + 1;
        }
        uint64_t end = UINT64_MAX;
        if (stream->duration != AV_NOPTS_VALUE) {
            int64_t start = stream->start_time == AV_NOPTS_VALUE ? 0 : stream->start_time;
            int64_t ticks = stream->duration + start;
            if (codec == 9 && first_size >= 12)
                ticks -= AV_RL16(first + 10);
            end = (uint64_t)ticks;
        }
        const uint64_t fields[] = {codec, rate, channels,
            codec == 10 ? (uint64_t)stream->time_base.den : 0,
            codec == 10 ? (uint64_t)stream->time_base.num : 0,
            end, 0, first_size != 0, strlen(tag)};
        reader_record(work, 2, fields, sizeof(fields) / sizeof(*fields), (const uint8_t *)tag, strlen(tag));
        const uint64_t first_len = first_size, setup_len = setup_size;
        reader_record(work, 3, &first_len, 1, first, first_size);
        reader_record(work, 3, &setup_len, 1, setup, setup_size);
        result = reader_tags(work, stream->metadata, 0);
    }
    if (!result && streams)
        result = reader_ogg_images(reader.format, work);
    reader_close(&reader);
    return streams ? result : reader_refusal(work);
}

static int codec_readers_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "media-probe-"))
        return reader_probe_prepare(input);
    if (!strcmp(input->prefix, "ogg-pages-") &&
        (input->argc == 0 || (input->argc == 1 && !strcmp(input->argv[0], "wire")))) {
        input->name = "ogg-pages";
        input->run = reader_pages;
        return 0;
    }
    if (!strcmp(input->prefix, "ogg-tracks-") && input->argc == 0) {
        input->name = "ogg-tracks";
        input->run = reader_ogg_tracks;
        return 0;
    }
    if (!strcmp(input->prefix, "elementary-metadata-") && input->argc == 1 &&
        (!strcmp(input->argv[0], "flac") || !strcmp(input->argv[0], "mp3") ||
         !strcmp(input->argv[0], "aac") || !strcmp(input->argv[0], "ac3") ||
         !strcmp(input->argv[0], "eac3"))) {
        input->name = "elementary-metadata";
        input->run = reader_elementary_metadata;
        return 0;
    }
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
    return 2;
}
