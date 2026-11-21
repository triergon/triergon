/* Preparation for the codec blocks operations. */
static int codec_blocks_prepare(ViewCase *input)
{
    fprintf(stderr, "usage: %s operation is unavailable\n", input->prefix);
    return 2;
}

static int codec_blocks_raw(int argc, char **argv)
{
    (void)argc;
    (void)argv;
    fputs("usage: dsp-raw SPEC OUT (operation is unavailable)\n", stderr);
    return 2;
}
