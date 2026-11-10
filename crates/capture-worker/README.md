# capture-worker

Harness worker for the capture pipeline. It runs a
[triergon-capture](../capture/README.md) session with the same command line as the
FFmpeg reference worker (`harness/reference.c`, capture build), so the harness can
compare both case by case. It is tooling (`publish = false`), not part of the
library.

## Command line

```text
capture-worker convert  <w> <h> <flip> <frames> <in.rgba> <out.yuv>
capture-worker recon    <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.yuv>
capture-worker pipeline <w> <h> <flip> <frames> <bitrate> <in.rgba> <out.ts>
capture-worker memory   <w> <h> <flip> <frames> <bitrate> <in.rgba>
capture-worker bench    <in.rgba> <iterations> <w> <h> <flip> <frames> <bitrate>
```

`bench` takes the harness's timing command line, input and iteration count first.

`in.rgba` holds at least `frames` concatenated `w × h × 4` RGBA frames; a nonzero
`flip` reads them bottom-up. Sessions use `SessionConfig::with_rate_rule` at
`bitrate` bit/s (`convert` uses 2.5 Mbit/s).

## Outputs

| Mode | Output |
|---|---|
| `convert` | the converted input of every frame, visible planes packed Y, Cb, Cr |
| `recon` | the encoder reconstruction of every frame, same layout |
| `pipeline` | the TS file, and `{"bytes", "fnv1a64", "vbv_underflow_bits"}` on stdout |
| `memory` | `TRIERGON_BEGIN_V1` and `TRIERGON_END_V1` markers around one session, then `{"bytes", "fnv1a64"}` |
| `bench` | `{"iterations", "elapsed_ns", "packets", "bytes", "digest"}`: the time of `iterations` sessions, each opened afresh, the frame and byte totals over them, and the hash of their stream; it fails if any session's stream differs |

`fnv1a64` is the 64-bit FNV-1a hash of all TS bytes, as 16 hex digits; `bench` prints
it as the integer `digest`. `bench` and `memory` feed every
frame to a hashing sink that keeps nothing, as the reference worker does. Any
failure prints `capture-worker: <reason>` to stderr and exits with status 2.
