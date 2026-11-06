# AC-3 / E-AC-3 derivation notes

Notes on the FFmpeg-compatible behaviour of
[crates/codec-ac3](../../../crates/codec-ac3/README.md), and the tools in this
directory. The decoder is written from ATSC A/52:2018, with FFmpeg 9.0.2 as the
compatibility reference.

## Black-box derivations

- Bit allocation: delta bit-allocation offsets count from the coupling channel's first
  band (5.4.3.51 wording; the 7.2.2.6 pseudo code starts at 0). Segments reaching band 50
  or beyond conceal the frame, as in FFmpeg.
- bsid 9/10: syntax as bsid ≤ 8 without Annex D xbsi; rate is fscod's rate `>> (bsid − 8)`;
  frame size from Table 5.18. Fitted bin for bin on self-authored probe streams
  (`gen_reduced_rate.py`, zero mantissas, random allocation parameters) by recovering
  FFmpeg's coefficients with a forward MDCT: only shifting both sdecay/fdecay and the
  hearing-threshold band index gives zero bap mismatches.
- First mixed-transform block: FFmpeg rewrites the delay lines once per decoder (2/0:
  R ← L; 3/x: R ← C, C, S, Ls, Rs ← 0).
- hebap 4 VQ: FFmpeg's index is consistently one row ahead of the printed Table E4.4
  (checked over 748 bins); index 31 decodes as zero.
- E-AC-3 SPX noise checked in the MDCT domain: the normalised residual is uniform on
  [−√3, √3] with unit variance.
- Downmix metadata export rules match `ffprobe -show_frames` on every frame of the FATE
  and synthetic streams (`check_downmix.py`).
- Transient pre-noise processing is parsed and not applied: FFmpeg output is identical
  with and without it.

## Tools

| Tool | Purpose |
|---|---|
| `run_vectors.sh`, `cmp_f32.py` | FATE vector comparison (as delivered and dither-stripped) |
| `gen_reduced_rate.py` | bsid 9/10 probe streams (`crates/codec-ac3/tests/reduced_rate.rs`) |
| `gen_tables.py` | Regenerates `src/spec_tables.rs` from the A/52 text |
| `check_downmix.py` | Downmix metadata check |
