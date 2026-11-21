/* Public format, stream, chapter and attachment metadata. */
#include <libavutil/avstring.h>

static void mkv_string(ViewWork *work, uint64_t kind, uint64_t field, const char *str)
{
    mkv_optional(work, kind, field, (const uint8_t *)str, str ? strlen(str) : 0);
}

static const char *mkv_tag(const AVDictionary *dict, const char *key)
{
    const AVDictionaryEntry *entry = av_dict_get(dict, key, NULL, 0);
    return entry ? entry->value : NULL;
}

static void mkv_attachments(MkvView *view, ViewWork *work)
{
    for (unsigned i = 0; i < view->format->nb_streams; ++i) {
        const AVStream *stream = view->format->streams[i];
        const uint8_t *data;
        size_t size;
        if (stream->codecpar->codec_type == AVMEDIA_TYPE_ATTACHMENT) {
            data = stream->codecpar->extradata;
            size = stream->codecpar->extradata_size;
        } else if (stream->disposition & AV_DISPOSITION_ATTACHED_PIC) {
            data = stream->attached_pic.data;
            size = stream->attached_pic.size;
        } else {
            continue;
        }
        const uint64_t fields[] = {0, size};
        mkv_row(work, 2, fields, 2, data, size);
        mkv_string(work, 2, 1, mkv_tag(stream->metadata, "filename"));
        mkv_string(work, 2, 2, mkv_tag(stream->metadata, "mimetype"));
        mkv_string(work, 2, 3, mkv_tag(stream->metadata, "title"));
    }
}

static int mkv_entry_order(const void *first, const void *second)
{
    const AVDictionaryEntry *const *a = first, *const *b = second;
    return av_strcasecmp((*a)->key, (*b)->key);
}

static int mkv_dictionary(ViewWork *work, uint64_t scope, uint64_t index, const AVDictionary *dict)
{
    const uint64_t fields[] = {scope, index};
    mkv_row(work, 3, fields, 2, NULL, 0);
    int capacity = av_dict_count(dict);
    if (!capacity)
        return 0;
    const AVDictionaryEntry **entries = av_malloc_array(capacity, sizeof(*entries));
    if (!entries)
        return AVERROR(ENOMEM);
    size_t count = 0;
    const AVDictionaryEntry *entry = NULL;
    while ((entry = av_dict_iterate(dict, entry))) {
        if (scope != 0 || av_strcasecmp(entry->key, "creation_time"))
            entries[count++] = entry;
    }
    qsort(entries, count, sizeof(*entries), mkv_entry_order);
    for (size_t i = 0; i < count; ++i) {
        mkv_string(work, 3, 16, entries[i]->key);
        mkv_string(work, 3, 17, entries[i]->value);
    }
    av_free(entries);
    return 0;
}

static int mkv_metadata(MkvView *view, ViewWork *work)
{
    AVFormatContext *format = view->format;
    int status = mkv_dictionary(work, 0, 0, format->metadata);
    for (unsigned i = 0; status >= 0 && i < format->nb_streams; ++i)
        status = mkv_dictionary(work, 1, i, format->streams[i]->metadata);
    for (unsigned i = 0; status >= 0 && i < format->nb_chapters; ++i) {
        const AVChapter *chapter = format->chapters[i];
        const uint64_t fields[] = {2, (uint64_t)chapter->id, (uint64_t)chapter->start,
            (uint64_t)chapter->end, chapter->time_base.num, chapter->time_base.den};
        mkv_row(work, 3, fields, 6, NULL, 0);
        status = mkv_dictionary(work, 3, i, chapter->metadata);
    }
    return status;
}

static int matroska_metadata_run(const ViewCase *input, ViewWork *work)
{
    MkvView view = {0};
    int status = mkv_view_open(&view, input);
    if (status >= 0) {
        if (!strcmp(input->name, "mkv-attachments"))
            mkv_attachments(&view, work);
        else
            status = mkv_metadata(&view, work);
    }
    mkv_view_close(&view);
    return mkv_view_finish(work, status);
}

static int matroska_metadata_prepare(ViewCase *input)
{
    if (input->argc)
        return 2;
    input->name = !strcmp(input->prefix, "mkv-attachments-") ? "mkv-attachments" : "mkv-metadata-fields";
    input->run = matroska_metadata_run;
    return 0;
}
