# Capture hardware-acceleration evaluation (platform results)

**Separate from qualification.** Nothing here changes M1, the frozen contract
([../contract.json](../contract.json)) or any software gate, and nothing here is
native Linux ARM64 qualification evidence (that is
[the archived v3 collector summary](../STATUS.md#run-v3-archived-report-verdict-superseded),
Graviton4). Production code is unchanged: `crates/capture` stays pure safe Rust
with no dependencies and no hardware API. All hardware code in this evaluation
is research tooling under `harness/capture/hw/`.

Evidence: [capability.json](capability.json) (encoder capability, FFmpeg
limits, upstream observations, pins), [nvidia-t4g-g5g.json](nvidia-t4g-g5g.json)
and [apple-m3pro-macos.json](apple-m3pro-macos.json) (per-case, per-config
results). Raw runs stay outside git under `.research/capture-d5/hw/`.

## Verdict

1. **No MPEG-1 encoder hardware exists on either device, so encoder
   acceleration is not proposed.** VideoToolbox on the M3 Pro refuses to create
   mp1v/mp2v sessions (−12908, no encoder), while an H.264 control session is
   created. NVENC on the T4G enumerates only H.264 and HEVC. The T4G's Vulkan
   Video exposes only H.264/H.265 encode. No other codec was substituted.
2. **The only part left to accelerate is RGBA → YUV 4:2:0. Every
   hardware-enabled FFmpeg path fails the frozen ±1 conversion rule, and none
   is faster than the CPU.**
   - `scale_vulkan` cannot produce 4:2:0 at all; that fails with ENOTSUP on
     both devices. The best it can do is a GPU matrix to 4:4:4 followed by
     CPU swscale subsampling. On the T4G it does not run at all ("No memory
     type found for flags 0x3"). On the M3 Pro it misses the rule (max error
     7–20, 75–81% of samples exact) and loses 1.2–1.6 dB PSNR-Y on the two UI
     cases. It is 2.2–5.8× slower than FFmpeg's own CPU path.
   - `libplacebo` (v7.351.0) does the whole 4:2:0 conversion on the GPU on
     both devices. Luma stays within ±1, but its chroma downsampling differs
     from swscale's by up to 41, so it misses the rule too. Encoder quality
     and bits after encoding are unchanged. It is 1.4–4.8× slower than
     swscale on the T4G and 2.7–12× slower on the M3 Pro. It needs 2.0–2.4 s of
     device start-up on the T4G and holds one frame back.
   - Transfers alone (upload and readback of the RGBA frame, no conversion)
     already take 1.0 ms per 960×540 frame on the T4G and 1.7 ms on the M3 Pro.
     That is more than the pure-Rust conversion of the whole frame (1.0 ms on
     Neoverse-N1, 0.37 ms on the M3 Pro).
3. **Every GPU path uses more memory.**
   - Live heap: 5–63 MB, against 0.9–3.2 MB for Rust.
   - Attributable RSS: 19–273 MB, against 0.9–6.9 MB for Rust.
   - Device memory: up to 47 MiB per process on the T4G. On the M3 Pro the
     physical footprint is about 100–290 MB above the CPU path.
4. **The pure-Rust pipeline stays the right implementation.** It is the only
   candidate that passes conversion, encoder and mux on every case on both
   devices. It has the lowest heap and RSS everywhere. Two platform results
   on speed relative to FFmpeg's CPU path must not be read as qualification:
   - on the T4G host's Neoverse-N1 CPU, Rust is 0.91–1.15× FFmpeg's time;
   - on the non-isolated Mac, the medians are 0.92–1.11×.

### Consumer-boundary option (proposal only, not built)

If a consumer ever needs GPU help, the only shape consistent with these
results and with the pure-safe-Rust constraint is the following:
- The consumer (Streem's renderer) samples its own framebuffer texture and
  writes BT.601 limited-range Y and 2×2-sited U/V planes. A GLES/Vulkan
  fragment or compute shader would do this, implementing exactly the
  contract's [1,3,3,1]/8 vertical and pair-mean horizontal chroma filter.
- The consumer reads back 1.5 bytes per pixel instead of 4.
- The planes go to a YUV entry point of `triergon-capture`.

That would remove CPU conversion and cut readback bandwidth by 62%. It needs:
- a new crate API that accepts planar YUV;
- per-device proof that the shader meets the ±1 rule (FFmpeg's GPU filters
  do not);
- TV-specific (32-bit, GLES) measurement.

None of this is evidence today, and the crate stays free of any hardware API.

## What was compared

Identical inputs, codec features, output format, frame residency and CPU
readback for every configuration:
- The inputs are the self-authored deterministic RGBA cases c04, c02, c09 and
  c07: 960×540 top-down and 480×270 bottom-up, 90 frames each.
- Each configuration produces one MPEG-1 I/P (GOP 30) MPEG-TS session at the
  case bitrate. The FFmpeg configurations use FFmpeg's unchanged software
  `mpeg1video` and `mpegts`, because no hardware encoder exists.
- Frames start in CPU memory and end in CPU memory, because the encoder
  needs CPU planes.

| Config | Build | Conversion |
|---|---|---|
| `rust` | `harness/capture/hw/rust-phases` (composes the qualified `triergon-capture` Converter/Encoder/Muxer exactly as `Session`; output hash identical to `capture-worker`) | pure safe Rust, CPU |
| `ffmpeg-sws` | pinned hardware-enabled FFmpeg 9.0.2 | Streem's swscale path (`SWS_BILINEAR` → NV12 → split); the grading reference |
| `ffmpeg-gpu` | same build | hwupload → `scale_vulkan` 4:4:4 (BT.601 limited) → hwdownload → swscale 4:4:4→4:2:0 |
| `ffmpeg-gpu-placebo` | same + libplacebo v7.351.0 | hwupload → `libplacebo` yuv420p (BT.601 limited, no dithering, RGB0 input) → hwdownload |
| `ffmpeg-gpu-transfer` | same build | hwupload → hwdownload of RGBA only (attribution; † its output is not a conversion and is not graded) |

Hardware builds (`harness/capture/hw/gpu-setup.sh`, `gpu-placebo-setup.sh`,
`mac-hw-setup.sh`):
- They use the FFmpeg 9.0.2 archive (sha256 8c385028…) with the gating
  software component set.
- They add avfilter (`buffer, buffersink, format, hwupload, hwdownload,
  scale, scale_vulkan, vflip_vulkan, flip_vulkan`, plus `scale_vt` on macOS
  and `libplacebo` in the placebo variant) and Vulkan (plus VideoToolbox on
  macOS).
- Headers are pinned to Khronos Vulkan-Headers v1.4.312, SPIRV-Headers
  vulkan-sdk-1.4.313.0, nv-codec-headers n13.0.19.0 and libplacebo v7.351.0;
  [capability.json](capability.json) records every commit and hash.

**Bottom-up input on the GPU paths.**
- hwupload crashes on swscale's negative-stride flip; the first Mac run
  (`eval-mac-v1`) keeps that failure.
- 9.0.2's `vflip_vulkan` writes each row one row off.
- So the GPU workers reverse rows into pooled CPU buffers before upload.
  That copy is timed as part of their conversion.

### Hardware-active verification

Each run selects the device by name (`CAPTURE_VK_DEVICE`), so a software
rasteriser cannot stand in: the T4G host also exposes Mesa llvmpipe.
- **NVIDIA T4G:** every GPU run logged "Device 0 selected: NVIDIA T4G
  (discrete)". nvidia-smi attributed 18–47 MiB of device memory to the worker
  PID, and device utilisation reached 8–24%.
- **M3 Pro:** the log shows "Device 0 selected: Apple M3 Pro (integrated)",
  and swscale reports "Compiled using backend 'spirv'". The Mac's
  IOAccelerator utilisation is system-wide (WindowServer and other
  applications), so it does not attribute GPU use to the worker. The
  physical-footprint growth above does.

### Measurement

- **Rounds and timing:** 10 rounds per case. In each round every
  configuration runs once, as its own process, in seeded random order.
  - *Complete operation* is device start-up (process-level, once) plus the
    session: open → all frames → converter flush → encoder drain → reset
    (close and reopen) → close → device release.
  - Latency is per frame (convert plus encode/mux). Throughput is frames
    over the frames phase.
- **CPU:**
  - *Session* CPU is all threads from open to close.
  - *Process* CPU comes from wait4 minus an input-load-only run of the same
    worker. It includes driver initialisation.
  - ‡ The Rust tooling worker does not report session CPU; its process
    figure has no device work in it.
- **Memory:**
  - Heap is peak live malloc-family bytes between markers. The macOS
    observer is `harness/allocations.c`. The Linux observer is a permissive
    LD_PRELOAD counter (`heapcount.c`), because the qualification observer
    rejects the threads and mappings every GPU driver creates.
  - RSS comes from `rss.py`: kernel `ru_maxrss` minus the load-only peak.
  - Device memory on the T4G is nvidia-smi per-process memory. On the
    M3 Pro it is `phys_footprint`, which includes GPU-mapped memory on
    unified memory.
- **Correctness:** `compare.py` against the `ffmpeg-sws` path of the same
  build.
  - For the Rust candidate its verdicts stand unchanged.
  - For FFmpeg candidates, compare.py's mux and encoder verdicts encode
    Triergon's declared differences D1–D3, which an FFmpeg candidate cannot
    meet. `report.py` therefore grades them on three things: identical
    PAT/PMT/SDT bytes; the same frame count and picture types; PSNR-Y within
    0.25 dB at ≤1.03× bits.
  - The ±1 conversion rule is applied unchanged.

## NVIDIA T4G (AWS g5g.xlarge, Linux ARM64, Graviton2 Neoverse-N1; isolated)

- **Host:** driver 580.178.04, Vulkan 1.4.312, kernel 7.0.0-1014-aws. Nothing
  else ran on the host during timing.
- **CPU caveat:** Neoverse-N1 is not the Graviton4 used for qualification.
- **`scale_vulkan`:** the config fails on this GPU, so it has no rows below.


Correctness (graded against the ffmpeg-sws path of the same pinned build)

| Case | Candidate | Conversion (max abs, exact share) | Encoder / mux | PSNR-Y cand / ref (dB) | Bits ratio |
|---|---|---|---|---|---|
| c04-motion-960 | FFmpeg scale_vulkan | **fails to run** | — | — | — |
| c04-motion-960 | FFmpeg libplacebo | **FAIL** (41, 82.6%) | same frames/quality, tables identical | 42.59 / 42.59 | 0.997 |
| c04-motion-960 | Rust | pass (1, 99.2%) | pass / pass | 42.70 / 42.59 | 0.907 |
| c02-ui-scroll-960 | FFmpeg scale_vulkan | **fails to run** | — | — | — |
| c02-ui-scroll-960 | FFmpeg libplacebo | **FAIL** (26, 90.8%) | same frames/quality, tables identical | 52.21 / 52.22 | 0.996 |
| c02-ui-scroll-960 | Rust | pass (1, 99.4%) | pass / pass | 52.12 / 52.22 | 1.066 |
| c09-motion-480-bottomup | FFmpeg scale_vulkan | **fails to run** | — | — | — |
| c09-motion-480-bottomup | FFmpeg libplacebo | **FAIL** (40, 82.0%) | same frames/quality, tables identical | 43.80 / 43.79 | 0.986 |
| c09-motion-480-bottomup | Rust | pass (1, 99.2%) | pass / pass | 43.90 / 43.79 | 1.031 |
| c07-ui-static-480-bottomup | FFmpeg scale_vulkan | **fails to run** | — | — | — |
| c07-ui-static-480-bottomup | FFmpeg libplacebo | **FAIL** (13, 92.4%) | same frames/quality, tables identical | 51.62 / 51.60 | 1.007 |
| c07-ui-static-480-bottomup | Rust | pass (1, 99.6%) | pass / pass | 52.15 / 51.60 | 1.340 |

Complete operation (session = open → frames → drain → reset → close → device release; median of 10 paired rounds)

| Case | Config | Session ms | × ffmpeg-sws (min–max) | Frames/s | Frame latency med / p95 ms | Convert med ms | CPU ms/frame session / process |
|---|---|---|---|---|---|---|---|
| c04-motion-960 | Rust (pure safe) | 781.1 | 1.148 (1.130–1.160) | 115 | 8.70 / 8.96 | 1.00 | 8.70‡ / 8.70 |
| c04-motion-960 | FFmpeg sws (CPU) | 680.9 | 1.000 (1.000–1.000) | 133 | 7.03 / 14.75 | 2.19 | 7.57 / 7.57 |
| c04-motion-960 | FFmpeg scale_vulkan | fails (ENOTSUP) | — | — | — | — | — |
| c04-motion-960 | FFmpeg libplacebo | 940.0 | 1.381 (1.358–1.418) | 120 | 5.94 / 13.94 | 1.14 | 10.17 / 19.80 |
| c04-motion-960 | transfer only† | 361.8 | 0.530 (0.523–0.546) | 443 | 2.11 / 3.52 | 1.00 | 3.39 / 13.33 |
| c02-ui-scroll-960 | Rust (pure safe) | 397.4 | 0.914 (0.911–0.919) | 227 | 4.22 / 5.38 | 1.00 | 4.44‡ / 4.44 |
| c02-ui-scroll-960 | FFmpeg sws (CPU) | 435.2 | 1.000 (1.000–1.000) | 209 | 4.47 / 6.03 | 2.18 | 4.84 / 4.87 |
| c02-ui-scroll-960 | FFmpeg scale_vulkan | fails (ENOTSUP) | — | — | — | — | — |
| c02-ui-scroll-960 | FFmpeg libplacebo | 691.3 | 1.592 (1.572–1.772) | 178 | 3.41 / 7.57 | 1.12 | 7.32 / 17.49 |
| c02-ui-scroll-960 | transfer only† | 361.4 | 0.829 (0.821–0.844) | 443 | 2.11 / 3.48 | 1.00 | 3.38 / 13.32 |
| c09-motion-480-bottomup | Rust (pure safe) | 239.9 | 1.056 (1.053–1.061) | 376 | 2.68 / 2.74 | 0.29 | 2.67‡ / 2.67 |
| c09-motion-480-bottomup | FFmpeg sws (CPU) | 227.2 | 1.000 (1.000–1.000) | 402 | 2.51 / 2.57 | 0.60 | 2.52 / 2.53 |
| c09-motion-480-bottomup | FFmpeg scale_vulkan | fails (ENOTSUP) | — | — | — | — | — |
| c09-motion-480-bottomup | FFmpeg libplacebo | 571.9 | 2.522 (2.481–2.596) | 234 | 2.46 / 3.90 | 0.54 | 6.27 / 16.29 |
| c09-motion-480-bottomup | transfer only† | 213.2 | 0.938 (0.935–0.946) | 1556 | 0.56 / 1.08 | 0.31 | 2.20 / 12.09 |
| c07-ui-static-480-bottomup | Rust (pure safe) | 96.5 | 1.097 (1.084–1.102) | 936 | 1.02 / 1.36 | 0.29 | 1.07‡ / 1.07 |
| c07-ui-static-480-bottomup | FFmpeg sws (CPU) | 88.1 | 1.000 (1.000–1.000) | 1040 | 0.91 / 1.22 | 0.60 | 0.98 / 1.00 |
| c07-ui-static-480-bottomup | FFmpeg scale_vulkan | fails (ENOTSUP) | — | — | — | — | — |
| c07-ui-static-480-bottomup | FFmpeg libplacebo | 426.7 | 4.827 (4.809–5.152) | 366 | 0.86 / 2.19 | 0.54 | 4.65 / 14.75 |
| c07-ui-static-480-bottomup | transfer only† | 213.9 | 2.433 (2.405–2.465) | 1561 | 0.56 / 1.07 | 0.31 | 2.21 / 12.13 |

Memory (heap: peak live bytes in the measured window, 3 runs; RSS: peak minus the same worker's input-load-only peak, median of 3)

| Case | Config | Live heap MB | Attributable RSS MB | Device / surface memory |
|---|---|---|---|---|
| c04-motion-960 | Rust (pure safe) | 3.18 | 3.1 | 0 |
| c04-motion-960 | FFmpeg sws (CPU) | 6.63 | 8.8 | 0 |
| c04-motion-960 | FFmpeg libplacebo | 63.06 | 272.6 | 47 MiB process (device total 57 MiB) |
| c04-motion-960 | transfer only† | 34.79 | 218.4 | 23 MiB process (device total 33 MiB) |
| c02-ui-scroll-960 | Rust (pure safe) | 3.19 | 3.4 | 0 |
| c02-ui-scroll-960 | FFmpeg sws (CPU) | 6.64 | 8.9 | 0 |
| c02-ui-scroll-960 | FFmpeg libplacebo | 63.06 | 272.6 | 47 MiB process (device total 57 MiB) |
| c02-ui-scroll-960 | transfer only† | 34.79 | 218.4 | 23 MiB process (device total 33 MiB) |
| c09-motion-480-bottomup | Rust (pure safe) | 0.86 | 1.0 | 0 |
| c09-motion-480-bottomup | FFmpeg sws (CPU) | 2.15 | 4.3 | 0 |
| c09-motion-480-bottomup | FFmpeg libplacebo | 58.66 | 256.9 | 29 MiB process (device total 39 MiB) |
| c09-motion-480-bottomup | transfer only† | 29.05 | 208.4 | 18 MiB process (device total 28 MiB) |
| c07-ui-static-480-bottomup | Rust (pure safe) | 0.87 | 0.9 | 0 |
| c07-ui-static-480-bottomup | FFmpeg sws (CPU) | 2.15 | 4.3 | 0 |
| c07-ui-static-480-bottomup | FFmpeg libplacebo | 58.66 | 257.0 | 29 MiB process (device total 39 MiB) |
| c07-ui-static-480-bottomup | transfer only† | 29.05 | 208.6 | 18 MiB process (device total 28 MiB) |

Lifecycle phases (ms, median; c04-motion-960 / c07-ui-static-480-bottomup)

| Config | Device startup | Open | Drain | Reset (close+reopen) | Close | Device shutdown |
|---|---|---|---|---|---|---|
| Rust (pure safe) | 0.00 / 0.00 | 0.36 / 0.29 | 0.00 / 0.00 | 0.70 / 0.06 | 0.16 / 0.00 | 0.00 / 0.00 |
| FFmpeg sws (CPU) | 0.00 / 0.00 | 0.86 / 0.77 | 4.00 / 0.28 | 1.29 / 0.54 | 0.01 / 0.01 | 0.00 / 0.00 |
| FFmpeg libplacebo | 2281.28 / 2000.95 | 12.87 / 12.45 | 4.01 / 0.29 | 25.11 / 23.00 | 5.14 / 4.68 | 141.79 / 141.63 |
| transfer only† | 2383.93 / 2004.54 | 4.60 / 4.52 | 0.98 / 0.21 | 10.11 / 9.09 | 2.28 / 2.27 | 140.39 / 140.30 |



## Apple M3 Pro (macOS 15.6.1, MoltenVK 1.4.2; not isolated)

**Timing on this host was not isolated.** Another session's decoder benchmark
and desktop applications were running: load average 7.4–9.9 during the run.
Treat the timings and ratio ranges as indicative only. Correctness, memory
and capability results do not depend on load. The correctness step was
re-run with a numpy-enabled interpreter after the timed run (recorded in the
per-case files), because the timed run's Python 3.15 lacked numpy.


Correctness (graded against the ffmpeg-sws path of the same pinned build)

| Case | Candidate | Conversion (max abs, exact share) | Encoder / mux | PSNR-Y cand / ref (dB) | Bits ratio |
|---|---|---|---|---|---|
| c04-motion-960 | FFmpeg scale_vulkan | **FAIL** (20, 75.9%) | same frames/quality, tables identical | 42.51 / 42.59 | 1.003 |
| c04-motion-960 | FFmpeg libplacebo | **FAIL** (41, 82.8%) | same frames/quality, tables identical | 42.59 / 42.59 | 0.998 |
| c04-motion-960 | Rust | pass (1, 99.2%) | pass / pass | 42.70 / 42.59 | 0.907 |
| c02-ui-scroll-960 | FFmpeg scale_vulkan | **FAIL** (12, 80.9%) | **quality FAIL**, tables identical | 50.64 / 52.22 | 0.998 |
| c02-ui-scroll-960 | FFmpeg libplacebo | **FAIL** (26, 91.6%) | same frames/quality, tables identical | 52.21 / 52.22 | 0.997 |
| c02-ui-scroll-960 | Rust | pass (1, 99.4%) | pass / pass | 52.12 / 52.22 | 1.066 |
| c09-motion-480-bottomup | FFmpeg scale_vulkan | **FAIL** (19, 75.4%) | same frames/quality, tables identical | 43.72 / 43.79 | 0.990 |
| c09-motion-480-bottomup | FFmpeg libplacebo | **FAIL** (40, 82.3%) | same frames/quality, tables identical | 43.80 / 43.79 | 0.988 |
| c09-motion-480-bottomup | Rust | pass (1, 99.2%) | pass / pass | 43.90 / 43.79 | 1.031 |
| c07-ui-static-480-bottomup | FFmpeg scale_vulkan | **FAIL** (7, 79.8%) | **quality FAIL**, tables identical | 50.38 / 51.60 | 0.974 |
| c07-ui-static-480-bottomup | FFmpeg libplacebo | **FAIL** (13, 94.1%) | same frames/quality, tables identical | 51.62 / 51.60 | 1.006 |
| c07-ui-static-480-bottomup | Rust | pass (1, 99.6%) | pass / pass | 52.15 / 51.60 | 1.340 |

Complete operation (session = open → frames → drain → reset → close → device release; median of 10 paired rounds)

| Case | Config | Session ms | × ffmpeg-sws (min–max) | Frames/s | Frame latency med / p95 ms | Convert med ms | CPU ms/frame session / process |
|---|---|---|---|---|---|---|---|
| c04-motion-960 | Rust (pure safe) | 275.8 | 1.110 (1.043–1.312) | 327 | 3.01 / 3.60 | 0.37 | 2.82‡ / 2.82 |
| c04-motion-960 | FFmpeg sws (CPU) | 243.9 | 1.000 (1.000–1.000) | 372 | 2.45 / 5.58 | 0.66 | 2.68 / 2.58 |
| c04-motion-960 | FFmpeg scale_vulkan | 558.6 | 2.228 (2.037–2.881) | 164 | 6.24 / 9.58 | 4.00 | 4.27 / 4.49 |
| c04-motion-960 | FFmpeg libplacebo | 664.7 | 2.706 (2.483–3.085) | 137 | 5.86 / 9.37 | 3.68 | 6.00 / 6.09 |
| c04-motion-960 | transfer only† | 208.4 | 0.862 (0.794–1.343) | 441 | 2.17 / 5.45 | 1.74 | 1.00 / 1.13 |
| c02-ui-scroll-960 | Rust (pure safe) | 135.6 | 0.922 (0.678–0.993) | 666 | 1.44 / 2.09 | 0.38 | 1.42‡ / 1.42 |
| c02-ui-scroll-960 | FFmpeg sws (CPU) | 147.1 | 1.000 (1.000–1.000) | 617 | 1.49 / 2.28 | 0.71 | 1.61 / 1.61 |
| c02-ui-scroll-960 | FFmpeg scale_vulkan | 383.0 | 2.592 (1.898–3.575) | 238 | 3.83 / 7.08 | 2.87 | 2.87 / 3.05 |
| c02-ui-scroll-960 | FFmpeg libplacebo | 525.0 | 3.489 (2.711–4.268) | 174 | 4.39 / 8.16 | 3.26 | 4.75 / 4.93 |
| c02-ui-scroll-960 | transfer only† | 197.3 | 1.322 (0.962–1.559) | 466 | 2.09 / 5.07 | 1.69 | 0.91 / 1.11 |
| c09-motion-480-bottomup | Rust (pure safe) | 100.8 | 0.938 (0.869–1.201) | 895 | 1.03 / 1.57 | 0.11 | 1.12‡ / 1.12 |
| c09-motion-480-bottomup | FFmpeg sws (CPU) | 107.9 | 1.000 (1.000–1.000) | 854 | 1.11 / 1.67 | 0.19 | 1.14 / 1.15 |
| c09-motion-480-bottomup | FFmpeg scale_vulkan | 342.1 | 3.466 (2.627–3.998) | 268 | 3.24 / 6.89 | 2.36 | 2.44 / 2.67 |
| c09-motion-480-bottomup | FFmpeg libplacebo | 514.9 | 4.699 (4.354–5.502) | 179 | 4.03 / 6.90 | 3.11 | 5.10 / 5.37 |
| c09-motion-480-bottomup | transfer only† | 131.3 | 1.228 (0.900–1.524) | 707 | 0.98 / 3.93 | 0.82 | 0.62 / 0.85 |
| c07-ui-static-480-bottomup | Rust (pure safe) | 36.1 | 1.097 (0.955–1.360) | 2507 | 0.34 / 0.67 | 0.11 | 0.40‡ / 0.40 |
| c07-ui-static-480-bottomup | FFmpeg sws (CPU) | 31.7 | 1.000 (1.000–1.000) | 2912 | 0.29 / 0.64 | 0.18 | 0.34 / 0.34 |
| c07-ui-static-480-bottomup | FFmpeg scale_vulkan | 214.7 | 5.829 (5.408–7.730) | 432 | 1.84 / 4.80 | 1.53 | 1.75 / 2.01 |
| c07-ui-static-480-bottomup | FFmpeg libplacebo | 383.6 | 12.127 (10.760–13.126) | 240 | 2.79 / 5.66 | 2.54 | 4.19 / 4.41 |
| c07-ui-static-480-bottomup | transfer only† | 116.6 | 3.075 (2.501–3.889) | 796 | 0.79 / 2.91 | 0.56 | 0.74 / 0.98 |

Memory (heap: peak live bytes in the measured window, 3 runs; RSS: peak minus the same worker's input-load-only peak, median of 3)

| Case | Config | Live heap MB | Attributable RSS MB | Device / surface memory |
|---|---|---|---|---|
| c04-motion-960 | Rust (pure safe) | 3.23 | 6.9 | n/a (no GPU use) |
| c04-motion-960 | FFmpeg sws (CPU) | 7.23 | 10.2 | phys_footprint peak 196 MB |
| c04-motion-960 | FFmpeg scale_vulkan | 15.40 | 33.3 | phys_footprint peak 302 MB |
| c04-motion-960 | FFmpeg libplacebo | 45.58 | 78.1 | phys_footprint peak 484 MB |
| c04-motion-960 | transfer only† | 11.10 | 25.0 | phys_footprint peak 292 MB |
| c02-ui-scroll-960 | Rust (pure safe) | 3.23 | 6.5 | n/a (no GPU use) |
| c02-ui-scroll-960 | FFmpeg sws (CPU) | 7.23 | 9.4 | phys_footprint peak 196 MB |
| c02-ui-scroll-960 | FFmpeg scale_vulkan | 15.41 | 32.9 | phys_footprint peak 302 MB |
| c02-ui-scroll-960 | FFmpeg libplacebo | 45.59 | 79.1 | phys_footprint peak 484 MB |
| c02-ui-scroll-960 | transfer only† | 11.09 | 25.4 | phys_footprint peak 292 MB |
| c09-motion-480-bottomup | Rust (pure safe) | 0.88 | 1.8 | n/a (no GPU use) |
| c09-motion-480-bottomup | FFmpeg sws (CPU) | 2.64 | 4.9 | phys_footprint peak 52 MB |
| c09-motion-480-bottomup | FFmpeg scale_vulkan | 10.10 | 27.7 | phys_footprint peak 151 MB |
| c09-motion-480-bottomup | FFmpeg libplacebo | 46.04 | 76.2 | phys_footprint peak 330 MB |
| c09-motion-480-bottomup | transfer only† | 5.25 | 19.0 | phys_footprint peak 143 MB |
| c07-ui-static-480-bottomup | Rust (pure safe) | 0.89 | 1.7 | n/a (no GPU use) |
| c07-ui-static-480-bottomup | FFmpeg sws (CPU) | 2.64 | 4.8 | phys_footprint peak 51 MB |
| c07-ui-static-480-bottomup | FFmpeg scale_vulkan | 10.10 | 27.2 | phys_footprint peak 150 MB |
| c07-ui-static-480-bottomup | FFmpeg libplacebo | 46.03 | 75.4 | phys_footprint peak 330 MB |
| c07-ui-static-480-bottomup | transfer only† | 5.24 | 19.4 | phys_footprint peak 143 MB |

Lifecycle phases (ms, median; c04-motion-960 / c07-ui-static-480-bottomup)

| Config | Device startup | Open | Drain | Reset (close+reopen) | Close | Device shutdown |
|---|---|---|---|---|---|---|
| Rust (pure safe) | 0.00 / 0.00 | 0.17 / 0.05 | 0.00 / 0.00 | 0.24 / 0.08 | 0.01 / 0.00 | 0.00 / 0.00 |
| FFmpeg sws (CPU) | 0.00 / 0.00 | 0.44 / 0.37 | 1.46 / 0.10 | 0.24 / 0.18 | 0.01 / 0.01 | 0.00 / 0.00 |
| FFmpeg scale_vulkan | 22.72 / 22.53 | 1.11 / 1.47 | 1.82 / 0.11 | 1.42 / 1.49 | 0.17 / 0.19 | 1.68 / 2.51 |
| FFmpeg libplacebo | 19.22 / 20.79 | 1.69 / 1.68 | 1.53 / 0.12 | 4.76 / 5.07 | 0.26 / 0.26 | 1.16 / 1.18 |
| transfer only† | 21.49 / 24.51 | 1.42 / 1.01 | 0.28 / 0.08 | 0.82 / 0.77 | 0.12 / 0.13 | 1.85 / 1.75 |


## Reproduce

```bash
# Linux GPU host (NVIDIA driver, nvidia_uvm loaded, libegl1 installed)
bash harness/capture/hw/gpu-setup.sh && bash harness/capture/hw/gpu-placebo-setup.sh
python3 harness/capture/hw/evaluate.py --hw-worker $B/bin/hw_capture --placebo-worker $B/bin/hw_capture_placebo \
  --rust-worker $B/bin/capture-rust-phases --sws-reference $B/bin/hw_capture --oracle-bin $B/ffmpeg/oracle/bin \
  --observer $B/bin/heapcount.so --vk-device NVIDIA --inputs <dir> --out <eval> \
  --cases c04-motion-960 c02-ui-scroll-960 c09-motion-480-bottomup c07-ui-static-480-bottomup --rounds 10
python3 harness/capture/hw/rss.py <same arguments>
python3 harness/capture/hw/report.py <eval> "<label>" research/capture/hw/<platform>.json
```

On macOS, run `harness/capture/hw/mac-hw-setup.sh` and pass
`--observer .research/capture-d5/hw/allocations.dylib --vk-device Apple
--extra-env '{"DYLD_FALLBACK_LIBRARY_PATH": "/opt/homebrew/lib"}'`.

Capability probes:
- `harness/capture/hw/vt_encoders.swift` (macOS)
- `harness/capture/hw/nvenc_probe.c` (Linux, nv-codec-headers loader)
- `CAPTURE_VK_DIRECT_FORMAT=yuv420p|nv12` on `hw_capture` (scale_vulkan
  4:2:0)
