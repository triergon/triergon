# capture-worker

Harness worker for the capture pipeline. It runs a
[triergon-capture](../capture/README.md) session with the same command line as the
FFmpeg reference worker `harness/capture/reference_capture.c`, so the harness can
compare both case by case. It is tooling (`publish = false`), not part of the
library.

## Command line

```text
capture-worker convert  <w> <h> <flip> <frames> <in.rgba> <out.yuv>
capture-worker recon    <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.yuv>
capture-worker pipeline <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.ts>
capture-worker time     <w> <h> <flip> <frames> <bitrate> <in.rgba> <iterations>
capture-worker memory   <w> <h> <flip> <frames> <bitrate> <in.rgba>
```

`in.rgba` holds at least `frames` concatenated `w × h × 4` RGBA frames; a nonzero
`flip` reads them bottom-up. Sessions use `SessionConfig::with_rate_rule` at
`bitrate` bit/s (`convert` uses 2.5 Mbit/s).

## Outputs

| Mode | Output |
|---|---|
| `convert` | the converted input of every frame, visible planes packed Y, Cb, Cr |
| `recon` | the encoder reconstruction of every frame, same layout |
| `pipeline` | the TS file, and `{"bytes", "fnv1a64", "vbv_underflow_bits"}` on stdout |
| `time` | one `{"ns", "bytes", "fnv1a64"}` line per iteration; each iteration opens a fresh session |
| `memory` | `SAFE_MEDIA_BEGIN_V1` and `SAFE_MEDIA_END_V1` markers around one session, then `{"bytes", "fnv1a64"}` |

`fnv1a64` is the 64-bit FNV-1a hash of all TS bytes. `time` and `memory` feed every
frame to a hashing sink that keeps nothing, as the reference worker does. Any
failure prints `capture-worker: <reason>` to stderr and exits with status 2.
