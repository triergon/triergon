/* Matroska public demuxer views over a preloaded, seekable input. */
#include <libavutil/dovi_meta.h>
#include <libavutil/hdr_dynamic_metadata.h>

typedef struct {
    const ViewCase *input;
    int64_t position;
    AVIOContext *io;
    AVFormatContext *format;
} MkvView;

static int mkv_view_read(void *opaque, uint8_t *dst, int size)
{
    MkvView *view = opaque;
    int64_t left = (int64_t)view->input->size - view->position;
    int count = left < size ? (int)left : size;
    if (!count)
        return AVERROR_EOF;
    memcpy(dst, view->input->data + view->position, count);
    view->position += count;
    return count;
}

static int64_t mkv_view_seek(void *opaque, int64_t offset, int whence)
{
    MkvView *view = opaque;
    int64_t size = (int64_t)view->input->size;
    if (whence == AVSEEK_SIZE)
        return size;
    int64_t base;
    switch (whence & ~AVSEEK_FORCE) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = view->position; break;
    case SEEK_END: base = size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > size - base)
        return AVERROR(EINVAL);
    view->position = base + offset;
    return view->position;
}

static int mkv_view_open(MkvView *view, const ViewCase *input)
{
    view->input = input;
    uint8_t *buffer = av_malloc(32768);
    if (!buffer)
        return AVERROR(ENOMEM);
    view->io = avio_alloc_context(buffer, 32768, 0, view, mkv_view_read, NULL, mkv_view_seek);
    if (!view->io) {
        av_free(buffer);
        return AVERROR(ENOMEM);
    }
    view->format = avformat_alloc_context();
    if (!view->format)
        return AVERROR(ENOMEM);
    view->format->pb = view->io;
    view->format->flags |= AVFMT_FLAG_CUSTOM_IO;
    return avformat_open_input(&view->format, NULL, av_find_input_format("matroska"), NULL);
}

static void mkv_view_close(MkvView *view)
{
    avformat_close_input(&view->format);
    if (view->io)
        av_freep(&view->io->buffer);
    avio_context_free(&view->io);
}

static void mkv_row(ViewWork *work, uint64_t kind, const uint64_t *fields, size_t count,
                    const uint8_t *data, size_t size)
{
    view_record(work, kind, fields, count, data, size);
    if (!work->trace)
        return;
    printf("%" PRIu64, kind);
    for (size_t i = 0; i < count; ++i)
        printf(" %" PRIu64, fields[i]);
    if (size) {
        putchar(' ');
        for (size_t i = 0; i < size; ++i)
            printf("%02x", data[i]);
    }
    putchar('\n');
}

static int mkv_view_finish(ViewWork *work, int status)
{
    if (status == AVERROR(ENOMEM))
        return 2;
    mkv_row(work, status >= 0 || status == AVERROR_EOF ? 7 : 8, NULL, 0, NULL, 0);
    return 0;
}

static void mkv_optional(ViewWork *work, uint64_t kind, uint64_t field,
                         const uint8_t *data, size_t size)
{
    const uint64_t words[] = {field, data != NULL, size};
    mkv_row(work, kind, words, 3, data, size);
}

static void mkv_ratio(uint64_t **out, AVRational value)
{
    *(*out)++ = (uint64_t)(int64_t)value.num;
    *(*out)++ = (uint64_t)(int64_t)value.den;
}

static void mkv_grid(uint64_t **out, unsigned flag, unsigned rows, unsigned columns,
                     const AVRational data[25][25])
{
    *(*out)++ = flag;
    if (!flag)
        return;
    *(*out)++ = rows;
    *(*out)++ = columns;
    for (unsigned row = 0; row < rows; ++row)
        for (unsigned col = 0; col < columns; ++col)
            mkv_ratio(out, data[row][col]);
}

static void mkv_window(uint64_t **out, const AVHDRPlusColorTransformParams *p, unsigned index)
{
    *(*out)++ = index != 0;
    if (index) {
        mkv_ratio(out, p->window_upper_left_corner_x);
        mkv_ratio(out, p->window_upper_left_corner_y);
        mkv_ratio(out, p->window_lower_right_corner_x);
        mkv_ratio(out, p->window_lower_right_corner_y);
        *(*out)++ = p->center_of_ellipse_x;
        *(*out)++ = p->center_of_ellipse_y;
        *(*out)++ = p->rotation_angle;
        *(*out)++ = p->semimajor_axis_internal_ellipse;
        *(*out)++ = p->semimajor_axis_external_ellipse;
        *(*out)++ = p->semiminor_axis_external_ellipse;
        *(*out)++ = p->overlap_process_option;
    }
    for (size_t i = 0; i < 3; ++i)
        mkv_ratio(out, p->maxscl[i]);
    mkv_ratio(out, p->average_maxrgb);
    *(*out)++ = p->num_distribution_maxrgb_percentiles;
    for (size_t i = 0; i < p->num_distribution_maxrgb_percentiles; ++i) {
        *(*out)++ = p->distribution_maxrgb[i].percentage;
        mkv_ratio(out, p->distribution_maxrgb[i].percentile);
    }
    mkv_ratio(out, p->fraction_bright_pixels);
    *(*out)++ = p->tone_mapping_flag;
    if (p->tone_mapping_flag) {
        mkv_ratio(out, p->knee_point_x);
        mkv_ratio(out, p->knee_point_y);
        *(*out)++ = p->num_bezier_curve_anchors;
        for (size_t i = 0; i < p->num_bezier_curve_anchors; ++i)
            mkv_ratio(out, p->bezier_curve_anchors[i]);
    }
    *(*out)++ = p->color_saturation_mapping_flag;
    if (p->color_saturation_mapping_flag)
        mkv_ratio(out, p->color_saturation_weight);
}

static int mkv_hdr(ViewWork *work, const AVDynamicHDRPlus *hdr)
{
    if (hdr->num_windows < 1 || hdr->num_windows > 3 ||
        hdr->num_rows_targeted_system_display_actual_peak_luminance > 25 ||
        hdr->num_cols_targeted_system_display_actual_peak_luminance > 25 ||
        hdr->num_rows_mastering_display_actual_peak_luminance > 25 ||
        hdr->num_cols_mastering_display_actual_peak_luminance > 25)
        return AVERROR_INVALIDDATA;
    /* Two 25x25 rational grids and the three bounded window records. */
    uint64_t fields[5 + 2 * (3 + 25 * 25 * 2) + 3 * 111], *out = fields;
    *out++ = 3;
    *out++ = hdr->application_version;
    *out++ = hdr->num_windows;
    mkv_ratio(&out, hdr->targeted_system_display_maximum_luminance);
    mkv_grid(&out, hdr->targeted_system_display_actual_peak_luminance_flag,
             hdr->num_rows_targeted_system_display_actual_peak_luminance,
             hdr->num_cols_targeted_system_display_actual_peak_luminance,
             hdr->targeted_system_display_actual_peak_luminance);
    mkv_grid(&out, hdr->mastering_display_actual_peak_luminance_flag,
             hdr->num_rows_mastering_display_actual_peak_luminance,
             hdr->num_cols_mastering_display_actual_peak_luminance,
             hdr->mastering_display_actual_peak_luminance);
    for (unsigned n = 0; n < hdr->num_windows; ++n) {
        const AVHDRPlusColorTransformParams *p = &hdr->params[n];
        if (p->num_bezier_curve_anchors > 15 || p->num_distribution_maxrgb_percentiles > 15)
            return AVERROR_INVALIDDATA;
        mkv_window(&out, p, n);
    }
    mkv_row(work, 4, fields, (size_t)(out - fields), NULL, 0);
    return 0;
}

static int mkv_packets(MkvView *view, ViewWork *work)
{
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        return AVERROR(ENOMEM);
    const enum AVPacketSideDataType types[] = {
        AV_PKT_DATA_MATROSKA_BLOCKADDITIONAL, AV_PKT_DATA_LCEVC, AV_PKT_DATA_DYNAMIC_HDR10_PLUS,
    };
    int status;
    while ((status = av_read_frame(view->format, packet)) >= 0) {
        const uint64_t index = (unsigned)packet->stream_index;
        mkv_row(work, 1, &index, 1, NULL, 0);
        for (size_t i = 0; i < 3; ++i) {
            size_t size;
            const uint8_t *data = av_packet_get_side_data(packet, types[i], &size);
            if (!data)
                continue;
            if (i == 2) {
                status = size < sizeof(AVDynamicHDRPlus) ? AVERROR_INVALIDDATA :
                         mkv_hdr(work, (const AVDynamicHDRPlus *)data);
                break;
            }
            const uint64_t fields[] = {i + 1, size};
            mkv_row(work, 4, fields, 2, data, size);
        }
        av_packet_unref(packet);
        if (status < 0)
            break;
    }
    av_packet_free(&packet);
    return status;
}

static int mkv_streams(MkvView *view, ViewWork *work)
{
    for (uint64_t track = 0; track < view->format->nb_streams; ++track) {
        const AVCodecParameters *par = view->format->streams[track]->codecpar;
        const AVPacketSideData *dv = av_packet_side_data_get(par->coded_side_data,
                                      par->nb_coded_side_data, AV_PKT_DATA_DOVI_CONF);
        const AVPacketSideData *hevc = av_packet_side_data_get(par->coded_side_data,
                                        par->nb_coded_side_data, AV_PKT_DATA_HEVC_CONF);
        mkv_row(work, 1, &track, 1, NULL, 0);
        if (dv) {
            if (dv->size < sizeof(AVDOVIDecoderConfigurationRecord))
                return AVERROR_INVALIDDATA;
            const AVDOVIDecoderConfigurationRecord *d = (const void *)dv->data;
            const uint64_t fields[] = {1, d->dv_version_major, d->dv_version_minor,
                d->dv_profile, d->dv_level, d->rpu_present_flag, d->el_present_flag,
                d->bl_present_flag, d->dv_bl_signal_compatibility_id, d->dv_md_compression};
            mkv_row(work, 2, fields, 10, NULL, 0);
        }
        mkv_optional(work, 2, 2, hevc ? hevc->data : NULL, hevc ? hevc->size : 0);
    }
    return 0;
}

static int matroska_side_data_run(const ViewCase *input, ViewWork *work)
{
    MkvView view = {0};
    int status = mkv_view_open(&view, input);
    if (status >= 0)
        status = !strcmp(input->name, "mkv-side-data") ? mkv_packets(&view, work) :
                 mkv_streams(&view, work);
    mkv_view_close(&view);
    return mkv_view_finish(work, status);
}

static int matroska_side_data_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "mkv-side-data-") && input->argc == 1 &&
        (!strcmp(input->argv[0], "matroska") || !strcmp(input->argv[0], "webm")))
        input->name = "mkv-side-data";
    else if (!strcmp(input->prefix, "mkv-track-extensions-") && !input->argc)
        input->name = "mkv-track-extensions";
    else
        return 2;
    input->run = matroska_side_data_run;
    return 0;
}
