/* Public libavutil CRC and rescale operations. */
#include <errno.h>
#include <limits.h>
#include <libavutil/bswap.h>
#include <libavutil/crc.h>
#include <libavutil/mathematics.h>

typedef struct {
    AVCRCId id;
    uint32_t initial;
    size_t split;
    int incremental;
} CodecCrc;

static int codec_crc_run(const ViewCase *input, ViewWork *work)
{
    const CodecCrc *crc = input->prepared;
    const AVCRC *table = av_crc_get_table(crc->id);
    if (!table)
        return 2;
    uint32_t result = av_crc(table, crc->initial, input->data, crc->split);
    if (crc->incremental) {
        const size_t tail = input->size - crc->split;
        if (crc->id == AV_CRC_32_IEEE && tail) {
            result = av_crc(table, result, input->data + crc->split, 1);
            result = av_crc(table, result, input->data + crc->split + 1, tail - 1);
        } else {
            result = av_crc(table, result, input->data + crc->split, tail);
        }
    }
    if (crc->id == AV_CRC_32_IEEE)
        result = av_bswap32(result);
    else if (crc->id == AV_CRC_32_IEEE_LE)
        result = ~result;
    else if (crc->id == AV_CRC_16_ANSI)
        result = av_bswap16((uint16_t)result);
    const uint64_t words[] = {input->size, result};
    view_record(work, 3, words, 2, NULL, 0);
    if (work->trace)
        printf("3 %zu %" PRIu32 "\n", input->size, result);
    return 0;
}

static int codec_crc_prepare(ViewCase *input)
{
    if (input->argc != 2)
        return 2;
    const char *name = input->argv[0];
    CodecCrc crc = {.split = input->size};
    if (!strcmp(name, "mpeg") || !strcmp(name, "ogg")) {
        crc.id = AV_CRC_32_IEEE;
        crc.initial = !strcmp(name, "mpeg") ? UINT32_MAX : 0;
    } else if (!strcmp(name, "ieee")) {
        crc.id = AV_CRC_32_IEEE_LE;
        crc.initial = UINT32_MAX;
    } else if (!strcmp(name, "flac-header")) {
        crc.id = AV_CRC_8_ATM;
    } else if (!strcmp(name, "flac-ac3") || !strcmp(name, "mpeg-audio")) {
        crc.id = AV_CRC_16_ANSI;
        crc.initial = !strcmp(name, "mpeg-audio") ? UINT16_MAX : 0;
    } else {
        return 2;
    }
    const char *schedule = input->argv[1];
    if (strcmp(schedule, "whole")) {
        if (!*schedule || strspn(schedule, "0123456789") != strlen(schedule))
            return 2;
        errno = 0;
        unsigned long split = strtoul(schedule, NULL, 10);
        if (errno || split > input->size)
            return 2;
        crc.split = (size_t)split;
        crc.incremental = 1;
    }
    CodecCrc *prepared = malloc(sizeof(*prepared));
    if (!prepared)
        return 2;
    *prepared = crc;
    input->prepared = prepared;
    input->release = free;
    input->run = codec_crc_run;
    input->name = "crc-values";
    return 0;
}

typedef struct {
    AVRational from, to;
    size_t count;
    int64_t ticks[];
} CodecRescale;

static int codec_rescale_base(const char *text, AVRational *base)
{
    const char *slash = strchr(text, '/');
    if (!slash || slash == text || !slash[1] ||
        strspn(text, "0123456789") != (size_t)(slash - text) ||
        strspn(slash + 1, "0123456789") != strlen(slash + 1))
        return 0;
    errno = 0;
    const long num = strtol(text, NULL, 10), den = strtol(slash + 1, NULL, 10);
    if (errno || num < 1 || num > INT_MAX || den < 1 || den > INT_MAX)
        return 0;
    *base = (AVRational){(int)num, (int)den};
    return 1;
}

static int codec_rescale_run(const ViewCase *input, ViewWork *work)
{
    const CodecRescale *rescale = input->prepared;
    const enum AVRounding modes[] = {
        AV_ROUND_ZERO, AV_ROUND_INF, AV_ROUND_DOWN, AV_ROUND_UP, AV_ROUND_NEAR_INF
    };
    for (size_t i = 0; i < rescale->count; ++i) {
        const int64_t tick = rescale->ticks[i];
        for (size_t mode = 0; mode < sizeof(modes) / sizeof(*modes); ++mode) {
            const int64_t result = av_rescale_q_rnd(tick, rescale->from, rescale->to, modes[mode]);
            const uint64_t words[] = {(uint64_t)tick, mode, (uint64_t)result};
            if (result == INT64_MIN) {
                view_record(work, 8, words, 2, NULL, 0);
                if (work->trace)
                    printf("8 %" PRId64 " %zu\n", tick, mode);
            } else {
                view_record(work, 3, words, 3, NULL, 0);
                if (work->trace)
                    printf("3 %" PRId64 " %zu %" PRId64 "\n", tick, mode, result);
            }
        }
    }
    return 0;
}

static int codec_rescale_prepare(ViewCase *input)
{
    AVRational from, to;
    if (input->argc != 3 ||
        !codec_rescale_base(input->argv[0], &from) ||
        !codec_rescale_base(input->argv[1], &to) ||
        (strcmp(input->argv[2], "direct") && strcmp(input->argv[2], "prepared")) ||
        input->size % sizeof(int64_t))
        return 2;
    CodecRescale *prepared = malloc(sizeof(*prepared) + input->size);
    if (!prepared)
        return 2;
    *prepared = (CodecRescale){.from = from, .to = to, .count = input->size / sizeof(int64_t)};
    /* Both gate targets use little-endian two's-complement integers. */
    memcpy(prepared->ticks, input->data, input->size);
    input->prepared = prepared;
    input->release = free;
    input->run = codec_rescale_run;
    input->name = "time-rescale";
    return 0;
}

static int codec_blocks_prepare(ViewCase *input)
{
    if (!strcmp(input->prefix, "crc-"))
        return codec_crc_prepare(input);
    if (!strcmp(input->prefix, "rescale-"))
        return codec_rescale_prepare(input);
    return 2;
}

#include <libavutil/error.h>
#include <libavutil/mem.h>
#include <libavutil/tx.h>

/*
 * SPEC begins with little-endian u32 N and f64 scale, then N/2 f32
 * coefficients. The parity driver normalizes this raw middle half.
 * Both gate targets are little-endian.
 */
static int codec_blocks_raw(int argc, char **argv)
{
    if (argc != 4) {
        fputs("usage: dsp-raw SPEC OUT\n", stderr);
        return 2;
    }

    FILE *input = fopen(argv[2], "rb");
    uint32_t n;
    double scale_word;
    if (!input)
        return 2;
    int read_ok = fread(&n, sizeof(n), 1, input) == 1 &&
                  fread(&scale_word, sizeof(scale_word), 1, input) == 1 &&
                  n >= 4 && n <= 8192 && n % 4 == 0;
    if (!read_ok) {
        fclose(input);
        return 2;
    }

    const size_t k = n / 2;
    const float scale = (float)scale_word;
    float *coefficients = av_malloc_array(k, sizeof(float));
    float *samples = av_malloc_array(k, sizeof(float));
    AVTXContext *context = NULL;
    av_tx_fn transform = NULL;
    int status = 2;
    read_ok = coefficients && samples &&
              fread(coefficients, sizeof(float), k, input) == k &&
              fgetc(input) == EOF && !ferror(input);
    if (fclose(input) || !read_ok)
        goto done;
    const int init = av_tx_init(&context, &transform, AV_TX_FLOAT_MDCT,
                                1, (int)k, &scale, 0);
    if (init < 0) {
        fprintf(stderr, "av_tx_init refused length %u with code %d\n", n, init);
        status = init == AVERROR(EINVAL) || init == AVERROR(ENOSYS) ? 3 : 2;
        goto done;
    }
    transform(context, samples, coefficients, sizeof(float));

    FILE *output = fopen(argv[3], "wb");
    if (output) {
        const int wrote = fwrite(samples, sizeof(float), k, output) == k;
        status = fclose(output) || !wrote ? 2 : 0;
    }

done:
    av_tx_uninit(&context);
    av_free(samples);
    av_free(coefficients);
    return status;
}
