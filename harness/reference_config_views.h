/* Public reference operations for config views. */
static int config_views_prepare(ViewCase *input)
{
    fprintf(stderr, "%s operation is unavailable\n", input->prefix);
    return 2;
}
