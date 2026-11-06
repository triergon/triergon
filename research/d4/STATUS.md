# Decoder comparison material

Inputs, rules and tools used to compare the codec crates (`crates/codec-aac`,
`codec-ac3`, `codec-dca`, `codec-h264`, `codec-hevc`) and the session crate
(`crates/sim`) with FFmpeg 9.0.2. What each decoder supports and refuses is in its crate
README. The comparison harness is `harness/d4` (`reference_decode.c`,
`native_correctness.py`, `time_d4.py`). Gate definitions:
[docs/quality.md](../../docs/quality.md).

| Path | Content |
|---|---|
| `NUMERIC-RULES.json` | Comparison rules: audio per channel RMS ≤ 8.8e-6 and max ≤ 6.1035e-5 with exact rate, layout and counts; video bit-exact; RGBA output bit-exact; stereo s16 output ≤ 1 LSB |
| `CORRECTNESS-MANIFEST.json` | The 454 correctness vectors |
| `WORKLOADS.json`, [WORKLOADS.md](WORKLOADS.md) | The 17 timing and memory workloads and how they are generated |
| [MEASUREMENT-POLICY.md](MEASUREMENT-POLICY.md) | What counts as equivalent work in timing and memory runs |
| [ADAPTER-CONTRACT.md](ADAPTER-CONTRACT.md) | Session-layer contract for a host adapter |
| `DOWNMIX-*.json`, `downmix-*.json` | Observed stereo downmix (`-ac 2`) rules |
| `RGBA-*.json`, `rgba-*.json` | Observed letterbox scaling and RGBA conversion rules |
| `evidence/` | Correctness rows and summaries, timing and memory records, identity hashes |
| `aac/`, `ac3/`, `dca/`, `h264/`, `hevc/` | Per-codec tools and derivation notes |

## Dispositions cited by the correctness summaries

- AC-3/E-AC-3 noise: bap-0 dither, AHT zero-bin fill and SPX blending noise are random.
  A decoder matches FFmpeg's noisy output only by reproducing its noise stream, which
  `codec-ac3` does under `ffmpeg_compat`. `evidence/CORRECTNESS-D1-SUMMARY.json` and
  `evidence/D2/CORRECTNESS-D2-SUMMARY.json` were collected with a build that used its own
  generator; they record 10 AC-3/E-AC-3 vectors as failures for this reason. The
  dither-stripped comparison set is `evidence/CORRECTNESS-MANIFEST-AC3-NODITHER.json`.
- `aac-sce-in-stereo.adts`: FFmpeg leaves FR unwritten in single-SCE frames; see the
  known differences in [crates/codec-aac](../../crates/codec-aac/README.md).
