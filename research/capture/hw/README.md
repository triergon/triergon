# Capture hardware-acceleration evaluation

Research tooling only. This evaluation is not qualification evidence and does
not change the capture contract ([../contract.json](../contract.json)) or the
[`triergon-capture`](../../../crates/capture/README.md) crate, which stays pure
safe Rust with no hardware API. The tooling lives in `harness/capture/hw/`.

Data: [capability.json](capability.json) (encoder capabilities, FFmpeg limits,
pins), [nvidia-t4g-g5g.json](nvidia-t4g-g5g.json) and
[apple-m3pro-macos.json](apple-m3pro-macos.json) (per-case, per-configuration
results).

## Findings

1. **No MPEG-1 encoder hardware on either device.** VideoToolbox (Apple M3 Pro)
   refuses mp1v/mp2v sessions (−12908) while creating an H.264 control session.
   NVENC and Vulkan Video on the NVIDIA T4G expose only H.264 and HEVC.
2. **GPU RGBA → YUV 4:2:0 fails the ±1 conversion rule and is slower.**
   - `scale_vulkan` cannot output 4:2:0 (ENOTSUP); the 4:4:4 + CPU subsampling
     fallback does not run on the T4G and on the M3 Pro misses the rule (max
     error 7–20) at 2.2–5.8× the CPU time.
   - `libplacebo` v7.351.0 converts fully on the GPU on both devices; chroma
     differs by up to 41. It is 1.4–4.8× (T4G) and 2.7–12× (M3 Pro) slower than
     swscale and needs 2.0–2.4 s device start-up on the T4G.
   - Upload plus readback alone of a 960×540 RGBA frame takes 1.0 ms (T4G) and
     1.7 ms (M3 Pro), more than the pure-Rust conversion (1.0 ms on
     Neoverse-N1, 0.37 ms on M3 Pro).
3. **Every GPU path uses more memory:** live heap 5–63 MB and attributable RSS
   19–273 MB, against 0.9–3.2 MB and 0.9–6.9 MB for Rust.
4. **The pure-Rust pipeline passes conversion, encoder and mux on every case on
   both devices** with the lowest heap and RSS. Its time relative to FFmpeg's
   CPU path was 0.91–1.15× on Neoverse-N1 and 0.92–1.11× on the non-isolated
   Mac; these are platform observations, not gate results.

A caller that wants GPU help could convert in its own shader to BT.601
limited-range planes with the contract's chroma filter and hand planar YUV to
the encoder. That would need a planar-input API and per-device proof of the
±1 rule; it is not implemented.

## Method

Inputs: authored cases c04, c02, c09, c07 (960×540 top-down and 480×270
bottom-up, 90 frames). Every configuration encodes one MPEG-1 I/P (GOP 30)
TS session at the case bitrate with frames starting and ending in CPU memory.

| Config | Conversion |
|---|---|
| `rust` | `harness/capture/hw/rust-phases`, composing the crate's converter, encoder and muxer exactly like `Session` |
| `ffmpeg-sws` | swscale `SWS_BILINEAR` → NV12 → planar; the grading reference |
| `ffmpeg-gpu` | hwupload → `scale_vulkan` 4:4:4 → hwdownload → swscale 4:2:0 |
| `ffmpeg-gpu-placebo` | hwupload → `libplacebo` yuv420p → hwdownload |
| `ffmpeg-gpu-transfer` | hwupload → hwdownload only (attribution; not graded) |

The FFmpeg builds use the pinned 9.0.2 archive with Vulkan (and VideoToolbox
on macOS) added; `capability.json` records every header and library pin. GPU
workers reverse bottom-up rows on the CPU before upload because hwupload
rejects negative strides and 9.0.2's `vflip_vulkan` is off by one row; that
copy is timed. Each run selects the device by name (`CAPTURE_VK_DEVICE`) so a
software rasteriser cannot stand in.

Ten rounds per case, each configuration once per round in seeded random
order. Heap is peak live malloc-family bytes; RSS is `ru_maxrss` minus an
input-load-only run; device memory is nvidia-smi per process (T4G) or
`phys_footprint` (M3 Pro). `compare.py` grades against `ffmpeg-sws` from the
same build; FFmpeg candidates are graded on identical PSI bytes, frame count,
picture types and PSNR-Y within 0.25 dB at ≤1.03× bits. The T4G host
(g5g.xlarge, Neoverse-N1) was isolated; the Mac was not, so its timings are
indicative only.

## Reproduce

```bash
bash harness/capture/hw/gpu-setup.sh && bash harness/capture/hw/gpu-placebo-setup.sh
python3 harness/capture/hw/evaluate.py --hw-worker $B/bin/hw_capture --placebo-worker $B/bin/hw_capture_placebo \
  --rust-worker $B/bin/capture-rust-phases --sws-reference $B/bin/hw_capture --oracle-bin $B/ffmpeg/oracle/bin \
  --observer $B/bin/heapcount.so --vk-device NVIDIA --inputs <dir> --out <eval> \
  --cases c04-motion-960 c02-ui-scroll-960 c09-motion-480-bottomup c07-ui-static-480-bottomup --rounds 10
python3 harness/capture/hw/rss.py <same arguments>
python3 harness/capture/hw/report.py <eval> "<label>" research/capture/hw/<platform>.json
```

On macOS run `harness/capture/hw/mac-hw-setup.sh` and pass the macOS
allocation observer, `--vk-device Apple` and
`--extra-env '{"DYLD_FALLBACK_LIBRARY_PATH": "/opt/homebrew/lib"}'`.
Capability probes: `vt_encoders.swift` (macOS), `nvenc_probe.c` (Linux), and
`CAPTURE_VK_DIRECT_FORMAT=yuv420p|nv12` on `hw_capture`.
