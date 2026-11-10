/* Public-API regression tests for the own entity adapter. Generate the AU
 * fixtures first, then pass the repository root as the only argument. */
#define main reference_tool_main
#include "reference.c"
#undef main
#include <assert.h>

static void test_input_boundaries(void)
{
    const uint8_t a[] = {1, 2}, b[] = {3, 4, 5};
    TsEntity entities[] = {{a, NULL, 2, 0, 0}, {b, NULL, 3, 2, 0}};
    TsEntities source = {entities, 2, 5};
    TsEntityInput input = {&source, 0, 0, 2, 0};
    uint8_t bytes[8] = {0};
    assert(ts_entity_read(&input, bytes, sizeof(bytes)) == 2);
    assert(ts_entity_read(&input, bytes + 2, sizeof(bytes) - 2) == 3);
    assert(!memcmp(bytes, (uint8_t[]){1, 2, 3, 4, 5}, 5));
    assert(ts_entity_read(&input, bytes, sizeof(bytes)) == AVERROR_EOF);
    input = (TsEntityInput){&source, 0, 0, 1, 0};
    assert(ts_entity_read(&input, bytes, sizeof(bytes)) == 2);
    assert(ts_entity_read(&input, bytes, sizeof(bytes)) == AVERROR(EAGAIN));
    assert(input.waiting && input.at == 2);
    assert(ts_entity_seek(&input, 3, SEEK_SET) == AVERROR(EAGAIN));
    assert(input.at == 2);
    assert(ts_entity_seek(&input, INT64_MIN, SEEK_CUR) == AVERROR(EINVAL));
    assert(ts_entity_seek(&input, INT64_MAX, SEEK_CUR) == AVERROR(EINVAL));
    assert(input.at == 2);
}

static void test_no_eof_drain_at_discontinuity(const char *root)
{
    const char *suffix = "/.research/fixtures-mpegts-access-units/au-dvb-ac3-one.ts";
    char *path = malloc(strlen(root) + strlen(suffix) + 1);
    assert(path);
    strcpy(path, root);
    strcat(path, suffix);
    FILE *file = fopen(path, "rb");
    assert(file && !fseek(file, 0, SEEK_END));
    long size = ftell(file);
    assert(size > 0);
    rewind(file);
    uint8_t *bytes = malloc((size_t)size);
    assert(bytes && fread(bytes, 1, (size_t)size, file) == (size_t)size);
    fclose(file);
    char *arguments[] = {"worker", "ts-au-fold", path, "188", "7",
                         "--discontinuity", "au-dvb-ac3-multiple-per-pes.ts"};
    TsEntities source = {0};
    assert(ts_entities_load(&source, 7, arguments, 3, bytes, (size_t)size, 1) == 0);
    TsWork work = {0};
    /* The old lone frame is still awaiting lookahead. A manufactured EOF would
     * emit it; a real discontinuity discards it before the next entity's frames. */
    assert(ts_entities_operation(&source, 7, 1, &work) == 1);
    assert(work.packets == 2 && work.bytes == 2 * 384);
    ts_entities_free(&source);
    free(bytes);
    free(path);
}

int main(int argc, char **argv)
{
    assert(argc == 2);
    av_log_set_level(AV_LOG_ERROR);
    test_input_boundaries();
    test_no_eof_drain_at_discontinuity(argv[1]);
    puts("MPEG-TS entity boundary regressions passed");
    return 0;
}
