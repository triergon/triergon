/* FIPS 180-4 SHA-256 input proof, outside the measured operation. */
#ifndef TRIERGON_ISOBMFF_INPUT_HASH_H
#define TRIERGON_ISOBMFF_INPUT_HASH_H
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
typedef struct {
    uint32_t h[8];
    uint8_t block[64];
    size_t used;
    uint64_t bytes;
} Sha256;
typedef struct {
    uint64_t bytes;
    uint8_t sha256[32];
} InputProof;
static const uint32_t SHA256_K[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2};
static uint32_t sha_rotr(uint32_t v, unsigned n) {
    return (v >> n) | (v << (32 - n));
}
static void sha_compress(Sha256 *s) {
    uint32_t w[64];
    for (size_t i = 0; i < 16; i++) {
        const uint8_t *p = s->block + 4 * i;
        w[i] = ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) | ((uint32_t)p[2] << 8) | p[3];
    }
    for (size_t i = 16; i < 64; i++) {
        uint32_t a = w[i - 15], b = w[i - 2];
        w[i] = w[i - 16] + (sha_rotr(a, 7) ^ sha_rotr(a, 18) ^ (a >> 3)) + w[i - 7] +
               (sha_rotr(b, 17) ^ sha_rotr(b, 19) ^ (b >> 10));
    }
    uint32_t a = s->h[0], b = s->h[1], c = s->h[2], d = s->h[3], e = s->h[4], f = s->h[5],
             g = s->h[6], h = s->h[7];
    for (size_t i = 0; i < 64; i++) {
        uint32_t t1 = h + (sha_rotr(e, 6) ^ sha_rotr(e, 11) ^ sha_rotr(e, 25)) +
                      ((e & f) ^ (~e & g)) + SHA256_K[i] + w[i];
        uint32_t t2 =
            (sha_rotr(a, 2) ^ sha_rotr(a, 13) ^ sha_rotr(a, 22)) + ((a & b) ^ (a & c) ^ (b & c));
        h = g;
        g = f;
        f = e;
        e = d + t1;
        d = c;
        c = b;
        b = a;
        a = t1 + t2;
    }
    s->h[0] += a;
    s->h[1] += b;
    s->h[2] += c;
    s->h[3] += d;
    s->h[4] += e;
    s->h[5] += f;
    s->h[6] += g;
    s->h[7] += h;
}
static void sha_init(Sha256 *s) {
    static const uint32_t initial[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                                        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    memset(s, 0, sizeof(*s));
    memcpy(s->h, initial, sizeof(initial));
}
static int sha_update(Sha256 *s, const uint8_t *p, size_t n) {
    if ((uint64_t)n > UINT64_MAX / 8 - s->bytes) {
        return -1;
    }
    s->bytes += (uint64_t)n;
    while (n) {
        size_t take = 64 - s->used;
        if (take > n) {
            take = n;
        }
        memcpy(s->block + s->used, p, take);
        s->used += take;
        p += take;
        n -= take;
        if (s->used == 64) {
            sha_compress(s);
            s->used = 0;
        }
    }
    return 0;
}
static void sha_finish(Sha256 *s, uint8_t out[32]) {
    s->block[s->used++] = 0x80;
    if (s->used > 56) {
        memset(s->block + s->used, 0, 64 - s->used);
        sha_compress(s);
        s->used = 0;
    }
    memset(s->block + s->used, 0, 56 - s->used);
    uint64_t bits = s->bytes * 8;
    for (size_t i = 0; i < 8; i++) {
        s->block[63 - i] = (uint8_t)(bits >> (8 * i));
    }
    sha_compress(s);
    for (size_t i = 0; i < 8; i++) {
        for (size_t j = 0; j < 4; j++) {
            out[4 * i + j] = (uint8_t)(s->h[i] >> (24 - 8 * j));
        }
    }
}
static void proof_hex(FILE *out, const InputProof *p) {
    for (size_t i = 0; i < 32; i++) {
        fprintf(out, "%02x", p->sha256[i]);
    }
}
static void proof_report(FILE *out, const InputProof *p) {
    fprintf(out,
            "{\"schema\":\"triergon.input-proof.v1\",\"input_bytes\":%" PRIu64
            ",\"input_sha256\":\"",
            p->bytes);
    proof_hex(out, p);
    fputs("\"}", out);
}
#endif
