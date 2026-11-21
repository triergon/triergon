/* Compare a session discontinuity using the public decoder flush API. */
#define main reference_decode_main
#include "reference_decode.h"
#undef main

#include <inttypes.h>
#include <libavutil/mathematics.h>

struct session_output {
    struct out device;
    AVFilterGraph *graph;
    AVFilterContext *src;
    AVFilterContext *sink;
    AVRational time_base;
    int video;
    int planar;
};

static void write_float(FILE *out, const AVFrame *frame) {
    const int channels = frame->ch_layout.nb_channels;
    const float *interleaved = (const float *)frame->extended_data[0];
    if (frame->format == AV_SAMPLE_FMT_FLT) {
        if (fwrite(interleaved, sizeof(*interleaved), (size_t)frame->nb_samples * channels, out)
            != (size_t)frame->nb_samples * channels) die("write PCM", AVERROR(EIO));
    } else if (frame->format == AV_SAMPLE_FMT_FLTP) {
        for (int i = 0; i < frame->nb_samples; i++)
            for (int c = 0; c < channels; c++) {
                const float *plane = (const float *)frame->extended_data[c];
                if (fwrite(&plane[i], sizeof(*plane), 1, out) != 1)
                    die("write PCM", AVERROR(EIO));
            }
    } else {
        die("expected float decoder output", AVERROR(EINVAL));
    }
}

static void emit_frame(struct session_output *output, AVCodecContext *decoder, AVFrame *frame) {
    int64_t pts = frame->pts;
    if (pts == AV_NOPTS_VALUE)
        fprintf(stderr, "{\"pts_ns\":null,\"nb_samples\":%d}\n", frame->nb_samples);
    else
        fprintf(stderr, "{\"pts_ns\":%" PRId64 ",\"nb_samples\":%d}\n",
                av_rescale_q(pts, output->time_base, (AVRational){1, 1000000000}),
                frame->nb_samples);
    if (output->planar) {
        write_float(output->device.raw, frame);
        return;
    }
    if (!output->graph)
        output->graph = graph_for(decoder, frame, output->video, &output->src, &output->sink);
    int rc = av_buffersrc_add_frame(output->src, frame);
    if (rc < 0) die("buffer input", rc);
    sink_frames(output->sink, &output->device, output->video);
}

static void decode_packet(struct session_output *output, AVCodecContext *decoder,
                          AVFrame *frame, const AVPacket *packet) {
    int rc = avcodec_send_packet(decoder, packet);
    if (rc == AVERROR_INVALIDDATA) {
        fprintf(stderr, "decode rejected packet\n");
        return;
    }
    if (rc < 0) die("send packet", rc);
    for (;;) {
        rc = avcodec_receive_frame(decoder, frame);
        if (rc == AVERROR(EAGAIN) || rc == AVERROR_EOF) return;
        if (rc == AVERROR_INVALIDDATA) {
            fprintf(stderr, "decode rejected frame\n");
            return;
        }
        if (rc < 0) die("receive frame", rc);
        emit_frame(output, decoder, frame);
        av_frame_unref(frame);
    }
}

static int reset_at(const char *indices, uint64_t packet_index) {
    while (*indices) {
        char *end;
        uint64_t index = strtoull(indices, &end, 10);
        if (end == indices || (*end && *end != ',')) die("reset index", AVERROR(EINVAL));
        if (index == packet_index) return 1;
        indices = *end ? end + 1 : end;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr, "usage: reference-session FORMAT INPUT OUTPUT planar|device RESET_INDEXES\n");
        return 2;
    }
    const AVInputFormat *demuxer = av_find_input_format(argv[1]);
    int video = !strcmp(argv[1], "h264") || !strcmp(argv[1], "hevc");
    if (!demuxer) die("demuxer", AVERROR_DEMUXER_NOT_FOUND);
    AVFormatContext *format = NULL;
    AVDictionary *options = NULL;
    int raw_mov = !strcmp(argv[1], "mov");
    if (raw_mov) av_dict_set(&options, "ignore_editlist", "1", 0);
    int rc = avformat_open_input(&format, argv[2], demuxer, &options);
    av_dict_free(&options);
    if (rc < 0) die("open input", rc);
    if ((rc = avformat_find_stream_info(format, NULL)) < 0) die("read stream info", rc);
    const AVCodec *codec = NULL;
    int index = av_find_best_stream(format, video ? AVMEDIA_TYPE_VIDEO : AVMEDIA_TYPE_AUDIO,
                                    -1, -1, &codec, 0);
    if (index < 0) die("select stream", index);
    AVStream *stream = format->streams[index];
    AVCodecContext *decoder = avcodec_alloc_context3(codec);
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    if (!decoder || !packet || !frame) die("allocate decoder state", AVERROR(ENOMEM));
    if ((rc = avcodec_parameters_to_context(decoder, stream->codecpar)) < 0)
        die("codec parameters", rc);
    decoder->thread_count = 1;
    decoder->pkt_timebase = stream->time_base;
    if (raw_mov) av_dict_set(&options, "flags2", "+skip_manual", 0);
    rc = avcodec_open2(decoder, codec, &options);
    av_dict_free(&options);
    if (rc < 0) die("open decoder", rc);
    struct session_output output = {
        .device.raw = !strcmp(argv[3], "-") ? stdout : fopen(argv[3], "wb"),
        .time_base = stream->time_base,
        .video = video,
        .planar = !strcmp(argv[4], "planar"),
    };
    if (!output.device.raw) die("open output", AVERROR(errno));
    uint64_t packet_index = 0;
    while ((rc = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index == index) {
            if (reset_at(argv[5], packet_index++)) avcodec_flush_buffers(decoder);
            decode_packet(&output, decoder, frame, packet);
        }
        av_packet_unref(packet);
    }
    if (rc != AVERROR_EOF) die("read packet", rc);
    decode_packet(&output, decoder, frame, NULL);
    if (output.graph) {
        if ((rc = av_buffersrc_add_frame(output.src, NULL)) < 0) die("drain graph", rc);
        sink_frames(output.sink, &output.device, video);
        avfilter_graph_free(&output.graph);
    }
    if (fclose(output.device.raw)) die("close output", AVERROR(errno));
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    avformat_close_input(&format);
    return 0;
}
