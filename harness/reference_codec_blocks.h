/* Preparation for the codec blocks operations. */
static int codec_blocks_prepare(ViewCase *input)
{
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
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
