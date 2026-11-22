static int playback_source_view_prepare(ViewCase *input)
{
    fprintf(stderr, "usage: %s{fold,trace,memory} FILE [ARG...] | %sbench FILE ITERATIONS [ARG...]\n",
            input->prefix, input->prefix);
    return 2;
}
