/*
 * ASS rendering reference: libass 0.17.5's ass_render_frame through its public
 * API, as a player embeds it, with the bitmap engine set through the renderer's
 * internals. Included by reference_ass.h, whose fixture
 * reader, stream commands and libass build it shares (tooling only; never
 * linked into the library).
 *
 * Usage:
 *   reference ass-render-fold   FILE MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H
 *   reference ass-render-bench  FILE ITERATIONS MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H
 *   reference ass-render-memory FILE MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H
 *   reference ass-render-dump   FILE MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H
 *
 * MODE is `script` (FILE is a whole script, read with ass_read_memory) or
 * `chunks` (FILE is a stream fixture whose commands run on a track from
 * ass_new_track, as reference_ass.h runs them). FONTS is the directory the
 * fixture's font paths are relative to. The fixture's directives say which
 * fonts a player attaches and what it falls back to:
 *
 *   ;@attach PATH            a font file given to ass_add_font, in order
 *   ;@default-family NAME    ass_set_fonts' default family
 *   ;@default-font PATH      ass_set_fonts' default font file
 *   ;@frames START STEP END  a whole script's frames: ass_render_frame at
 *                            START, START+STEP, ... up to END milliseconds
 *
 * and a stream's `@frames START STEP END` command renders those frames at that
 * point of the stream, after the packets and prunes before it.
 *
 * One operation: a library with font extraction on and the attached fonts, a
 * renderer with libass's plain C bitmap engine (the one a 32-bit ARMv7 build
 * runs, on every host) and no system font provider (ASS_FONTPROVIDER_NONE), the given
 * frame and storage size and no hinting, the script read or the stream run,
 * and every frame rendered with libass's cache limits set to 1000 glyphs and
 * 24 MiB. Each frame folds its time, the change ass_render_frame reports (0,
 * 1 or 2) and its image count, then
 * each image its type, position, size, colour and every row of alpha bytes, in
 * list order; `packets` counts images and `bytes` their alpha bytes. The box
 * libass draws behind a BorderStyle 4 event has no type (add_background leaves
 * it uninitialised): it is told by its buffer (reference_ass.h) and folded
 * without one, and dumped with `-` for it. The file and the fonts are read
 * before the operation. `ass-render-dump` prints the same values as lines:
 *   frame TIME CHANGED IMAGES
 *   image TYPE X Y W H RRGGBBAA ROW-HEX...
 */

#define ASSR_MAX_FONTS 64
#define ASSR_MAX_FRAMES 64
#define ASSR_MAX_FONT_SIZE (64L << 20)

typedef struct {
    long long start, step, end;
} AssFrames;

typedef struct {
    char *path;
    char *data;
    size_t size;
} AssFontFile;

typedef struct {
    AssFontFile fonts[ASSR_MAX_FONTS];
    size_t n_fonts;
    char *default_family;
    char *default_font;
    AssFrames frames[ASSR_MAX_FRAMES];
    size_t n_frames;
    int frame_w, frame_h, storage_w, storage_h;
} AssRenderCase;

typedef struct {
    uint64_t packets, bytes, digest;
    FILE *dump;
    int unknown; /* an image's kind could not be told */
} AssImages;

static void assr_render_fold(AssImages *w, uint64_t v) { w->digest = w->digest * 31 + v; }

/* The exact ordered polynomial of crates/harness-protocol `fold_bytes`, regrouped. */
static uint32_t assr_fold_four(const uint8_t *b) {
    return (uint32_t)b[0] * UINT32_C(29791) + (uint32_t)b[1] * UINT32_C(961) + (uint32_t)b[2] * UINT32_C(31) +
           (uint32_t)b[3];
}
static uint64_t assr_fold_eight(const uint8_t *b) {
    return (uint64_t)assr_fold_four(b) * UINT64_C(923521) + (uint64_t)assr_fold_four(b + 4);
}
static void assr_fold_row(uint64_t *digest, const uint8_t *bytes, size_t n) {
    size_t i = 0;
    if (n >= 256) {
        const uint64_t step = UINT64_C(3671467063254694913);
        size_t exponent = n / 32;
        uint64_t power = 1, base = step, lanes[4] = {0, 0, 0, 0};
        while (exponent) {
            if (exponent & 1) power *= base;
            base *= base;
            exponent >>= 1;
        }
        for (; n - i >= 32; i += 32)
            for (unsigned j = 0; j < 4; j++) lanes[j] = lanes[j] * step + assr_fold_eight(bytes + i + j * 8);
        *digest = *digest * power + lanes[0] * UINT64_C(13349005091172011265) +
                  lanes[1] * UINT64_C(12662688596514954753) + lanes[2] * UINT64_C(852891037441) + lanes[3];
    }
    for (; n - i >= 8; i += 8) *digest = *digest * UINT64_C(852891037441) + assr_fold_eight(bytes + i);
    for (; i < n; i++) *digest = *digest * 31 + bytes[i];
}

static void assr_frame(AssImages *w, long long now, int changed, ASS_Image *images) {
    size_t count = 0;
    for (ASS_Image *image = images; image; image = image->next) count++;
    if (w->dump) {
        fprintf(w->dump, "frame %lld %d %zu\n", now, changed, count);
    } else {
        assr_render_fold(w, (uint64_t)now);
        assr_render_fold(w, (uint64_t)(int64_t)changed);
        assr_render_fold(w, count);
    }
    for (ASS_Image *image = images; image; image = image->next) {
        /* A BorderStyle 4 box: its own buffer, no composite behind it. */
        int box = assr_is_box(image->bitmap);
        if (box && ((ASS_ImagePriv *)image)->source) w->unknown = 1;
        if (w->dump) {
            if (box)
                fprintf(w->dump, "image - %d %d %d %d %08" PRIx32, image->dst_x, image->dst_y, image->w, image->h,
                        image->color);
            else
                fprintf(w->dump, "image %d %d %d %d %d %08" PRIx32, image->type, image->dst_x, image->dst_y,
                        image->w, image->h, image->color);
            for (int y = 0; y < image->h; y++) {
                fputc(' ', w->dump);
                for (int x = 0; x < image->w; x++)
                    fprintf(w->dump, "%02x", image->bitmap[(size_t)y * (size_t)image->stride + (size_t)x]);
            }
            fputc('\n', w->dump);
            continue;
        }
        const uint64_t fields[] = {(uint64_t)image->type,   (uint64_t)(int64_t)image->dst_x,
                                   (uint64_t)(int64_t)image->dst_y, (uint64_t)image->w,
                                   (uint64_t)image->h,      image->color};
        for (size_t i = box ? 1 : 0; i < sizeof fields / sizeof fields[0]; i++) assr_render_fold(w, fields[i]);
        for (int y = 0; y < image->h; y++)
            assr_fold_row(&w->digest, image->bitmap + (size_t)y * (size_t)image->stride, (size_t)image->w);
        w->packets++;
        w->bytes += (uint64_t)image->w * (uint64_t)image->h;
    }
}

static void assr_frames(AssImages *w, ASS_Renderer *r, ASS_Track *t, AssFrames f) {
    for (long long now = f.start; now <= f.end; now += f.step) {
        int changed = -1;
        ASS_Image *images = ass_render_frame(r, t, now, &changed);
        assr_frame(w, now, changed, images);
    }
}

/* One operation; 0 when libass could not be set up. */
static int assr_render(AssImages *w, const AssRenderCase *c, const char *data, size_t size, int stream,
                       const AssFixture *f) {
    int ok = 0;
    ASS_Renderer *r = NULL;
    ASS_Track *track = NULL;
    ASS_Library *lib = ass_library_init();
    if (!lib) return 0;
    ass_set_message_cb(lib, assr_quiet, NULL);
    ass_set_extract_fonts(lib, 1);
    for (size_t i = 0; i < c->n_fonts; i++)
        ass_add_font(lib, c->fonts[i].path, c->fonts[i].data, (int)c->fonts[i].size);
    r = ass_renderer_init(lib);
    if (!r) goto done;
    /* libass's plain C engine on every host, as reference_raster.h sets it:
     * ass_renderer_init picks the host's SIMD engine (AVX2's 32-byte rows
     * change \be at a bitmap's right edge), so the C engine replaces it and
     * the rasteriser state is rebuilt for it. It is what a 32-bit ARMv7 build
     * runs: 16x16 tiles and 16-byte rows. */
    r->engine = ass_bitmap_engine_init(ASS_CPU_FLAG_NONE);
    if (r->engine.tile_order != 4 || r->engine.align_order != 4) {
        fprintf(stderr, "the C engine is not 16x16 tiles with 16-byte rows\n");
        goto done;
    }
    ass_rasterizer_done(&r->state.rasterizer);
    if (!ass_rasterizer_init(&r->engine, &r->state.rasterizer, RASTERIZER_PRECISION)) goto done;
    ass_set_frame_size(r, c->frame_w, c->frame_h);
    ass_set_storage_size(r, c->storage_w, c->storage_h);
    ass_set_hinting(r, ASS_HINTING_NONE);
    /* A player's cache limits: 1000 outlines, 24 MiB of bitmaps and
     * composites. */
    ass_set_cache_limits(r, 1000, 24);
    ass_set_fonts(r, c->default_font, c->default_family, ASS_FONTPROVIDER_NONE, NULL, 0);
    if (!stream) {
        /* libass may modify the buffer it reads. */
        char *copy = malloc(size + 1);
        if (!copy) goto done;
        memcpy(copy, data, size);
        copy[size] = 0;
        track = ass_read_memory(lib, copy, size, NULL);
        free(copy);
        for (size_t i = 0; track && i < c->n_frames; i++) assr_frames(w, r, track, c->frames[i]);
    } else {
        track = ass_new_track(lib);
        if (!track) goto done;
        for (size_t i = 0; i < f->n_commands; i++) {
            const AssCommand *k = f->commands + i;
            switch (k->kind) {
            case ASSC_PRIVATE: ass_process_codec_private(track, k->text, (int)k->len); break;
            case ASSC_DATA: ass_process_data(track, k->text, (int)k->len); break;
            case ASSC_STYLES: assr_read_styles(track, k->text, k->len); break;
            case ASSC_CHUNK: ass_process_chunk(track, k->packet, (int)k->packet_len, k->start, k->duration); break;
            case ASSC_FLUSH: ass_flush_events(track); break;
            case ASSC_PRUNE: ass_prune_events(track, k->start); break;
            case ASSC_CHECK: ass_set_check_readorder(track, (int)k->start); break;
            case ASSC_FRAMES: assr_frames(w, r, track, (AssFrames){k->start, k->duration, k->end}); break;
            case ASSC_DUMP: break;
            }
        }
    }
    ok = !assr_boxes_lost;
done:
    if (track) ass_free_track(track);
    if (r) ass_renderer_done(r);
    ass_library_done(lib);
    return ok;
}

/* A directive's argument: the rest of the line after the prefix. */
static char *assr_directive(AssLine l, const char *prefix) {
    size_t n = strlen(prefix);
    return assr_starts(l, prefix) ? strndup(l.start + n, l.len - n) : NULL;
}

static char *assr_join_path(const char *directory, const char *path) {
    size_t a = strlen(directory), b = strlen(path);
    char *joined = malloc(a + b + 2);
    if (!joined) return NULL;
    memcpy(joined, directory, a);
    joined[a] = '/';
    memcpy(joined + a + 1, path, b + 1);
    return joined;
}

static char *assr_read_file(const char *path, long limit, size_t *size) {
    FILE *file = fopen(path, "rb");
    if (!file) return NULL;
    fseek(file, 0, SEEK_END);
    long length = ftell(file);
    rewind(file);
    char *data = length >= 0 && length <= limit ? malloc((size_t)length + 1) : NULL;
    if (data && fread(data, 1, (size_t)length, file) != (size_t)length) {
        free(data);
        data = NULL;
    }
    fclose(file);
    if (data) {
        data[length] = 0;
        *size = (size_t)length;
    }
    return data;
}

/* The render directives of a fixture; 0 when a font cannot be read or a frames
 * directive is malformed or one too many. */
static int assr_render_directives(const char *data, size_t size, const char *fonts, AssRenderCase *c) {
    for (const char *p = data, *end = data + size; p < end;) {
        const char *q = memchr(p, '\n', (size_t)(end - p));
        if (!q) q = end;
        AssLine l = {p, (size_t)(q - p)};
        p = q + 1;
        char *value;
        if ((value = assr_directive(l, ";@attach "))) {
            if (c->n_fonts == ASSR_MAX_FONTS) {
                free(value);
                continue;
            }
            AssFontFile *font = c->fonts + c->n_fonts++;
            font->path = assr_join_path(fonts, value);
            free(value);
            if (!font->path || !(font->data = assr_read_file(font->path, ASSR_MAX_FONT_SIZE, &font->size))) {
                fprintf(stderr, "%s: unreadable font\n", font->path ? font->path : "?");
                return 0;
            }
        } else if ((value = assr_directive(l, ";@default-family "))) {
            free(c->default_family);
            c->default_family = value;
        } else if ((value = assr_directive(l, ";@default-font "))) {
            free(c->default_font);
            c->default_font = assr_join_path(fonts, value);
            free(value);
            if (!c->default_font) return 0;
        } else if (assr_starts(l, ";@frames ")) {
            const char *at = l.start + 9, *stop = l.start + l.len;
            AssFrames *f = c->frames + c->n_frames;
            if (c->n_frames >= ASSR_MAX_FRAMES || !assr_number(&at, stop, &f->start) ||
                !assr_number(&at, stop, &f->step) || !assr_number(&at, stop, &f->end) || f->step <= 0) {
                fprintf(stderr, "malformed or one too many ;@frames directives\n");
                return 0;
            }
            c->n_frames++;
        }
    }
    return 1;
}

static int assr_size(const char *text, int *out) {
    char *end;
    long v = strtol(text, &end, 10);
    if (!*text || *end || v < 1 || v > 16384) return 0;
    *out = (int)v;
    return 1;
}

static int assr_render_main(int argc, char **argv) {
    const char *usage = "usage: reference ass-render-fold FILE MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H\n"
                        "       | ass-render-bench FILE N MODE ... | ass-render-memory FILE MODE ...\n"
                        "       | ass-render-dump FILE MODE ...\n"
                        "  MODE: script | chunks\n";
    const char *action = argv[1];
    int bench = !strcmp(action, "ass-render-bench"), memory = !strcmp(action, "ass-render-memory");
    int dump = !strcmp(action, "ass-render-dump"), fold = !strcmp(action, "ass-render-fold");
    if (!(bench || memory || dump || fold) || argc != 9 + bench) {
        fputs(usage, stderr);
        return 2;
    }
    char *end = NULL;
    unsigned long iterations = bench ? strtoul(argv[3], &end, 10) : 1;
    char **args = argv + 3 + bench;
    int stream = !strcmp(args[0], "chunks");
    AssRenderCase c;
    memset(&c, 0, sizeof c);
    if ((end && *end) || iterations < 1 || iterations > 1000000 || (!stream && strcmp(args[0], "script")) ||
        !assr_size(args[2], &c.frame_w) || !assr_size(args[3], &c.frame_h) || !assr_size(args[4], &c.storage_w) ||
        !assr_size(args[5], &c.storage_h)) {
        fputs(usage, stderr);
        return 2;
    }
    size_t size = 0;
    char *data = assr_read_file(argv[2], ASSR_MAX_INPUT, &size);
    AssFixture fixture;
    memset(&fixture, 0, sizeof fixture);
    int status = 0;
    if (!data || !assr_read_fixture(data, size, stream, &fixture) ||
        !assr_render_directives(data, size, args[1], &c)) {
        fprintf(stderr, "%s: unreadable, too large, or out of memory\n", argv[2]);
        status = 2;
        goto done;
    }
    if (dump) {
        AssImages work = {0, 0, 0, stdout, 0};
        if (!assr_render(&work, &c, data, size, stream, &fixture) || work.unknown) status = 2;
    } else {
        if (memory && write(STDOUT_FILENO, "TRIERGON_BEGIN_V1\n", 18) != 18) status = 2;
        uint64_t start = assr_now_ns(), packets = 0, bytes = 0, digest = 0;
        for (unsigned long i = 0; i < iterations && !status; i++) {
            AssImages work = {0, 0, 0, NULL, 0};
            if (!assr_render(&work, &c, data, size, stream, &fixture) || work.unknown) status = 2;
            packets += work.packets;
            bytes += work.bytes;
            digest = digest * 31 + work.digest;
        }
        uint64_t elapsed = bench ? assr_now_ns() - start : 0;
        if (memory && !status && write(STDOUT_FILENO, "TRIERGON_END_V1\n", 16) != 16) status = 2;
        if (!status)
            printf("{\"schema\":1,\"operation\":\"ass-render\",\"iterations\":%lu,\"packets\":%" PRIu64
                   ",\"bytes\":%" PRIu64 ",\"digest\":%" PRIu64 ",\"elapsed_ns\":%" PRIu64 ",\"eof\":true}\n",
                   iterations, packets, bytes, digest, elapsed);
    }
    fflush(stdout);
    if (status) fprintf(stderr, "%s: libass could not be set up, or an image's kind could not be told\n", argv[2]);
done:
    for (size_t i = 0; i < c.n_fonts; i++) {
        free(c.fonts[i].path);
        free(c.fonts[i].data);
    }
    free(c.default_family);
    free(c.default_font);
    assr_free_fixture(&fixture);
    free(data);
    return status;
}
