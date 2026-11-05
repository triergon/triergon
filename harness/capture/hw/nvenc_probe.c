/*
 * Research probe (not production): ask the NVIDIA driver which codecs its
 * hardware encoder (NVENC) supports on this GPU, via the NVENC API's own
 * GUID enumeration. Uses the pinned nv-codec-headers dynamic loader, so no
 * CUDA toolkit is needed.
 *
 *   cc -O2 -I<nv-codec-headers>/include -o nvenc_probe nvenc_probe.c -ldl
 */
#include <stdio.h>
#include <string.h>
#include <ffnvcodec/dynlink_loader.h>

static const struct { const GUID *guid; const char *name; } known[] = {
    {&NV_ENC_CODEC_H264_GUID, "H.264"},
    {&NV_ENC_CODEC_HEVC_GUID, "HEVC"},
    {&NV_ENC_CODEC_AV1_GUID, "AV1"},
};

int main(void) {
    CudaFunctions *cu = NULL;
    NvencFunctions *nv = NULL;
    if (cuda_load_functions(&cu, NULL) < 0 || nvenc_load_functions(&nv, NULL) < 0) {
        printf("{\"error\":\"driver libraries not loadable\"}\n");
        return 1;
    }
    CUdevice dev;
    CUcontext ctx;
    char name[128] = {0};
    if (cu->cuInit(0) || cu->cuDeviceGet(&dev, 0) || cu->cuDeviceGetName(name, sizeof name, dev) ||
        cu->cuCtxCreate(&ctx, 0, dev)) {
        printf("{\"error\":\"cuda init\"}\n");
        return 1;
    }
    uint32_t max_version = 0;
    nv->NvEncodeAPIGetMaxSupportedVersion(&max_version);
    NV_ENCODE_API_FUNCTION_LIST fl = {.version = NV_ENCODE_API_FUNCTION_LIST_VER};
    if (nv->NvEncodeAPICreateInstance(&fl) != NV_ENC_SUCCESS) {
        printf("{\"error\":\"NvEncodeAPICreateInstance\"}\n");
        return 1;
    }
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open = {
        .version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        .deviceType = NV_ENC_DEVICE_TYPE_CUDA,
        .device = ctx,
        .apiVersion = NVENCAPI_VERSION,
    };
    void *enc = NULL;
    NVENCSTATUS st = fl.nvEncOpenEncodeSessionEx(&open, &enc);
    if (st != NV_ENC_SUCCESS) {
        printf("{\"error\":\"nvEncOpenEncodeSessionEx\",\"status\":%d}\n", st);
        return 1;
    }
    uint32_t count = 0, got = 0;
    fl.nvEncGetEncodeGUIDCount(enc, &count);
    GUID guids[32];
    fl.nvEncGetEncodeGUIDs(enc, guids, count < 32 ? count : 32, &got);
    printf("{\"gpu\":\"%s\",\"nvenc_max_api\":\"%u.%u\",\"header_api\":\"%u.%u\",\"encode_codecs\":[", name, max_version >> 4,
           max_version & 15, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
    for (uint32_t i = 0; i < got; i++) {
        const char *label = "unknown";
        for (size_t k = 0; k < sizeof known / sizeof *known; k++)
            if (!memcmp(&guids[i], known[k].guid, sizeof(GUID))) label = known[k].name;
        printf("%s{\"codec\":\"%s\",\"guid\":\"%08x-%04x-%04x\"}", i ? "," : "", label, guids[i].Data1, guids[i].Data2,
               guids[i].Data3);
    }
    printf("],\"note\":\"NVENC API has no MPEG-1 or MPEG-2 codec GUID at all\"}\n");
    fl.nvEncDestroyEncoder(enc);
    cu->cuCtxDestroy(ctx);
    return 0;
}
