/* Compare decoded audio in the format of each frame. */
#define main reference_decode_main
#include "../../../harness/reference_decode.h"
#undef main

struct native_output {
    FILE *pcm;
    struct out device;
    AVFrame *previous;
    AVFilterGraph *graph;
    AVFilterContext *src;
    AVFilterContext *sink;
};

static void drain_device(struct native_output *output) {
    if (!output->graph) return;
    int rc = av_buffersrc_add_frame(output->src, NULL);
    if (rc < 0) die("drain device", rc);
    sink_frames(output->sink, &output->device, 0);
    avfilter_graph_free(&output->graph);
}

static void emit(struct native_output *output, AVCodecContext *decoder, AVFrame *frame) {
    const int channels = frame->ch_layout.nb_channels;
    const int planar = av_sample_fmt_is_planar(frame->format);
    const enum AVSampleFormat packed = av_get_packed_sample_fmt(frame->format);
    char layout[128];
    av_channel_layout_describe(&frame->ch_layout, layout, sizeof(layout));
    fprintf(stderr, "{\"rate\":%d,\"channels\":%d,\"layout\":\"%s\",\"samples\":%d}\n",
            frame->sample_rate, channels, layout, frame->nb_samples);
    for (int i = 0; i < frame->nb_samples; i++) {
        for (int c = 0; c < channels; c++) {
            const void *plane = frame->extended_data[planar ? c : 0];
            const size_t offset = planar ? (size_t)i : (size_t)i * channels + c;
            float value;
            switch (packed) {
            case AV_SAMPLE_FMT_FLT: value = ((const float *)plane)[offset]; break;
            case AV_SAMPLE_FMT_S16: value = ((const int16_t *)plane)[offset] / 32768.0F; break;
            case AV_SAMPLE_FMT_S32: value = ((const int32_t *)plane)[offset] / 2147483648.0F; break;
            default: die("native PCM format", AVERROR(EINVAL)); return;
            }
            if (fwrite(&value, sizeof(value), 1, output->pcm) != 1)
                die("write native PCM", AVERROR(EIO));
        }
    }
    const AVFrame *previous = output->previous;
    if (!output->graph || previous->format != frame->format ||
        previous->sample_rate != frame->sample_rate ||
        av_channel_layout_compare(&previous->ch_layout, &frame->ch_layout)) {
        drain_device(output);
        av_frame_unref(output->previous);
        const int rc = av_frame_ref(output->previous, frame);
        if (rc < 0) die("retain segment format", rc);
        output->graph = graph_for(decoder, frame, 0, &output->src, &output->sink);
    }
    const int rc = av_buffersrc_add_frame(output->src, frame);
    if (rc < 0) die("device input", rc);
    sink_frames(output->sink, &output->device, 0);
}

static void receive(struct native_output *output, AVCodecContext *decoder, AVFrame *frame,
                    const AVPacket *packet) {
    int rc = avcodec_send_packet(decoder, packet);
    if (rc < 0) die("send packet", rc);
    for (;;) {
        rc = avcodec_receive_frame(decoder, frame);
        if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) return;
        if (rc < 0) die("receive frame", rc);
        emit(output, decoder, frame);
        av_frame_unref(frame);
    }
}

int main(int argc, char **argv) {
    if (argc != 5) return 2;
    const AVInputFormat *demuxer = av_find_input_format(argv[1]);
    if (!demuxer) die("demuxer", AVERROR_DEMUXER_NOT_FOUND);
    AVFormatContext *source = NULL;
    int rc = avformat_open_input(&source, argv[2], demuxer, NULL);
    if (rc < 0) die("open input", rc);
    const AVStream *stream = NULL;
    for (unsigned i = 0; i < source->nb_streams; i++) {
        if (source->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_AUDIO) {
            stream = source->streams[i];
            break;
        }
    }
    if (!stream) die("audio stream", AVERROR_STREAM_NOT_FOUND);
    const AVCodec *codec = avcodec_find_decoder_by_name(!strcmp(argv[1], "flac") ? "flac" : "mp3float");
    AVCodecContext *decoder = avcodec_alloc_context3(codec);
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    struct native_output output = {
        .pcm = fopen(argv[3], "wb"),
        .device.raw = fopen(argv[4], "wb"),
        .previous = av_frame_alloc(),
    };
    if (!decoder || !packet || !frame || !output.previous) die("allocate decode state", AVERROR(ENOMEM));
    if (!output.pcm || !output.device.raw) die("open output", AVERROR(errno));
    rc = avcodec_parameters_to_context(decoder, stream->codecpar);
    if (rc < 0) die("codec parameters", rc);
    decoder->thread_count = 1;
    decoder->pkt_timebase = stream->time_base;
    rc = avcodec_open2(decoder, codec, NULL);
    if (rc < 0) die("open decoder", rc);
    while ((rc = av_read_frame(source, packet)) >= 0) {
        if (packet->stream_index == stream->index) receive(&output, decoder, frame, packet);
        av_packet_unref(packet);
    }
    if (rc != AVERROR_EOF) die("read packet", rc);
    receive(&output, decoder, frame, NULL);
    drain_device(&output);
    if (fclose(output.pcm) || fclose(output.device.raw)) die("close output", AVERROR(errno));
    av_frame_free(&output.previous);
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    avformat_close_input(&source);
    return 0;
}
