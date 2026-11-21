/* Public reference operations for isobmff trace. */
static int isobmff_trace_prepare(ViewCase *input)
{
    fprintf(stderr, "%s operation is unavailable\n", input->prefix);
    return 2;
}
