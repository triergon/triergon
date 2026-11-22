static int playback_subtitles_prepare(ViewCase *input)
{
    fprintf(stderr, "usage: %s{fold,trace,memory} FILE [ARG...] | %sbench FILE ITERATIONS [ARG...]\n",
            input->prefix, input->prefix);
    return 2;
}

/* The wrapped link reports host-dependent subtitle folds. */
static int playback_subtitle_command(int argc, char **argv)
{
    (void)argc;
    (void)argv;
    fputs("usage: subtitle-bridge-{fold,trace,memory} FILE [ARG...] | subtitle-bridge-bench FILE ITERATIONS [ARG...]\n", stderr);
    return 2;
}
