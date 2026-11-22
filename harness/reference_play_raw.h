/* The decoder helpers and the views digest use different folds. */
#define main play_decode_main
#define fold decode_fold
#include "reference_decode.h"
#undef fold
#undef main
#include "reference_views_protocol.h"

static int play_raw_prepare(ViewCase *input)
{
    fprintf(stderr, "usage: %s{fold,trace,memory} FILE [ARG...] | %sbench FILE ITERATIONS [ARG...]\n",
            input->prefix, input->prefix);
    return 2;
}

static int play_raw_command(int argc, char **argv)
{
    (void)argc;
    (void)argv;
    fputs("usage: play-raw FILE VIDEO AUDIO PACKETS [WxH]\n", stderr);
    return 2;
}
