/* The decoder helpers and the views digest use different folds. */
#define main play_decode_main
#define fold decode_fold
#include "reference_decode.h"
#undef fold
#undef main
#include "reference_views_protocol.h"
#include <libavutil/md5.h>

typedef struct {
    int width, height, video, audio;
} PlayOptions;

typedef struct {
    AVCodecContext *decoder;
    AVStream *stream;
    AVFilterGraph *graph;
    AVFilterContext *source, *sink;
} PlayTrack;

typedef struct {
    FILE *video, *audio, *packets;
    ViewWork *work;
    int trace;
} PlayOutput;

static int play_options(int count, char **args, PlayOptions *options)
{
    *options = (PlayOptions){1920, 1080, -1, -1};
    if (!count)
        return 0;
    int used = 0;
    if ((count != 1 && count != 3) ||
        sscanf(args[0], "%dx%d%n", &options->width, &options->height, &used) != 2 ||
        args[0][used] || options->width < 1 || options->height < 1 ||
        options->width > 8192 || options->height > 8192)
        return 2;
    if (count == 3) {
        int *tracks[] = {&options->video, &options->audio};
        for (int i = 0; i < 2; i++) {
            const char *arg = args[i + 1];
            if (!strcmp(arg, "-"))
                continue;
            if (!*arg || strspn(arg, "0123456789") != strlen(arg))
                return 2;
            long index = strtol(arg, NULL, 10);
            if (index < 0 || index > INT_MAX)
                return 2;
            *tracks[i] = (int)index;
        }
    }
    return 0;
}

static int64_t play_seek(void *opaque, int64_t offset, int whence)
{
    struct memory_input *input = opaque;
    if (whence == AVSEEK_SIZE)
        return (int64_t)input->size;
    whence &= ~AVSEEK_FORCE;
    int64_t base;
    switch (whence) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (int64_t)input->at; break;
    case SEEK_END: base = (int64_t)input->size; break;
    default: return AVERROR(EINVAL);
    }
    if (offset < -base || offset > (int64_t)input->size - base)
        return AVERROR(EINVAL);
    input->at = (size_t)(base + offset);
    return (int64_t)input->at;
}

static void play_track_open(PlayTrack *track, AVFormatContext *format,
                            enum AVMediaType type, int index)
{
    if (index < 0) {
        index = av_find_best_stream(format, type, -1, -1, NULL, 0);
        if (type == AVMEDIA_TYPE_AUDIO && index == AVERROR_STREAM_NOT_FOUND)
            return;
    }
    if (index < 0 || (unsigned)index >= format->nb_streams ||
        format->streams[index]->codecpar->codec_type != type)
        die("play track", AVERROR_STREAM_NOT_FOUND);
    track->stream = format->streams[index];
    const AVCodec *codec = avcodec_find_decoder(track->stream->codecpar->codec_id);
    track->decoder = avcodec_alloc_context3(codec);
    if (!track->decoder)
        die("play decoder allocation", AVERROR(ENOMEM));
    int result = avcodec_parameters_to_context(track->decoder, track->stream->codecpar);
    if (result < 0)
        die("play decoder parameters", result);
    track->decoder->pkt_timebase = track->stream->time_base;
    track->decoder->thread_count = 1;
    result = avcodec_open2(track->decoder, codec, NULL);
    if (result < 0)
        die("play decoder open", result);
}

static void play_video_graph(PlayTrack *track, const AVFrame *frame, const PlayOptions *options)
{
    track->graph = avfilter_graph_alloc();
    if (!track->graph)
        die("play video graph allocation", AVERROR(ENOMEM));
    track->graph->nb_threads = 1;
    char args[256];
    AVRational sar = frame->sample_aspect_ratio;
    snprintf(args, sizeof args,
             "video_size=%dx%d:pix_fmt=%d:time_base=1/1000000000:pixel_aspect=%d/%d:colorspace=%d:range=%d",
             frame->width, frame->height, frame->format, sar.num ? sar.num : 1,
             sar.den ? sar.den : 1, frame->colorspace, frame->color_range);
    int result = avfilter_graph_create_filter(&track->source, avfilter_get_by_name("buffer"),
                                               "in", args, NULL, track->graph);
    if (result < 0)
        die("play video buffer", result);
    result = avfilter_graph_create_filter(&track->sink, avfilter_get_by_name("buffersink"),
                                         "out", NULL, NULL, track->graph);
    if (result < 0)
        die("play video sink", result);
    AVBufferSrcParameters *params = av_buffersrc_parameters_alloc();
    if (!params)
        die("play video parameters", AVERROR(ENOMEM));
    params->side_data = frame->side_data;
    params->nb_side_data = frame->nb_side_data;
    result = av_buffersrc_parameters_set(track->source, params);
    av_free(params);
    if (result < 0)
        die("play video source", result);
    AVFilterInOut *inputs = avfilter_inout_alloc(), *outputs = avfilter_inout_alloc();
    if (!inputs || !outputs)
        die("play filter attachments", AVERROR(ENOMEM));
    outputs->name = av_strdup("in");
    outputs->filter_ctx = track->source;
    inputs->name = av_strdup("out");
    inputs->filter_ctx = track->sink;
    char chain[160];
    snprintf(chain, sizeof chain,
             "scale=%d:%d:force_original_aspect_ratio=decrease,pad=%d:%d:(ow-iw)/2:(oh-ih)/2,format=rgba",
             options->width, options->height, options->width, options->height);
    result = avfilter_graph_parse_ptr(track->graph, chain, &inputs, &outputs, NULL);
    avfilter_inout_free(&inputs);
    avfilter_inout_free(&outputs);
    if (result < 0 || (result = avfilter_graph_config(track->graph, NULL)) < 0)
        die("play video filters", result);
}

static void play_write(FILE *file, const void *bytes, size_t length)
{
    if (fwrite(bytes, 1, length, file) != length)
        die("play output write", AVERROR(EIO));
}

static const char *play_tick(char *text, int64_t value)
{
    if (value == AV_NOPTS_VALUE)
        return "-";
    snprintf(text, 24, "%" PRId64, value);
    return text;
}

static int64_t play_rescale(int64_t value, AVRational from, AVRational to)
{
    return av_rescale_q_rnd(value, from, to, AV_ROUND_DOWN | AV_ROUND_PASS_MINMAX);
}

static void play_packet(const AVPacket *packet, const AVFormatContext *format,
                        PlayOutput *output)
{
    AVRational source = format->streams[packet->stream_index]->time_base;
    AVRational base = !strcmp(format->iformat->name, "matroska,webm")
        ? (AVRational){1, 1000000000}
        : source;
    int64_t pts = play_rescale(packet->pts, source, base);
    int64_t dts = play_rescale(packet->dts, source, base);
    int64_t duration = packet->duration
        ? play_rescale(packet->duration, source, base)
        : AV_NOPTS_VALUE;
    unsigned flags = !!(packet->flags & AV_PKT_FLAG_KEY);
    if (output->packets) {
        uint8_t digest[16];
        char pts_text[24], dts_text[24], duration_text[24];
        av_md5_sum(digest, packet->data, (size_t)packet->size);
        fprintf(output->packets, "%d %d/%d %s %s %s %u %d ",
                packet->stream_index, base.num, base.den, play_tick(pts_text, pts),
                play_tick(dts_text, dts), play_tick(duration_text, duration), flags, packet->size);
        for (size_t i = 0; i < sizeof digest; i++)
            fprintf(output->packets, "%02x", digest[i]);
        fputc('\n', output->packets);
    } else {
        uint64_t words[] = {
            (uint64_t)packet->stream_index, (uint64_t)base.num, (uint64_t)base.den,
            (uint64_t)(pts), (uint64_t)(dts), (uint64_t)(duration),
            flags, (uint64_t)packet->size,
        };
        view_record(output->work, 1, words, 8, packet->data, (size_t)packet->size);
    }
}

static void play_frames(PlayTrack *track, PlayOutput *output, int video)
{
    AVFrame *frame = av_frame_alloc();
    if (!frame)
        die("play frame allocation", AVERROR(ENOMEM));
    for (;;) {
        int result = av_buffersink_get_frame(track->sink, frame);
        if (result == AVERROR(EAGAIN) || result == AVERROR_EOF)
            break;
        if (result < 0)
            die("play filtered frame", result);
        if (video) {
            char text[24];
            int64_t pts = play_rescale(frame->pts, av_buffersink_get_time_base(track->sink),
                                       (AVRational){1, 1000000000});
            if (output->trace)
                printf("v %s\n", play_tick(text, pts));
            size_t width = (size_t)frame->width * 4;
            uint64_t bytes = width * (size_t)frame->height;
            if (!output->video) {
                fold(&output->work->digest, 4);
                fold(&output->work->digest, (uint64_t)(pts));
                output->work->packets++;
                output->work->bytes += bytes;
            }
            for (int y = 0; y < frame->height; y++) {
                const uint8_t *row = frame->data[0] + (ptrdiff_t)y * frame->linesize[0];
                if (output->video)
                    play_write(output->video, row, width);
                else
                    fold_bytes(&output->work->digest, row, width);
            }
        } else {
            if (output->trace)
                printf("a %d %d\n", frame->sample_rate, frame->nb_samples);
            if (output->audio) {
                play_write(output->audio, frame->data[0], (size_t)frame->nb_samples * 4);
            } else {
                uint64_t rate = (uint64_t)frame->sample_rate;
                for (int i = 0; i < frame->nb_samples; i++)
                    view_record(output->work, 3, &rate, 1, frame->data[0] + (size_t)i * 4, 4);
            }
        }
        av_frame_unref(frame);
    }
    av_frame_free(&frame);
}

static void play_decode(PlayTrack *track, const AVPacket *packet,
                        const PlayOptions *options, PlayOutput *output, int video)
{
    if (!track->decoder)
        return;
    int result = avcodec_send_packet(track->decoder, packet);
    if (result < 0)
        die("play send packet", result);
    AVFrame *frame = av_frame_alloc();
    if (!frame)
        die("play frame allocation", AVERROR(ENOMEM));
    while ((result = avcodec_receive_frame(track->decoder, frame)) >= 0) {
        if (!track->graph) {
            if (video)
                play_video_graph(track, frame, options);
            else
                track->graph = graph_for(track->decoder, frame, 0, &track->source, &track->sink);
        }
        if (video)
            frame->pts = play_rescale(frame->pts, track->stream->time_base,
                                      (AVRational){1, 1000000000});
        else
            frame->pts = play_rescale(frame->pts, track->stream->time_base,
                                      (AVRational){1, frame->sample_rate});
        result = av_buffersrc_add_frame(track->source, frame);
        if (result < 0)
            die("play filter input", result);
        play_frames(track, output, video);
        av_frame_unref(frame);
    }
    av_frame_free(&frame);
    if (result != AVERROR(EAGAIN) && result != AVERROR_EOF)
        die("play receive frame", result);
    if (!packet && track->graph) {
        result = av_buffersrc_add_frame(track->source, NULL);
        if (result < 0)
            die("play filter drain", result);
        play_frames(track, output, video);
    }
}

static void play_file(const ViewCase *input, const PlayOptions *options, PlayOutput *output)
{
    struct memory_input source = {input->data, input->size, 0};
    const int window = 64 * 1024;
    uint8_t *buffer = av_malloc(window);
    AVIOContext *io = avio_alloc_context(buffer, window, 0, &source, memory_read, NULL, play_seek);
    AVFormatContext *format = avformat_alloc_context();
    if (!buffer || !io || !format)
        die("play input allocation", AVERROR(ENOMEM));
    format->pb = io;
    format->flags |= AVFMT_FLAG_CUSTOM_IO;
    int result = avformat_open_input(&format, NULL, NULL, NULL);
    if (result < 0)
        die("play demux open", result);
    PlayTrack video = {0}, audio = {0};
    /* The discovery pass populates codec parameters and reorder delay. */
    result = avformat_find_stream_info(format, NULL);
    if (result < 0)
        die("play stream discovery", result);
    play_track_open(&video, format, AVMEDIA_TYPE_VIDEO, options->video);
    play_track_open(&audio, format, AVMEDIA_TYPE_AUDIO, options->audio);
    AVPacket *packet = av_packet_alloc();
    if (!packet)
        die("play packet allocation", AVERROR(ENOMEM));
    while ((result = av_read_frame(format, packet)) >= 0) {
        play_packet(packet, format, output);
        if (packet->stream_index == video.stream->index)
            play_decode(&video, packet, options, output, 1);
        else if (audio.stream && packet->stream_index == audio.stream->index)
            play_decode(&audio, packet, options, output, 0);
        av_packet_unref(packet);
    }
    if (result != AVERROR_EOF)
        die("play read packet", result);
    play_decode(&video, NULL, options, output, 1);
    play_decode(&audio, NULL, options, output, 0);
    av_packet_free(&packet);
    avfilter_graph_free(&video.graph);
    avfilter_graph_free(&audio.graph);
    avcodec_free_context(&video.decoder);
    avcodec_free_context(&audio.decoder);
    avformat_close_input(&format);
    av_freep(&io->buffer);
    avio_context_free(&io);
}

static int play_raw_fold(const ViewCase *input, ViewWork *work)
{
    PlayOutput output = {.work = work, .trace = work->trace};
    play_file(input, input->prepared, &output);
    return 0;
}

static int play_raw_prepare(ViewCase *input)
{
    if (strcmp(input->prefix, "play-"))
        return 2;
    PlayOptions opts;
    if (play_options(input->argc, input->argv, &opts))
        return 2;
    PlayOptions *saved = malloc(sizeof(*saved));
    if (!saved)
        return 2;
    *saved = opts;
    input->prepared = saved;
    input->release = free;
    input->name = "play";
    input->run = play_raw_fold;
    return 0;
}

static int play_raw_command(int argc, char **argv)
{
    PlayOptions opts;
    if (argc < 6 || play_options(argc - 6, argv + 6, &opts)) {
        fputs("usage: play-raw FILE VIDEO AUDIO PACKETS [WxH [VIDEO_TRACK AUDIO_TRACK]]\n", stderr);
        return 2;
    }
    ViewCase input = {0};
    uint8_t *data = view_load(argv[2], &input.size);
    input.data = data;
    if (!data)
        die("play input read", AVERROR(EIO));
    PlayOutput output = {
        .video = fopen(argv[3], "wb"), .audio = fopen(argv[4], "wb"),
        .packets = fopen(argv[5], "wb"), .trace = 1,
    };
    if (!output.video || !output.audio || !output.packets)
        die("play output open", AVERROR(EIO));
    play_file(&input, &opts, &output);
    int result = fclose(output.video) | fclose(output.audio) | fclose(output.packets);
    free(data);
    return result ? 2 : 0;
}
