/* Read the reference's stream catalogue and forward packets through public AVIO. */
#define now_ns view_now
#include "reference_packet_fold.h"
#undef now_ns
#include <libavutil/intreadwrite.h>
#include <libavutil/crc.h>

typedef struct {
    const char *demuxer;
    int seekable, ignore_edits, transport, raw, matroska;
    uint8_t *entities;
    size_t size, pid_count;
    unsigned pids[64];
} PlaybackSourceCase;

typedef struct {
    const uint8_t *data;
    size_t size;
} CatalogueField;

typedef struct {
    CatalogueField rows;
    uint32_t next, left, value;
} SourceMovRun;

typedef struct {
    CatalogueField chunks, mapping, sizes, sync;
    SourceMovRun decode, composition;
    uint32_t sample, chunk, run, left, sync_index;
    uint64_t offset;
    int64_t dts;
    int width, signed_composition;
} SourceMovSamples;

static int player_ebml_vint(const uint8_t *bytes, size_t end, size_t *at,
                            unsigned maximum, int identifier, uint64_t *value);

/* This walk reads the source fields AVStream does not expose. The native
 * decoder identity, configuration, dispositions and packets still come
 * from the public demux API. */
static int catalogue_element(CatalogueField parent, size_t *at, uint64_t *id,
                              CatalogueField *payload)
{
    uint64_t size;
    if (player_ebml_vint(parent.data, parent.size, at, 4, 1, id) < 0 ||
        player_ebml_vint(parent.data, parent.size, at, 8, 0, &size) < 0)
        return AVERROR_INVALIDDATA;
    if (size == UINT64_MAX && *id == UINT64_C(0x18538067))
        size = parent.size - *at;
    if (size > parent.size - *at)
        return AVERROR_INVALIDDATA;
    *payload = (CatalogueField){parent.data + *at, (size_t)size};
    *at += (size_t)size;
    return 0;
}

static int catalogue_find(CatalogueField parent, uint64_t wanted, CatalogueField *field)
{
    for (size_t at = 0; at < parent.size;) {
        uint64_t id;
        int rc = catalogue_element(parent, &at, &id, field);
        if (rc < 0 || id == wanted)
            return rc;
    }
    return AVERROR(ENOENT);
}

static int catalogue_track(CatalogueField tracks, uint64_t number, CatalogueField *codec,
                            CatalogueField *language)
{
    for (size_t at = 0; at < tracks.size;) {
        uint64_t id;
        CatalogueField entry, field;
        if (catalogue_element(tracks, &at, &id, &entry) < 0)
            return AVERROR_INVALIDDATA;
        if (id != 0xae)
            continue;
        if (catalogue_find(entry, 0xd7, &field) < 0 || !field.size || field.size > 8)
            return AVERROR_INVALIDDATA;
        uint64_t value = 0;
        for (size_t i = 0; i < field.size; ++i)
            value = (value << 8) | field.data[i];
        if (value != number)
            continue;
        if (catalogue_find(entry, 0x86, codec) < 0 || !codec->size)
            return AVERROR_INVALIDDATA;
        int rc = catalogue_find(entry, 0x22b59d, language);
        if (rc == AVERROR(ENOENT))
            rc = catalogue_find(entry, 0x22b59c, language);
        if (rc == AVERROR(ENOENT)) {
            *language = (CatalogueField){(const uint8_t *)"eng", 3};
            return 0;
        }
        return rc;
    }
    return AVERROR_INVALIDDATA;
}

static int catalogue_mov_box(CatalogueField parent, size_t *at, const uint8_t **kind,
                             CatalogueField *payload)
{
    const size_t remain = parent.size - *at;
    const uint8_t *box = parent.data + *at;
    if (remain < 8)
        return AVERROR_INVALIDDATA;
    uint64_t size = AV_RB32(box);
    size_t head = 8;
    if (size == 1) {
        if (remain < 16)
            return AVERROR_INVALIDDATA;
        size = AV_RB64(box + 8);
        head = 16;
    } else if (size == 0) {
        size = remain;
    }
    if (!memcmp(box + 4, "uuid", 4))
        head += 16;
    if (size < head || size > remain)
        return AVERROR_INVALIDDATA;
    *kind = box + 4;
    *payload = (CatalogueField){box + head, (size_t)size - head};
    *at += (size_t)size;
    return 0;
}

static int catalogue_mov_find(CatalogueField parent, const char *wanted, CatalogueField *child)
{
    for (size_t at = 0; at < parent.size;) {
        const uint8_t *kind;
        int rc = catalogue_mov_box(parent, &at, &kind, child);
        if (rc < 0 || !memcmp(kind, wanted, 4))
            return rc;
    }
    return AVERROR(ENOENT);
}

/* The neutral catalogue uses the first stsd entry. Its body is the
 * configuration for a raw or opaque entry; a decoder record is separate. */
static int catalogue_mov_entry(CatalogueField movie, uint64_t id, CatalogueField *tag,
                               CatalogueField *entry, CatalogueField *table)
{
    for (size_t at = 0; at < movie.size;) {
        const uint8_t *kind;
        CatalogueField track, header, media, info, descriptions;
        int rc = catalogue_mov_box(movie, &at, &kind, &track);
        if (rc < 0)
            return rc;
        if (memcmp(kind, "trak", 4))
            continue;
        if (catalogue_mov_find(track, "tkhd", &header) < 0 || header.size < 16 ||
            header.data[0] > 1)
            return AVERROR_INVALIDDATA;
        size_t track_id = header.data[0] ? 20 : 12;
        if (header.size < track_id + 4)
            return AVERROR_INVALIDDATA;
        if (AV_RB32(header.data + track_id) != id)
            continue;
        if (catalogue_mov_find(track, "mdia", &media) < 0 ||
            catalogue_mov_find(media, "minf", &info) < 0 ||
            catalogue_mov_find(info, "stbl", table) < 0 ||
            catalogue_mov_find(*table, "stsd", &descriptions) < 0 || descriptions.size < 8)
            return AVERROR_INVALIDDATA;
        if (!AV_RB32(descriptions.data + 4))
            return 0;
        size_t first = 8;
        rc = catalogue_mov_box(descriptions, &first, &kind, entry);
        if (rc < 0)
            return rc;
        *tag = (CatalogueField){kind, 4};
        return 0;
    }
    return AVERROR(ENOENT);
}

static int source_mov_rows(CatalogueField table, const char *kind, size_t width,
                           CatalogueField *rows)
{
    CatalogueField box;
    int rc = catalogue_mov_find(table, kind, &box);
    if (rc < 0)
        return rc;
    if (box.size < 8 || (box.size - 8) / width != AV_RB32(box.data + 4) ||
        (box.size - 8) % width)
        return AVERROR_INVALIDDATA;
    *rows = (CatalogueField){box.data + 8, box.size - 8};
    return 0;
}

static void source_mov_regular(CatalogueField table, SourceMovSamples *samples)
{
    int width = 4;
    int rc = source_mov_rows(table, "stco", 4, &samples->chunks);
    if (rc == AVERROR(ENOENT)) {
        width = 8;
        rc = source_mov_rows(table, "co64", 8, &samples->chunks);
    }
    if (rc < 0 || source_mov_rows(table, "stsc", 12, &samples->mapping) < 0 ||
        catalogue_mov_find(table, "stsz", &samples->sizes) < 0 || samples->sizes.size < 12)
        return;
    const uint32_t fixed = AV_RB32(samples->sizes.data + 4);
    if (!fixed && ((samples->sizes.size - 12) % 4 ||
                   (samples->sizes.size - 12) / 4 != AV_RB32(samples->sizes.data + 8)))
        return;
    rc = source_mov_rows(table, "stss", 4, &samples->sync);
    if (rc == 0 || rc == AVERROR(ENOENT))
        samples->width = width;
    if (source_mov_rows(table, "stts", 8, &samples->decode.rows) < 0)
        return;
    rc = source_mov_rows(table, "ctts", 8, &samples->composition.rows);
    if (rc == 0) {
        const uint8_t version = samples->composition.rows.data[-8];
        if (version > 1)
            samples->decode.rows = (CatalogueField){0};
        samples->signed_composition = version == 1;
    } else if (rc != AVERROR(ENOENT)) {
        samples->decode.rows = (CatalogueField){0};
    }
}

static int source_mov_value(SourceMovRun *run, uint32_t *value)
{
    if (!run->left) {
        if (run->next >= run->rows.size / 8)
            return AVERROR_INVALIDDATA;
        const uint8_t *row = run->rows.data + 8 * (size_t)run->next++;
        run->left = AV_RB32(row);
        run->value = AV_RB32(row + 4);
        if (!run->left)
            return AVERROR_INVALIDDATA;
    }
    --run->left;
    *value = run->value;
    return 0;
}

static void source_mov_timing(SourceMovSamples *s, int64_t *pts, int64_t *dts, int64_t *duration)
{
    uint32_t delta, encoded = 0;
    if (source_mov_value(&s->decode, &delta) < 0 ||
        (s->composition.rows.data && source_mov_value(&s->composition, &encoded) < 0))
        goto unavailable;
    const int64_t composition = s->signed_composition && (encoded & UINT32_C(0x80000000))
        ? (int64_t)encoded - INT64_C(4294967296) : (int64_t)encoded;
    if (s->dts > INT64_MAX - delta || (composition > 0 && s->dts > INT64_MAX - composition))
        goto unavailable;
    *pts = s->dts + composition;
    *dts = s->dts;
    *duration = delta;
    s->dts += delta;
    return;
unavailable:
    s->decode = (SourceMovRun){0};
}

/* Project table clocks and sync after matching the exact occurrence to the
 * public packet's position and extent. Preserve independent demux flags. */
static int source_mov_sample(SourceMovSamples *s, const AVPacket *packet, CatalogueField file,
                             int64_t *pts, int64_t *dts, int64_t *duration, uint64_t *flags)
{
    if (!s->width || s->sample >= AV_RB32(s->sizes.data + 8))
        return 0;
    if (!s->left) {
        const size_t width = (size_t)s->width;
        if (s->chunk >= s->chunks.size / width || s->run >= s->mapping.size / 12)
            goto unmatched;
        if ((size_t)s->run + 1 < s->mapping.size / 12 &&
            AV_RB32(s->mapping.data + 12 * ((size_t)s->run + 1)) == s->chunk + 1)
            ++s->run;
        s->left = AV_RB32(s->mapping.data + 12 * (size_t)s->run + 4);
        if (!s->left || AV_RB32(s->mapping.data + 12 * (size_t)s->run) > s->chunk + 1)
            goto unmatched;
        const uint8_t *offset = s->chunks.data + width * s->chunk++;
        s->offset = width == 4 ? AV_RB32(offset) : AV_RB64(offset);
    }
    uint32_t size = AV_RB32(s->sizes.data + 4);
    if (!size)
        size = AV_RB32(s->sizes.data + 12 + 4 * (size_t)s->sample);
    if (packet->pos < 0 || packet->size < 0 || (uint64_t)packet->pos != s->offset ||
        (uint32_t)packet->size != size || s->offset > file.size || size > file.size - s->offset ||
        (size && memcmp(packet->data, file.data + (size_t)s->offset, size)))
        goto unmatched;
    s->offset += size;
    --s->left;
    ++s->sample;
    source_mov_timing(s, pts, dts, duration);
    int sync = s->sync.data == NULL;
    if (s->sync_index < s->sync.size / 4 &&
        AV_RB32(s->sync.data + 4 * (size_t)s->sync_index) == s->sample) {
        sync = 1;
        ++s->sync_index;
    }
    *flags = (*flags & ~(uint64_t)AV_PKT_FLAG_KEY) | (sync ? AV_PKT_FLAG_KEY : 0);
    return 1;
unmatched:
    s->width = 0;
    return 0;
}

typedef struct {
    uint32_t duration, size, flags;
} SourceMovDefaults;

typedef struct {
    SourceMovDefaults sample;
    uint64_t base;
    uint32_t track;
} SourceMovFragment;

typedef struct {
    int found, timed;
    int64_t pts, dts;
    uint32_t duration, flags;
} SourceMovFragmentSample;

static int source_mov_fragment_header(CatalogueField traf, CatalogueField extensions,
                                      uint64_t moof, uint64_t previous,
                                      SourceMovFragment *fragment)
{
    CatalogueField tfhd;
    if (catalogue_mov_find(traf, "tfhd", &tfhd) < 0 || tfhd.size < 8 || tfhd.data[0])
        return AVERROR_INVALIDDATA;
    const uint32_t mode = AV_RB32(tfhd.data);
    fragment->track = AV_RB32(tfhd.data + 4);
    int found = 0;
    for (size_t at = 0; at < extensions.size;) {
        CatalogueField box;
        const uint8_t *kind;
        if (catalogue_mov_box(extensions, &at, &kind, &box) < 0)
            return AVERROR_INVALIDDATA;
        if (memcmp(kind, "trex", 4))
            continue;
        if (box.size != 24 || AV_RB32(box.data))
            return AVERROR_INVALIDDATA;
        if (AV_RB32(box.data + 4) != fragment->track)
            continue;
        if (found++)
            return AVERROR_INVALIDDATA;
        fragment->sample = (SourceMovDefaults){AV_RB32(box.data + 12),
            AV_RB32(box.data + 16), AV_RB32(box.data + 20)};
    }
    if (!found || (mode & ~UINT32_C(0x03003b)))
        return AVERROR_INVALIDDATA;
    fragment->base = mode & 0x020000 ? moof : previous;
    size_t field = 8;
    if (mode & 1) {
        if (tfhd.size - field < 8)
            return AVERROR_INVALIDDATA;
        fragment->base = AV_RB64(tfhd.data + field);
        field += 8;
    }
    uint32_t *defaults[] = {NULL, &fragment->sample.duration, &fragment->sample.size,
        &fragment->sample.flags};
    const uint32_t masks[] = {2, 8, 16, 32};
    for (size_t i = 0; i < sizeof(masks) / sizeof(*masks); ++i) {
        if (!(mode & masks[i]))
            continue;
        if (tfhd.size - field < 4)
            return AVERROR_INVALIDDATA;
        if (defaults[i])
            *defaults[i] = AV_RB32(tfhd.data + field);
        field += 4;
    }
    return tfhd.size == field ? 0 : AVERROR_INVALIDDATA;
}

static int64_t source_mov_signed(uint32_t value)
{
    return (int64_t)value - (value & UINT32_C(0x80000000) ? INT64_C(4294967296) : 0);
}

static int source_mov_fragment_run(CatalogueField trun, const SourceMovFragment *fragment,
                                   uint64_t *offset, int64_t *clock, int timed,
                                   size_t extent,
                                   const AVPacket *packet, SourceMovFragmentSample *match)
{
    if (trun.size < 8 || trun.data[0] > 1)
        return AVERROR_INVALIDDATA;
    const uint32_t mode = AV_RB32(trun.data) & 0xffffff;
    const uint32_t count = AV_RB32(trun.data + 4);
    if ((mode & ~UINT32_C(0xf05)) || (mode & 0x404) == 0x404)
        return AVERROR_INVALIDDATA;
    size_t field = 8;
    if (mode & 1) {
        if (trun.size - field < 4)
            return AVERROR_INVALIDDATA;
        const int64_t relative = source_mov_signed(AV_RB32(trun.data + field));
        if ((relative < 0 && (uint64_t)-relative > fragment->base) ||
            (relative >= 0 && (uint64_t)relative > UINT64_MAX - fragment->base))
            return AVERROR_INVALIDDATA;
        *offset = relative < 0 ? fragment->base - (uint64_t)-relative
            : fragment->base + (uint64_t)relative;
        field += 4;
    }
    uint32_t first_flags = fragment->sample.flags;
    if (mode & 4) {
        if (trun.size - field < 4)
            return AVERROR_INVALIDDATA;
        first_flags = AV_RB32(trun.data + field);
        field += 4;
    }
    const size_t width = 4 * (!!(mode & 0x100) + !!(mode & 0x200) +
        !!(mode & 0x400) + !!(mode & 0x800));
    if ((uint64_t)count * width != trun.size - field)
        return AVERROR_INVALIDDATA;
    if (!width && !fragment->sample.size && count > 1)
        return AVERROR_INVALIDDATA;
    /* Fixed rows can refer to a large number of samples without a physical
     * row per sample. Bound iteration by the actual input extent. */
    if (!width && (uint64_t)count * fragment->sample.size > extent)
        return AVERROR_INVALIDDATA;
    for (uint32_t i = 0; i < count; ++i) {
        uint32_t duration = fragment->sample.duration, size = fragment->sample.size;
        uint32_t flags = i ? fragment->sample.flags : first_flags, encoded = 0;
        uint32_t *fields[] = {&duration, &size, &flags, &encoded};
        for (unsigned k = 0; k < sizeof(fields) / sizeof(*fields); ++k)
            if (mode & (0x100u << k)) {
                *fields[k] = AV_RB32(trun.data + field);
                field += 4;
            }
        if (size > UINT64_MAX - *offset || (timed && duration > INT64_MAX - *clock))
            return AVERROR_INVALIDDATA;
        if (packet && *offset == (uint64_t)packet->pos && size == (uint32_t)packet->size) {
            if (match->found)
                return AVERROR_INVALIDDATA;
            const int64_t composition = trun.data[0] ? source_mov_signed(encoded) : encoded;
            if (timed && composition > 0 && *clock > INT64_MAX - composition)
                return AVERROR_INVALIDDATA;
            *match = (SourceMovFragmentSample){.found = 1, .timed = timed,
                .pts = timed ? *clock + composition : AV_NOPTS_VALUE, .dts = *clock,
                .duration = duration, .flags = flags};
        }
        *offset += size;
        if (timed)
            *clock += duration;
    }
    return 0;
}

/* The wire flags and any explicit tfdt are meaningful only for the exact
 * public packet's track, file range and bytes. Leave implicit fragment clocks
 * to the native demux observation until their predecessors can be proved. */
static int source_mov_fragment_sample(CatalogueField file, CatalogueField movie,
                                      uint64_t track, const AVPacket *packet,
                                      SourceMovFragmentSample *match)
{
    CatalogueField extensions;
    if (packet->pos < 0 || packet->size < 0 || (uint64_t)packet->pos > file.size ||
        (size_t)packet->size > file.size - (size_t)packet->pos ||
        (packet->size && memcmp(file.data + (size_t)packet->pos, packet->data, (size_t)packet->size)) ||
        catalogue_mov_find(movie, "mvex", &extensions) < 0)
        return AVERROR_INVALIDDATA;
    for (size_t root = 0; root < file.size;) {
        const uint64_t start = root;
        const uint8_t *kind;
        CatalogueField moof;
        if (catalogue_mov_box(file, &root, &kind, &moof) < 0)
            return AVERROR_INVALIDDATA;
        if (memcmp(kind, "moof", 4))
            continue;
        uint64_t previous = start;
        for (size_t at = 0; at < moof.size;) {
            CatalogueField traf, tfdt;
            if (catalogue_mov_box(moof, &at, &kind, &traf) < 0)
                return AVERROR_INVALIDDATA;
            if (memcmp(kind, "traf", 4))
                continue;
            SourceMovFragment fragment;
            if (source_mov_fragment_header(traf, extensions, start, previous, &fragment) < 0)
                return AVERROR_INVALIDDATA;
            int64_t clock = 0;
            const int timed = catalogue_mov_find(traf, "tfdt", &tfdt) == 0;
            if (timed) {
                if (tfdt.size == 8 && AV_RB32(tfdt.data) == 0)
                    clock = AV_RB32(tfdt.data + 4);
                else if (tfdt.size == 12 && AV_RB32(tfdt.data) == 0x01000000 &&
                         AV_RB64(tfdt.data + 4) <= INT64_MAX)
                    clock = (int64_t)AV_RB64(tfdt.data + 4);
                else
                    return AVERROR_INVALIDDATA;
            }
            previous = fragment.base;
            for (size_t child = 0; child < traf.size;) {
                CatalogueField trun;
                if (catalogue_mov_box(traf, &child, &kind, &trun) < 0)
                    return AVERROR_INVALIDDATA;
                if (!memcmp(kind, "trun", 4) &&
                    source_mov_fragment_run(trun, &fragment, &previous, &clock, timed, file.size,
                        fragment.track == track ? packet : NULL, match) < 0)
                    return AVERROR_INVALIDDATA;
            }
        }
    }
    return match->found ? 0 : AVERROR(ENOENT);
}

static int source_pes_timestamp(const uint8_t *bytes, unsigned prefix, int64_t *ticks)
{
    if (bytes[0] >> 4 != prefix || !(bytes[0] & bytes[2] & bytes[4] & 1))
        return AVERROR_INVALIDDATA;
    *ticks = (((int64_t)bytes[0] & 14) << 29) | ((int64_t)bytes[1] << 22) |
        (((int64_t)bytes[2] & 254) << 14) | ((int64_t)bytes[3] << 7) | (bytes[4] >> 1);
    return 0;
}

/* Read a PES on an uninterrupted carrier run. Require that the entire public
 * packet has the same extent and bytes before projecting the source clocks. */
static int source_pes_clock(CatalogueField source, size_t width, unsigned pid,
                             const AVPacket *packet, int64_t *pts, int64_t *dts)
{
    if (packet->pos < 0 || (uint64_t)packet->pos >= source.size || packet->size < 0)
        return AVERROR_INVALIDDATA;
    const size_t prefix = width - 188;
    const size_t position = (size_t)packet->pos;
    if (position % width && position % width != prefix)
        return AVERROR_INVALIDDATA;
    size_t at = position - position % width;
    uint8_t header[19];
    size_t received = 0, payload = 0, header_end = 9;
    int counter = -1, timed = 0, finite = 0;
    for (; source.size - at >= width; at += width) {
        const uint8_t *ts = source.data + at + prefix;
        if (ts[0] != 0x47)
            return AVERROR_INVALIDDATA;
        if ((((unsigned)ts[1] & 31) << 8 | ts[2]) != pid) {
            if (counter < 0)
                return AVERROR_INVALIDDATA;
            continue;
        }
        if ((ts[1] & 0x80) || (ts[3] & 0xc0) || !(ts[3] & 0x30))
            return AVERROR_INVALIDDATA;
        if (!(ts[3] & 0x10))
            continue;
        if (counter >= 0 && (ts[1] & 0x40))
            break;
        if ((counter < 0 && !(ts[1] & 0x40)) || (counter >= 0 && (ts[3] & 15) != ((counter + 1) & 15)))
            return AVERROR_INVALIDDATA;
        counter = ts[3] & 15;
        const size_t start = ts[3] & 0x20 ? 5u + ts[4] : 4u;
        if (start >= 188)
            return AVERROR_INVALIDDATA;
        for (size_t i = start; i < 188; ++i) {
            if (received < header_end) {
                if (received < sizeof(header))
                    header[received] = ts[i];
                if (++received != 9)
                    continue;
                if (header[0] || header[1] || header[2] != 1 || (header[6] & 0xc0) != 0x80)
                    return AVERROR_INVALIDDATA;
                timed = header[7] >> 6;
                header_end = 9u + header[8];
                const unsigned length = AV_RB16(header + 4);
                finite = length != 0;
                if (timed == 1 || (timed == 2 && header[8] < 5) || (timed == 3 && header[8] < 10) ||
                    (finite && length + 6u != header_end + (size_t)packet->size) ||
                    (!finite && (header[3] & 0xf0) != 0xe0))
                    return AVERROR_INVALIDDATA;
            } else if (payload >= (size_t)packet->size || packet->data[payload++] != ts[i]) {
                return AVERROR_INVALIDDATA;
            }
        }
        if (finite && received == header_end && payload == (size_t)packet->size)
            break;
    }
    if (received != header_end || payload != (size_t)packet->size ||
        (!finite && at < source.size && source.size - at < width))
        return AVERROR_INVALIDDATA;
    *pts = *dts = AV_NOPTS_VALUE;
    if (timed && source_pes_timestamp(header + 9, timed == 3 ? 3u : 2u, pts) < 0)
        return AVERROR_INVALIDDATA;
    if (timed == 3 && source_pes_timestamp(header + 14, 1, dts) < 0)
        return AVERROR_INVALIDDATA;
    return header[3];
}

/* AVStream's codec tag can come from an elementary registration descriptor.
 * The source catalogue's coding tag is the PMT's separate stream_type octet. */
static int catalogue_pmt_type(CatalogueField source, size_t width, int pmt_pid, int pid,
                              CatalogueField *descriptors)
{
    const AVCRC *crc = av_crc_get_table(AV_CRC_32_IEEE);
    for (size_t at = 0; source.size - at >= width; at += width) {
        const uint8_t *ts = source.data + at + width - 188;
        if (ts[0] != 0x47 || (ts[1] & 0xc0) != 0x40 || ((ts[1] & 31) << 8 | ts[2]) != pmt_pid ||
            !(ts[3] & 0x10) || (ts[3] & 0xc0))
            continue;
        size_t offset = ts[3] & 0x20 ? 5u + ts[4] : 4u;
        if (offset >= 188)
            continue;
        offset += (size_t)ts[offset] + 1;
        if (offset > 188 - 16)
            continue;
        const uint8_t *section = ts + offset;
        const size_t length = (AV_RB16(section + 1) & 0x0fff) + 3;
        if (section[0] != 2 || (section[1] & 0xf0) != 0xb0 || length < 16 || length > 188 - offset ||
            AV_RB16(section + 3) != 1 || !(section[5] & 1) || section[6] || section[7] ||
            av_crc(crc, UINT32_MAX, section, length))
            continue;
        size_t item = 12 + (AV_RB16(section + 10) & 0x0fff);
        const size_t end = length - 4;
        int code = -1;
        CatalogueField loop = {0};
        while (item <= end && end - item >= 5) {
            const size_t next = item + 5 + (AV_RB16(section + item + 3) & 0x0fff);
            if (next > end)
                break;
            if ((AV_RB16(section + item + 1) & 0x1fff) == pid) {
                if (code >= 0)
                    return -1;
                code = section[item];
                loop = (CatalogueField){section + item + 5, next - item - 5};
            }
            item = next;
        }
        if (item == end && code >= 0) {
            *descriptors = loop;
            return code;
        }
    }
    return -1;
}

static int source_view_codec_is_pcm(enum AVCodecID codec)
{
    const AVCodecDescriptor *descriptor = avcodec_descriptor_get(codec);
    return descriptor && !strncmp(descriptor->name, "pcm_", 4);
}

static const char *source_view_codec(const AVCodecParameters *codec)
{
    switch (codec->codec_id) {
    case AV_CODEC_ID_MPEG1VIDEO: return "mpeg2video";
    case AV_CODEC_ID_MP1:
    case AV_CODEC_ID_MP2:
    case AV_CODEC_ID_MP3: return "mpeg_audio";
    case AV_CODEC_ID_NONE: return "other";
    default: return source_view_codec_is_pcm(codec->codec_id) ? "pcm" : avcodec_get_name(codec->codec_id);
    }
}

static uint64_t source_view_kind(enum AVMediaType kind)
{
    switch (kind) {
    case AVMEDIA_TYPE_VIDEO: return 1;
    case AVMEDIA_TYPE_AUDIO: return 2;
    case AVMEDIA_TYPE_SUBTITLE: return 3;
    case AVMEDIA_TYPE_DATA:
    case AVMEDIA_TYPE_ATTACHMENT: return 4;
    default: return 0;
    }
}

static void source_view_string(ViewWork *work, const uint8_t *data, size_t size)
{
    fold(&work->digest, size);
    if (size)
        fold_bytes(&work->digest, data, size);
    work->bytes += size;
}

static void source_view_hex(const uint8_t *data, size_t size)
{
    if (!size)
        putchar('-');
    for (size_t i = 0; i < size; ++i)
        printf("%02x", data[i]);
}

static uint64_t source_view_track_id(const AVStream *stream)
{
    return stream->id > 0 ? (uint64_t)stream->id : (uint64_t)stream->index + 1;
}

static void source_view_languages(ViewWork *work, uint64_t id, CatalogueField descriptors)
{
    for (size_t at = 0; descriptors.size - at >= 2;) {
        const uint8_t *tag = descriptors.data + at;
        const size_t size = tag[1];
        if (size > descriptors.size - at - 2)
            break;
        if (tag[0] == 0x0a && size % 4 == 0) {
            for (size_t entry = 2; entry < size + 2; entry += 4) {
                const uint8_t *language = tag + entry;
                const uint64_t words[] = {id, language[0], language[1], language[2], language[3]};
                view_record(work, 4, words, 5, NULL, 0);
                if (work->trace)
                    printf("4 %" PRIu64 " %u %u %u %u\n", id, language[0], language[1],
                           language[2], language[3]);
            }
        }
        at += size + 2;
    }
}

static int source_view_track(ViewWork *work, const AVStream *stream,
                             const PlaybackSourceCase *source, CatalogueField tracks,
                             SourceMovSamples *samples, int coded_type, CatalogueField descriptors)
{
    const AVCodecParameters *parameters = stream->codecpar;
    const char *codec = source_view_codec(parameters);
    const uint64_t id = source_view_track_id(stream);
    const uint64_t kind = source_view_kind(parameters->codec_type);
    const AVDictionaryEntry *language = av_dict_get(stream->metadata, "language", NULL, 0);
    uint64_t default_track = 1 + !!(stream->disposition & AV_DISPOSITION_DEFAULT);
    uint64_t forced = 1 + !!(stream->disposition & AV_DISPOSITION_FORCED);
    const uint32_t tag = parameters->codec_tag;
    const uint8_t source_type = (uint8_t)coded_type;
    const uint8_t tag_bytes[] = {tag, tag >> 8, tag >> 16, tag >> 24};
    CatalogueField codec_tag = {tag_bytes, source->transport && tag <= 255 ? 1 : tag ? 4 : 0};
    CatalogueField lang = {0};
    CatalogueField configuration = {parameters->extradata,
        parameters->extradata_size > 0 ? (size_t)parameters->extradata_size : 0};
    if (language)
        lang = (CatalogueField){(const uint8_t *)language->value, strlen(language->value)};
    if (source->matroska) {
        const int rc = catalogue_track(tracks, id, &codec_tag, &lang);
        if (rc < 0)
            return rc;
        if (work->trace)
            fprintf(stderr, "track %" PRIu64 " native codec tag=%" PRIu32 " language=%s\n",
                    id, tag, language ? language->value : "(absent)");
    }
    if (source->ignore_edits) {
        CatalogueField entry = {0}, table = {0};
        codec_tag = (CatalogueField){0};
        int rc = catalogue_mov_entry(tracks, id, &codec_tag, &entry, &table);
        if (rc < 0)
            return rc;
        if (parameters->codec_id == AV_CODEC_ID_RAWVIDEO ||
            parameters->codec_id == AV_CODEC_ID_MOV_TEXT || source_view_codec_is_pcm(parameters->codec_id))
            configuration = entry;
        if (work->trace)
            fprintf(stderr, "track %" PRIu64 " native tag=%" PRIu32 " language=%s disposition=%d extradata=%d\n",
                    id, tag, language ? language->value : "(absent)", stream->disposition,
                    parameters->extradata_size);
        /* TrackHeader enabled/in_movie and MediaHeader language are separate
         * source fields. The neutral MOV view has no language or dispositions. */
        lang = (CatalogueField){0};
        default_track = forced = 0;
        source_mov_regular(table, samples);
    }
    if (source->transport) {
        if (work->trace)
            fprintf(stderr, "PID %" PRIu64 " native tag=%" PRIu32 " disposition=%d\n",
                    id, tag, stream->disposition);
        if (coded_type >= 0)
            codec_tag = (CatalogueField){&source_type, 1};
        if (language)
            lang.size = strcspn(language->value, ",");
        default_track = forced = 0;
    }
    fold(&work->digest, 2);
    fold(&work->digest, id);
    fold(&work->digest, kind);
    source_view_string(work, (const uint8_t *)codec, strlen(codec));
    source_view_string(work, codec_tag.data, codec_tag.size);
    source_view_string(work, configuration.data, configuration.size);
    fold(&work->digest, lang.data != NULL);
    if (lang.data)
        source_view_string(work, lang.data, lang.size);
    fold(&work->digest, default_track);
    fold(&work->digest, forced);
    ++work->packets;
    if (work->trace) {
        printf("2 %" PRIu64 " %" PRIu64 " %s ", id, kind, codec);
        source_view_hex(codec_tag.data, codec_tag.size);
        putchar(' ');
        source_view_hex(configuration.data, configuration.size);
        if (lang.data)
            printf(" Some(\"%.*s\")", (int)lang.size, (const char *)lang.data);
        else
            printf(" None");
        printf(" %" PRIu64 " %" PRIu64 "\n", default_track, forced);
    }
    if (source->transport)
        source_view_languages(work, id, descriptors);
    return 0;
}

static int source_view_open_mp4(AVFormatContext **format, PacketInput *input, int seekable)
{
    uint8_t *buffer = av_malloc(input->quantum);
    if (!buffer)
        return AVERROR(ENOMEM);
    AVIOContext *io = avio_alloc_context(buffer, input->quantum, 0, input, packet_input_read,
                                         NULL, seekable ? packet_input_seek : NULL);
    if (!io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    *format = avformat_alloc_context();
    if (!*format) {
        av_freep(&io->buffer);
        avio_context_free(&io);
        return AVERROR(ENOMEM);
    }
    (*format)->pb = io;
    (*format)->flags |= AVFMT_FLAG_CUSTOM_IO;
    AVDictionary *options = NULL;
    int rc = av_dict_set(&options, "ignore_editlist", "1", 0);
    if (rc >= 0)
        rc = avformat_open_input(format, NULL, av_find_input_format("mov"), &options);
    av_dict_free(&options);
    if (rc < 0) {
        avformat_free_context(*format);
        *format = NULL;
        av_freep(&io->buffer);
        avio_context_free(&io);
    }
    return rc;
}

static int source_view_selected(const AVFormatContext *format, const AVStream *stream,
                                 const PlaybackSourceCase *source)
{
    if (!source->transport)
        return 1;
    if (source->pid_count) {
        int selected = 0;
        for (size_t i = 0; i < source->pid_count; ++i)
            selected |= stream->id == (int)source->pids[i];
        if (!selected)
            return 0;
    }
    for (unsigned p = 0; p < format->nb_programs; ++p) {
        const AVProgram *program = format->programs[p];
        if (program->program_num != 1)
            continue;
        for (unsigned i = 0; i < program->nb_stream_indexes; ++i)
            if (program->stream_index[i] == (unsigned)stream->index)
                return 1;
    }
    return 0;
}

static int source_view_raw(AVFormatContext *format, ViewWork *work)
{
    int rc = avformat_find_stream_info(format, NULL);
    if (rc < 0)
        return rc;
    if (format->nb_streams != 1)
        return AVERROR_INVALIDDATA;
    const AVStream *stream = format->streams[0];
    const AVCodecParameters *parameters = stream->codecpar;
    AVRational rate = parameters->framerate;
    if (rate.num <= 0 || rate.den <= 0)
        rate = (AVRational){0, 1};
    const uint64_t words[] = {(uint64_t)rate.num, (uint64_t)rate.den};
    view_record(work, 4, words, 2, NULL, 0);
    if (work->trace) {
        printf("4 %d %d\n", rate.num, rate.den);
        fprintf(stderr, "reference SAR: %d/%d\n",
                parameters->sample_aspect_ratio.num, parameters->sample_aspect_ratio.den);
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    while ((rc = av_read_frame(format, packet)) >= 0) {
        const int64_t ns = packet->pts == AV_NOPTS_VALUE ? 0 :
            av_rescale_q(packet->pts, stream->time_base, (AVRational){1, 1000000000});
        const uint64_t word = (uint64_t)ns;
        view_record(work, 3, &word, 1, NULL, 0);
        if (work->trace)
            printf("3 %" PRIu64 "\n", word);
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    return rc;
}

static int source_view_run(const ViewCase *input, ViewWork *work)
{
    const PlaybackSourceCase *source = input->prepared;
    const AVInputFormat *demuxer = av_find_input_format(source->demuxer);
    if (!demuxer)
        return 2;
    PacketInput reader = {
        .data = source->entities ? source->entities : input->data,
        .size = source->entities ? (int64_t)source->size : (int64_t)input->size,
        .quantum = 32768
    };
    AVFormatContext *format = NULL;
    const int ts_flags = AVFMT_FLAG_NOPARSE | AVFMT_FLAG_NOFILLIN;
    int rc = source->ignore_edits ? source_view_open_mp4(&format, &reader, source->seekable)
        : packet_open(&format, &reader, source->seekable, demuxer, source->transport ? ts_flags : 0);
    if (rc == AVERROR(ENOMEM))
        return 2;
    if (rc < 0) {
        view_record(work, 8, NULL, 0, NULL, 0);
        if (work->trace)
            puts("8");
        return 0;
    }
    if (source->raw) {
        rc = source_view_raw(format, work);
        packet_close(&format);
        if (rc == AVERROR(ENOMEM))
            return 2;
        const uint64_t terminal = rc == AVERROR_EOF ? 7 : 8;
        view_record(work, terminal, NULL, 0, NULL, 0);
        if (work->trace)
            printf("%" PRIu64 "\n", terminal);
        return 0;
    }
    CatalogueField tracks = {0};
    if (source->matroska) {
        CatalogueField segment;
        if (catalogue_find((CatalogueField){input->data, input->size}, 0x18538067, &segment) < 0 ||
            catalogue_find(segment, 0x1654ae6b, &tracks) < 0) {
            packet_close(&format);
            return 2;
        }
    } else if (source->ignore_edits &&
               catalogue_mov_find((CatalogueField){input->data, input->size}, "moov", &tracks) < 0) {
        packet_close(&format);
        return 2;
    }
    const unsigned track_count = format->nb_streams;
    SourceMovSamples *samples = source->ignore_edits ? av_calloc(track_count, sizeof(*samples)) : NULL;
    if (source->ignore_edits && track_count && !samples) {
        packet_close(&format);
        return 2;
    }
    int pmt_pid = -1;
    if (source->transport)
        for (unsigned i = 0; i < format->nb_programs; ++i)
            if (format->programs[i]->program_num == 1)
                pmt_pid = format->programs[i]->pmt_pid;
    for (unsigned i = 0; i < track_count; ++i) {
        CatalogueField descriptors = {0};
        const int coded = pmt_pid >= 0 ? catalogue_pmt_type(
            (CatalogueField){reader.data, (size_t)reader.size}, (size_t)source->transport,
            pmt_pid, format->streams[i]->id, &descriptors) : -1;
        if (source_view_selected(format, format->streams[i], source) &&
            source_view_track(work, format->streams[i], source, tracks, samples ? &samples[i] : NULL,
                              coded, descriptors) < 0) {
            av_free(samples);
            packet_close(&format);
            return 2;
        }
    }
    AVPacket *packet = av_packet_alloc();
    if (!packet) {
        av_free(samples);
        packet_close(&format);
        return 2;
    }
    while ((rc = av_read_frame(format, packet)) >= 0) {
        AVStream *stream = format->streams[packet->stream_index];
        if (source_view_selected(format, stream, source)) {
            int64_t pts = packet->pts, dts = packet->dts, duration = packet->duration;
            uint64_t flags = (uint64_t)packet->flags;
            if (samples && (unsigned)packet->stream_index < track_count) {
                const CatalogueField file = {input->data, input->size};
                SourceMovFragmentSample sample = {0};
                if (!source_mov_sample(&samples[packet->stream_index], packet, file,
                                       &pts, &dts, &duration, &flags) &&
                    source_mov_fragment_sample(file, tracks, source_view_track_id(stream),
                                               packet, &sample) == 0) {
                    flags &= ~(uint64_t)AV_PKT_FLAG_KEY;
                    if (!(sample.flags & UINT32_C(0x10000)))
                        flags |= AV_PKT_FLAG_KEY;
                    if (sample.timed) {
                        pts = sample.pts;
                        dts = sample.dts;
                        duration = sample.duration;
                    }
                }
                if (work->trace && (flags != (uint64_t)packet->flags || pts != packet->pts ||
                                    dts != packet->dts || duration != packet->duration))
                    fprintf(stderr, "sample at %" PRId64 " native PTS=%" PRId64 " DTS=%" PRId64
                            " duration=%" PRId64 " flags=%d\n", packet->pos, packet->pts,
                            packet->dts, packet->duration, packet->flags);
            }
            int64_t wire_pts, wire_dts;
            if (source->transport && source_pes_clock(
                    (CatalogueField){reader.data, (size_t)reader.size},
                    (size_t)source->transport, (unsigned)stream->id, packet, &wire_pts, &wire_dts) >= 0) {
                if (work->trace)
                    fprintf(stderr, "PES at %" PRId64 " native PTS=%" PRId64 " DTS=%" PRId64
                            " duration=%" PRId64 " flags=%d\n", packet->pos, packet->pts,
                            packet->dts, packet->duration, packet->flags);
                pts = wire_pts;
                dts = wire_dts;
                duration = 0;
                flags = 0;
            }
            if (!source->transport && !source->ignore_edits) {
                const AVRational ns = {1, 1000000000};
                if (pts != AV_NOPTS_VALUE)
                    pts = av_rescale_q(pts, stream->time_base, ns);
                if (dts != AV_NOPTS_VALUE)
                    dts = av_rescale_q(dts, stream->time_base, ns);
                duration = av_rescale_q(duration, stream->time_base, ns);
            }
            fold(&work->digest, 1);
            PacketWork view = {.digest = work->digest, .trace = work->trace};
            if (work->trace)
                printf("1 ");
            packet_fold(&view, source_view_track_id(stream), pts, dts,
                        duration > 0 ? (uint64_t)duration : 0, flags,
                        packet->data, (size_t)packet->size);
            work->digest = view.digest;
            work->packets += view.packets;
            work->bytes += view.bytes;
        }
        av_packet_unref(packet);
    }
    av_packet_free(&packet);
    av_free(samples);
    packet_close(&format);
    if (rc == AVERROR(ENOMEM))
        return 2;
    uint64_t terminal = rc == AVERROR_EOF ? 7 : 8;
    view_record(work, terminal, NULL, 0, NULL, 0);
    if (work->trace)
        printf("%" PRIu64 "\n", terminal);
    return 0;
}

static void source_view_release(void *opaque)
{
    PlaybackSourceCase *source = opaque;
    free(source->entities);
    free(source);
}

static int source_view_add_entity(const ViewCase *input, PlaybackSourceCase *source)
{
    const char *dir = input->argv[1], *name = input->argv[2];
    if (!*name || name[0] == '-' || strpbrk(name, "/\\"))
        return 2;
    size_t path_size = strlen(dir) + strlen(name) + 2;
    char *path = malloc(path_size);
    if (!path)
        return 2;
    snprintf(path, path_size, "%s/%s", dir, name);
    size_t tail_size = 0;
    uint8_t *tail = view_load(path, &tail_size);
    free(path);
    if (!tail)
        return 2;
    source->size = input->size + tail_size;
    source->entities = malloc(source->size ? source->size : 1);
    if (source->entities) {
        memcpy(source->entities, input->data, input->size);
        memcpy(source->entities + input->size, tail, tail_size);
    }
    free(tail);
    return source->entities ? 0 : 2;
}

static int source_view_parse_pids(const char *pids, PlaybackSourceCase *source)
{
    while (*pids && source->pid_count < 64) {
        char *end = NULL;
        unsigned long pid = strtoul(pids, &end, 10);
        if (pids == end || !pid || pid >= 0x1fff || (*end && *end != ','))
            return 2;
        source->pids[source->pid_count++] = (unsigned)pid;
        if (!*end)
            return 0;
        pids = end + 1;
    }
    return 2;
}

static int playback_source_view_prepare(ViewCase *input)
{
    /* This entry uses the shared row fold and provides its own prefix dispatch. */
    (void)packet_main;
    if (input->argc < 1)
        return 2;
    const char *layout = input->argv[0];
    PlaybackSourceCase source = {0};
    if (!strcmp(layout, "matroska")) {
        source.demuxer = "matroska";
        source.seekable = 1;
        source.matroska = 1;
    } else if (!strcmp(layout, "mp4") || !strcmp(layout, "mp4-progressive") ||
               !strcmp(layout, "mp4-timing-policy")) {
        source.demuxer = "mov";
        source.seekable = strcmp(layout, "mp4-progressive") != 0;
        source.ignore_edits = 1;
    } else if (!strcmp(layout, "raw-avc") || !strcmp(layout, "raw-hevc")) {
        source.demuxer = !strcmp(layout, "raw-avc") ? "h264" : "hevc";
        source.raw = 1;
    } else if (!strcmp(layout, "ts188") || !strcmp(layout, "m2ts192") ||
               !strcmp(layout, "ts-entities")) {
        source.demuxer = "mpegts";
        source.transport = !strcmp(layout, "m2ts192") ? 192 : 188;
    } else {
        return 2;
    }
    if (!strcmp(layout, "ts-entities")) {
        if (input->argc != 3 || source_view_add_entity(input, &source))
            return 2;
    } else if (source.transport && input->argc == 2) {
        if (source_view_parse_pids(input->argv[1], &source))
            return 2;
    } else if (input->argc != 1) {
        return 2;
    }
    PlaybackSourceCase *prepared = malloc(sizeof(*prepared));
    if (!prepared) {
        free(source.entities);
        return 2;
    }
    *prepared = source;
    input->prepared = prepared;
    input->release = source_view_release;
    input->name = "playback-source-view";
    input->run = source_view_run;
    return 0;
}
