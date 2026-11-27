/* Compare lifts whose anchor chooses the pinned demuxer's clock interval. */
static int transport_bounded_time(const ViewCase *input, ViewWork *work)
{
    int pid = atoi(input->argv[1]);
    int has_dts = !strcmp(input->argv[0], "ref-au");
    TransportViewInput source;
    int status = transport_view_open(&source, input, 32768, "mpegts");
    AVPacket *packet = av_packet_alloc();
    if (status >= 0 && !packet)
        status = AVERROR(ENOMEM);
    if (status >= 0) {
        while ((status = av_read_frame(source.format, packet)) >= 0) {
            const AVStream *stream = source.format->streams[packet->stream_index];
            if (stream->id == pid) {
                if (stream->time_base.num != 1 || stream->time_base.den != 90000 ||
                    packet->pts == AV_NOPTS_VALUE || (has_dts && packet->dts == AV_NOPTS_VALUE)) {
                    status = AVERROR_INVALIDDATA;
                    break;
                }
                const uint64_t pts[] = {(uint64_t)pid, 0, (uint64_t)packet->pts};
                transport_view_row(work, 3, pts, 3, NULL, 0);
                if (has_dts) {
                    const uint64_t dts[] = {(uint64_t)pid, 1, (uint64_t)packet->dts};
                    transport_view_row(work, 3, dts, 3, NULL, 0);
                }
            }
            av_packet_unref(packet);
        }
    }
    if (status == AVERROR_EOF)
        transport_view_row(work, 7, NULL, 0, NULL, 0);
    av_packet_free(&packet);
    transport_view_close(&source);
    return status == AVERROR_EOF ? 0 : 2;
}


/* Share the same public AVIO entity protocol as the access-unit reference. */
#include <limits.h>
#include <libavcodec/ac3_parser.h>

static void transport_reader_quote(const char *text)
{
    putchar('"');
    for (const unsigned char *byte = (const unsigned char *)text; byte && *byte; ++byte) {
        if (*byte < 32)
            printf("\\u%04x", (unsigned)*byte);
        else {
            if (*byte == '"' || *byte == '\\')
                putchar('\\');
            putchar(*byte);
        }
    }
    putchar('"');
}
#define quote transport_reader_quote
#define now_ns view_now
#include "reference_ts.h"
#undef now_ns
#undef quote

typedef struct {
    uint64_t epoch, elementary;
} TransportReaderTrack;

typedef struct {
    size_t offset;
    uint64_t epoch;
    unsigned pid, type;
} TransportRouteEpoch;

typedef struct {
    TransportRouteEpoch entries[16];
    size_t count;
} TransportRouteEpochs;

static int transport_reader_section(const uint8_t *ts, const uint8_t **section, size_t *size)
{
    if (!(ts[1] & 64) || !(ts[3] & 16))
        return 0;
    const size_t payload = ts[3] & 32 ? 5u + ts[4] : 4u;
    if (payload >= 188 || payload + 1u + ts[payload] > 185)
        return AVERROR_INVALIDDATA;
    const size_t first = payload + 1u + ts[payload];
    const uint8_t *p = ts + first;
    const size_t length = 3u + ((size_t)(p[1] & 15) << 8) + p[2];
    if (length < 12 || length > 188 - first || !(p[1] & 0x80) || p[6] || p[7])
        return AVERROR_INVALIDDATA;
    uint32_t crc = UINT32_MAX;
    for (size_t i = 0; i < length; ++i) {
        crc ^= (uint32_t)p[i] << 24;
        for (unsigned bit = 0; bit < 8; ++bit)
            crc = (crc << 1) ^ ((crc >> 31) ? UINT32_C(0x04c11db7) : 0);
    }
    if (crc)
        return AVERROR_INVALIDDATA;
    if (!(p[5] & 1))
        return 0;
    *section = p;
    *size = length;
    return 1;
}

/* These controls have one route and wholly contained PAT and PMT sections.
 * Count source routing transitions independently of when AVProgram publishes
 * its most recent PMT version. */
static int transport_reader_route_epochs(const TsEntities *source, size_t width,
                                         unsigned program, TransportRouteEpochs *epochs)
{
    uint64_t epoch = 0;
    unsigned pmt = UINT_MAX, tsid = UINT_MAX, version = UINT_MAX;
    const uint8_t *last_map = NULL;
    size_t last_size = 0;
    for (size_t i = 0; i < source->count; ++i) {
        const TsEntity *entity = &source->entities[i];
        if (entity->discontinuity) {
            ++epoch;
            pmt = tsid = version = UINT_MAX;
            last_map = NULL;
        }
        if (entity->size % width)
            return AVERROR_INVALIDDATA;
        for (size_t at = 0; at < entity->size; at += width) {
            const uint8_t *ts = entity->bytes + at + width - 188;
            unsigned pid = ((unsigned)(ts[1] & 31) << 8) | ts[2];
            if (ts[0] != 0x47)
                return AVERROR_INVALIDDATA;
            if (pid && pid != pmt)
                continue;
            const uint8_t *section;
            size_t size;
            int status = transport_reader_section(ts, &section, &size);
            if (status < 0)
                return status;
            if (!status)
                continue;
            unsigned extension = ((unsigned)section[3] << 8) | section[4];
            if (pid == 0) {
                if (section[0] || size != 16 ||
                    (((unsigned)section[8] << 8) | section[9]) != program)
                    return AVERROR_INVALIDDATA;
                unsigned announced = ((unsigned)(section[10] & 31) << 8) | section[11];
                if (announced != pmt || extension != tsid) {
                    ++epoch;
                    pmt = announced;
                    tsid = extension;
                    version = UINT_MAX;
                    last_map = NULL;
                }
                continue;
            }
            if (section[0] != 2 || extension != program || size < 21)
                return AVERROR_INVALIDDATA;
            if (last_map && size == last_size && !memcmp(section, last_map, size))
                continue;
            unsigned current = (section[5] >> 1) & 31;
            size_t es = 12u + (((size_t)section[10] & 15) << 8) + section[11];
            if (current == version || es + 5 > size - 4 ||
                es + 5 + (((size_t)section[es + 3] & 15) << 8) + section[es + 4] != size - 4 ||
                epochs->count == sizeof(epochs->entries) / sizeof(*epochs->entries))
                return AVERROR_INVALIDDATA;
            unsigned track = ((unsigned)(section[es + 1] & 31) << 8) | section[es + 2];
            epochs->entries[epochs->count++] = (TransportRouteEpoch){
                entity->base + at, ++epoch, track, section[es]};
            last_map = section;
            last_size = size;
            version = current;
        }
    }
    return epochs->count ? 0 : AVERROR_INVALIDDATA;
}

static uint64_t transport_reader_epoch_at(const TransportRouteEpochs *epochs, int64_t position,
                                          const AVStream *stream, uint64_t previous)
{
    if (position < 0)
        return previous;
    for (size_t index = epochs->count; index; --index) {
        const TransportRouteEpoch *entry = &epochs->entries[index - 1];
        if (entry->offset <= (uint64_t)position)
            return entry->pid == (unsigned)stream->id && entry->type == stream->codecpar->codec_tag
                ? entry->epoch : 0;
    }
    return 0;
}

static uint64_t transport_reader_codec(enum AVCodecID codec)
{
    switch (codec) {
    case AV_CODEC_ID_MPEG2VIDEO: return 1;
    case AV_CODEC_ID_H264: return 2;
    case AV_CODEC_ID_HEVC: return 3;
    case AV_CODEC_ID_AAC: return 4;
    case AV_CODEC_ID_AAC_LATM: return 5;
    case AV_CODEC_ID_AC3: return 6;
    case AV_CODEC_ID_EAC3: return 7;
    case AV_CODEC_ID_DTS: return 8;
    case AV_CODEC_ID_MP1: case AV_CODEC_ID_MP2: case AV_CODEC_ID_MP3: return 9;
    case AV_CODEC_ID_DVB_SUBTITLE: return 10;
    case AV_CODEC_ID_DVB_TELETEXT: return 11;
    default: return 0;
    }
}

static void transport_reader_track(ViewWork *work, const AVStream *stream,
                                   TransportReaderTrack *state, uint64_t epoch)
{
    if (state->epoch == epoch)
        return;
    state->epoch = epoch;
    const AVCodecParameters *parameters = stream->codecpar;
    uint64_t kind = 0;
    switch (parameters->codec_type) {
    case AVMEDIA_TYPE_VIDEO: kind = 1; break;
    case AVMEDIA_TYPE_AUDIO: kind = 2; break;
    case AVMEDIA_TYPE_SUBTITLE: kind = 3; break;
    case AVMEDIA_TYPE_DATA: kind = 4; break;
    default: break;
    }
    const uint64_t words[] = {(uint64_t)stream->id, kind,
        transport_reader_codec(parameters->codec_id), parameters->codec_tag,
        epoch, (uint64_t)parameters->extradata_size};
    transport_view_row(work, 2, words, 6, parameters->extradata,
                       (size_t)parameters->extradata_size);
}

static int transport_reader_start_code(const uint8_t *bytes, size_t size)
{
    for (size_t at = 2; at < size; ++at)
        if (!bytes[at - 2] && !bytes[at - 1] && bytes[at] == 1)
            return 1;
    return 0;
}

/* The segment data of the original PES that starts at the packet's TS188
 * carrier: its payload after data_identifier and subtitle_stream_id (EN 300 743
 * §7.1), up to PES_packet_length or the PID's next unit start. Returns its size,
 * or -1 when the packet does not start such a PES in a single entity. */
static int64_t transport_reader_dvb_original(const TsEntities *source, unsigned pid,
                                             int64_t position, uint8_t *out, size_t capacity)
{
    if (source->count != 1 || position < 0 || (uint64_t)position % 188)
        return -1;
    const uint8_t *data = source->entities[0].bytes;
    const size_t end = source->entities[0].size;
    size_t used = 0, header = 9, length = 0, total = 0;
    uint8_t fixed[9];
    for (size_t at = (size_t)position; end - at >= 188 && at < end; at += 188) {
        const uint8_t *ts = data + at;
        if (ts[0] != 0x47)
            return -1;
        if ((((unsigned)ts[1] & 31) << 8 | ts[2]) != pid || !(ts[3] & 16))
            continue;
        if (!!(ts[1] & 64) != (at == (size_t)position)) {
            if (at != (size_t)position)
                break;
            return -1;
        }
        size_t start = ts[3] & 32 ? 5u + ts[4] : 4u;
        for (; start < 188; ++start, ++total) {
            if (total < 9) {
                fixed[total] = ts[start];
                if (total == 8) {
                    if (fixed[0] || fixed[1] || fixed[2] != 1)
                        return -1;
                    header = 9u + fixed[8];
                    length = ((size_t)fixed[4] << 8 | fixed[5]) + 6u;
                }
                continue;
            }
            if (length > 9 && total >= length)
                break;
            if (total < header + 2)
                continue;
            if (used == capacity)
                return -1;
            out[used++] = ts[start];
        }
        if (length > 9 && total >= length)
            break;
    }
    return total >= header + 2 ? (int64_t)used : -1;
}

/* Count the complete EN 300 743 §7.2 segments and the unfinished tail. The segments
 * are those of the original PES at the packet's position, which the reader reports;
 * the demuxer's packet holds the complete prefix. */
static void transport_reader_dvb(ViewWork *work, const TsEntities *source, const AVStream *stream,
                                 const AVPacket *packet)
{
    static uint8_t original[65536];
    const int64_t found = transport_reader_dvb_original(source, (unsigned)stream->id, packet->pos,
                                                        original, sizeof(original));
    const uint8_t *bytes = found >= 0 ? original : packet->data;
    const size_t size = found >= 0 ? (size_t)found : (size_t)packet->size;
    uint64_t count = 0;
    size_t at = 0, tail = 0;
    while (at < size && bytes[at] == 15) {
        if (size - at < 6) {
            tail = size - at;
            break;
        }
        size_t extent = 6u + ((unsigned)bytes[at + 4] << 8) + bytes[at + 5];
        if (extent > size - at) {
            tail = size - at;
            break;
        }
        ++count;
        at += extent;
    }
    const uint64_t words[] = {10, count, tail != 0, tail};
    transport_view_row(work, 4, words, 4, NULL, 0);
}

static int transport_reader_ac3(ViewWork *work, const AVPacket *packet,
                                const AVCodecParameters *parameters)
{
    AVCodecParserContext *parser = av_parser_init(AV_CODEC_ID_AC3);
    AVCodecContext *context = avcodec_alloc_context3(NULL);
    int status = AVERROR(ENOMEM), consumed = 0, produced = 0;
    if (context && parser) {
        context->codec_id = parameters->codec_id;
        context->codec_type = AVMEDIA_TYPE_AUDIO;
        while (consumed < packet->size) {
            uint8_t *output = NULL;
            int length = 0;
            status = av_parser_parse2(parser, context, &output, &length,
                packet->data + consumed, packet->size - consumed,
                AV_NOPTS_VALUE, AV_NOPTS_VALUE, packet->pos);
            if (status < 0 || (!status && !length))
                goto done;
            consumed += status;
            if (length) {
                const uint64_t row[] = {6, (uint64_t)length};
                transport_view_row(work, 4, row, 2, NULL, 0);
                ++produced;
            }
        }
        uint8_t *output = NULL;
        int length = 0;
        status = av_parser_parse2(parser, context, &output, &length, NULL, 0,
                                  AV_NOPTS_VALUE, AV_NOPTS_VALUE, -1);
        if (status >= 0 && length) {
            uint8_t bitstream_id;
            uint16_t declared;
            if (av_ac3_parse_header(output, (size_t)length, &bitstream_id, &declared) >= 0 &&
                length < declared) {
                const uint64_t row[] = {8, (uint64_t)length, declared};
                transport_view_row(work, 4, row, 3, NULL, 0);
            } else {
                const uint64_t row[] = {6, (uint64_t)length};
                transport_view_row(work, 4, row, 2, NULL, 0);
            }
            ++produced;
        }
    }
done:
    if (parser)
        av_parser_close(parser);
    avcodec_free_context(&context);
    return status < 0 ? status : produced;
}

/* A wire projection for the carrier that precedes a counter diagnostic.
 * Demux packet bytes and positions are compared independently of this record. */
static int transport_reader_same_carrier(const uint8_t *a, const uint8_t *b)
{
    if ((a[3] & 0x20) && a[4] >= 7 && (a[5] & 0x10) &&
        (a[10] & 0x7e) == 0x7e && (b[10] & 0x7e) == 0x7e &&
        ((a[10] & 1) * 256 + a[11]) < 300 && ((b[10] & 1) * 256 + b[11]) < 300)
        return !memcmp(a, b, 6) && !memcmp(a + 12, b + 12, 188 - 12);
    return !memcmp(a, b, 188);
}

static int transport_reader_evidence(ViewWork *work, const TsEntities *entities,
                                     size_t size, unsigned pid)
{
    if (entities->count != 1)
        return 0;
    const uint8_t *data = entities->entities[0].bytes;
    const size_t end = entities->entities[0].size, prefix = size == 192 ? 4 : 0;
    const uint8_t *previous = NULL;
    int saw_copy = 0;
    size_t offsets[16], count = 0;
    for (size_t offset = 0; offset + size <= end; offset += size) {
        const uint8_t *carrier = data + offset + prefix;
        if (carrier[0] != 0x47 || (((unsigned)carrier[1] & 31) << 8 | carrier[2]) != pid ||
            !(carrier[3] & 16))
            continue;
        int counter = carrier[3] & 15;
        if (previous && counter != ((previous[3] & 15) + 1) % 16) {
            const int duplicate = transport_reader_same_carrier(previous, carrier);
            const int discontinuity = (carrier[3] & 0x20) && carrier[4] &&
                                      (carrier[5] & 0x80);
            /* A signalled discontinuity whose carrier also codes a PCR starts a new
             * system time base (H.222.0 §2.4.3.5): the counter observation and the
             * time-base change both admit the continuity change. */
            const int timeline = discontinuity && (carrier[5] & 0x10);
            /* An unpermitted change contributes the discontinuity observation
             * and its confirming source diagnostic at the same carrier. */
            size_t copies = duplicate ? 1u + (unsigned)saw_copy
                                      : 2u - (unsigned)(discontinuity && !timeline);
            if (copies > sizeof(offsets) / sizeof(*offsets) - count)
                return AVERROR_INVALIDDATA;
            offsets[count++] = offset;
            if (copies == 2)
                offsets[count++] = offset;
            saw_copy = duplicate;
        } else {
            saw_copy = 0;
        }
        previous = carrier;
    }
    if (count) {
        const uint64_t terminal[] = {end, count};
        transport_view_row(work, 6, terminal, 2, NULL, 0);
        for (size_t i = 0; i != count; ++i) {
            const uint64_t carrier[] = {offsets[i], size};
            transport_view_row(work, 6, carrier, 2, data + offsets[i], size);
        }
    }
    return 0;
}

static const AVProgram *transport_reader_program(const AVFormatContext *format, int id)
{
    for (unsigned i = 0; i < format->nb_programs; ++i)
        if (format->programs[i]->program_num == id)
            return format->programs[i];
    return NULL;
}

static int transport_reader_views(const ViewCase *view, ViewWork *work)
{
    const int program = atoi(view->argv[1]);
    const size_t carrier_size = (size_t)atoi(view->argv[0]);
    const int container = !strcmp(view->prefix, "ts-reader-container-");
    const TsEntities *source = view->prepared;
    TransportRouteEpochs route_epochs = {0};
    int source_epochs = 0;
    if (!container &&
        transport_reader_route_epochs(source, carrier_size, (unsigned)program, &route_epochs) >= 0) {
        for (size_t i = 1; i < source->count; ++i)
            source_epochs |= source->entities[i].discontinuity;
        for (size_t i = 1; i < route_epochs.count; ++i)
            source_epochs |= route_epochs.entries[i].pid != route_epochs.entries[i - 1].pid ||
                             route_epochs.entries[i].type != route_epochs.entries[i - 1].type;
    }
    TsEntityInput input = {source, 0, 0, ts_next_boundary(source, 1), 0};
    AVFormatContext *format = avformat_alloc_context();
    AVIOContext *io = NULL;
    AVPacket *packet = NULL;
    TransportReaderTrack *tracks = av_calloc(8192, sizeof(*tracks));
    uint8_t *buffer = av_malloc(TS_AVIO_BYTES);
    int status = AVERROR(ENOMEM), pmt_version = -1, opaque_pid = -1;
    uint64_t epoch = 1;
    if (!format || !buffer || !tracks) {
        av_free(buffer);
        goto done;
    }
    io = avio_alloc_context(buffer, TS_AVIO_BYTES, 0, &input, ts_entity_read,
                            NULL, ts_entity_seek);
    if (!io) {
        av_free(buffer);
        goto done;
    }
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO | AVFMT_FLAG_NOFILLIN;
    status = avformat_open_input(&format, NULL, av_find_input_format("mpegts"), NULL);
    if (status < 0)
        goto done;
    packet = av_packet_alloc();
    if (!packet) {
        status = AVERROR(ENOMEM);
        goto done;
    }
    for (;;) {
        status = av_read_frame(format, packet);
        if (status == AVERROR(EAGAIN) && input.waiting) {
            status = ts_entity_resume(format, &io, &input);
            if (status < 0)
                break;
            continue;
        }
        if (status < 0)
            break;
        const AVStream *stream = format->streams[packet->stream_index];
        const AVProgram *selected = transport_reader_program(format, program);
        if (selected && ts_in_program(format, packet->stream_index, program) && stream->id >= 0 && stream->id < 8192) {
            TransportReaderTrack *track = &tracks[stream->id];
            if (pmt_version != selected->pmt_version) {
                ++epoch;
                pmt_version = selected->pmt_version;
            }
            if (!container) {
                uint64_t track_epoch = source_epochs
                    ? transport_reader_epoch_at(&route_epochs, packet->pos, stream, track->epoch)
                    : epoch;
                if (!track_epoch) {
                    status = AVERROR_INVALIDDATA;
                    break;
                }
                transport_reader_track(work, stream, track, track_epoch);
            }
            const uint64_t fields[] = {
                (uint64_t)packet->stream_index, (uint64_t)stream->id,
                (uint64_t)packet->pts, (uint64_t)packet->dts, (uint64_t)packet->duration,
                (uint64_t)packet->pos, (uint64_t)packet->flags, (uint64_t)packet->size,
                (uint64_t)stream->time_base.num, (uint64_t)stream->time_base.den,
            };
            transport_view_row(work, 1, fields, 10, packet->data, (size_t)packet->size);
            for (int i = 0; i < packet->side_data_elems; ++i) {
                const AVPacketSideData *side = &packet->side_data[i];
                const uint64_t words[] = {side->type == AV_PKT_DATA_MPEGTS_STREAM_ID ? 1 :
                    2u + (uint64_t)side->type, side->size};
                transport_view_row(work, 3, words, 2, side->data, side->size);
            }
            uint64_t codec = transport_reader_codec(stream->codecpar->codec_id);
            if (!container && codec == 10) {
                transport_reader_dvb(work, source, stream, packet);
            } else if (!container && (codec == 6 || codec == 7)) {
                int encoded = packet->size >= 2 && packet->data[0] == 0x0b && packet->data[1] == 0x77;
                if (encoded) {
                    status = transport_reader_ac3(work, packet, stream->codecpar);
                    if (status < 0)
                        break;
                } else {
                    uint64_t carriage = stream->codecpar->codec_tag == 0x81 ? 2 :
                        stream->codecpar->codec_tag == 0x87 ? 3 : codec == 7 ? 1 : 0;
                    const uint64_t words[] = {7, carriage, track->elementary,
                        track->elementary + (uint64_t)packet->size};
                    transport_view_row(work, 4, words, 4, NULL, 0);
                }
            } else if ((codec == 2 || codec == 3) &&
                       !transport_reader_start_code(packet->data, (size_t)packet->size)) {
                if (container) {
                    opaque_pid = stream->id;
                } else {
                    const uint64_t words[] = {2, track->elementary,
                        track->elementary + (uint64_t)packet->size};
                    transport_view_row(work, 4, words, 3, NULL, 0);
                }
            }
            track->elementary += (uint64_t)packet->size;
        }
        av_packet_unref(packet);
    }
    if (status == AVERROR_EOF && input.at == source->size && input.boundary == source->count) {
        const AVProgram *selected = transport_reader_program(format, program);
        if (!container && selected) {
            if (pmt_version != selected->pmt_version)
                ++epoch;
            for (unsigned index = 0; index < selected->nb_stream_indexes; ++index) {
                const AVStream *stream = format->streams[selected->stream_index[index]];
                if (stream->id >= 0 && stream->id < 8192) {
                    TransportReaderTrack *track = &tracks[stream->id];
                    uint64_t track_epoch = source_epochs
                        ? transport_reader_epoch_at(&route_epochs, (int64_t)source->size,
                                                    stream, track->epoch)
                        : epoch;
                    if (!track_epoch) {
                        status = AVERROR_INVALIDDATA;
                        goto done;
                    }
                    transport_reader_track(work, stream, track, track_epoch);
                }
            }
        }
        if (opaque_pid >= 0) {
            status = transport_reader_evidence(work, source, carrier_size, (unsigned)opaque_pid);
            if (status < 0)
                goto done;
        }
        transport_view_row(work, 7, NULL, 0, NULL, 0);
        status = 0;
    } else if (status == AVERROR_EOF) {
        status = AVERROR_INVALIDDATA;
    }
done:
    av_free(tracks);
    av_packet_free(&packet);
    avformat_close_input(&format);
    if (io) {
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    if (status < 0) {
        if (status == AVERROR(ENOMEM))
            return 2;
        transport_view_row(work, 8, NULL, 0, NULL, 0);
    }
    return 0;
}

static void transport_reader_release(void *opaque)
{
    TsEntities *source = opaque;
    ts_entities_free(source);
    free(source);
}

static int transport_reader_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "ts-bounded-time-")) {
        if (input->argc != 9 ||
            (strcmp(input->argv[0], "ref-pes") && strcmp(input->argv[0], "ref-au")))
            return 2;
        input->name = "ts-bounded-time";
        input->run = transport_bounded_time;
        return 0;
    }
    if (!strcmp(input->prefix, "ts-epoch-index-seek-"))
        return 2; /* The contract uses the original ffmpeg-ts-seek binary. */
    if (input->argc < 2 || input->argc > 130 || input->size > 64u * 1024u * 1024u ||
        (strcmp(input->argv[0], "188") && strcmp(input->argv[0], "192")))
        return 2;
    (void)ts_main; /* The transitively included legacy entry stays private. */
    TsEntities *source = calloc(1, sizeof(*source));
    if (!source)
        return 2;
    source->entities = calloc((size_t)input->argc, sizeof(*source->entities));
    if (!source->entities) {
        free(source);
        return 2;
    }
    source->size = input->size;
    source->entities[source->count++] = (TsEntity){input->data, NULL, input->size, 0, 0};
    input->prepared = source;
    input->release = transport_reader_release;
    int at = 2;
    if (at < input->argc && (!strcmp(input->argv[at], "--dvb") || !strcmp(input->argv[at], "--atsc")))
        ++at;
    while (at < input->argc) {
        int discontinuity = !strcmp(input->argv[at], "--discontinuity");
        at += discontinuity;
        if (at == input->argc || source->count == 64)
            return 2;
        size_t size = 0;
        uint8_t *bytes = view_load(input->argv[at++], &size);
        if (!bytes || size > 64u * 1024u * 1024u - source->size) {
            free(bytes);
            return 2;
        }
        source->entities[source->count++] = (TsEntity){bytes, bytes, size, source->size, discontinuity};
        source->size += size;
    }
    input->name = !strcmp(input->prefix, "ts-reader-container-") ? "ts-reader-container" : "ts-reader-views";
    input->run = transport_reader_views;
    return 0;
}
