# D4 AC-3 / E-AC-3 decoder lane: status

Crate: `crates/codec-ac3` (`triergon-codec-ac3`). It is pure safe Rust (`#![forbid(unsafe_code)]`) with no
dependencies, and it was written from ATSC A/52:2018. The only use of FFmpeg 9.0.2
(`/opt/homebrew/bin/ffmpeg`) was as a black-box oracle. I did not open any FFmpeg source.

State: **working prototype, not qualified.** These are Mac development results only. The coordinator runs
the Linux ARM64 gates.

## Headline

* Every deterministic part of the decoder matches FFmpeg's float decoder to float rounding.
  The worst case is max |diff| 1.5e-7 and RMS ≤ 5e-9, on all 12 FATE-derived streams and all 22
  self-generated streams.
* **The numeric contract (RMS ≤ 8.8e-6, max ≤ 6.1e-5) fails on 10 of 12 FATE vectors as delivered.** The
  whole failure is FFmpeg's pseudo-random noise, which is non-normative. A/52 7.3.4 lets a decoder use
  "any reasonably random sequence" for bap-0 dither. E-AC-3 SPX noise blending (Annex E 3.6.4.2.4) and
  FFmpeg's AHT zero-bin fill are random too.
  * Proof that this is unsatisfiable: FFmpeg checked against FFmpeg, with the only change being its
    other noise seed (`-cons_noisegen 1`), fails the same vectors with the same magnitudes. See column C
    of the table.
  * A decoder can only pass these vectors by reproducing FFmpeg's private PRNG state sequence: the
    libavutil `av_lfg` seeding, the per-bin draw order and the scaling. The brief forbids
    reading/translating FFmpeg source, so that was **not done**. **This needs an owner decision**, either:
    1. authorise an FFmpeg-compatible noise generator, or
    2. amend NUMERIC-RULES so that noise-carrying output is compared with dither stripped (column D),
       plus SPX/AHT noise checked statistically (described below).
* AC-3 bsid 9/10 (reduced sample rate) now decode like FFmpeg. 12 self-authored bsid 9/10 probe
  streams (plus 6 bsid 8 controls) match to float rounding (max 1.0e-5 at peak 89; ≤ 2.3e-7 relative
  to the peak, like the controls), and dither-stripped FATE/synthetic
  streams patched to bsid 9/10 match on every frame that carries no misparsed-dither noise. See
  [AC-3 bsid 9/10](#ac-3-bsid-910-reduced-sample-rate).
* Outside the noise, every remaining difference against FFmpeg was traced to a specific FFmpeg
  convention. Each one is reproduced behind `DecoderOptions::ffmpeg_compat` (default on), and each is
  listed below.

## Vector results (FATE suite, `.research/m1d4/fate-suite`)

Reproduce with `PYTHON=python3.14 research/d4/ac3/run_vectors.sh` (needs numpy). The `.ts` and `.wav`
AC-3 tracks are first extracted with `ffmpeg -c copy -f ac3`. Columns:

* **B**: ours vs FFmpeg, as delivered (the contract).
* **C**: FFmpeg `-cons_noisegen 1` vs FFmpeg default. This is the noise floor of FFmpeg against itself.
* **D**: ours vs FFmpeg on the **dither-stripped** stream: every dithflag is set to 0 with
  `examples/strip_dither.rs`, the CRCs are fixed, and E-AC-3 frames are grown where needed.

Values are the worst channel. Sample and frame counts and layout/plane order match FFmpeg exactly on
every vector.

| vector | layout ffprobe / ours | samples/ch FFmpeg / ours | B ours vs FFmpeg | C FFmpeg vs FFmpeg(cng) | D ours vs FFmpeg, no dither |
|---|---|---|---|---|---|
| csi_miami_5.1_256_spx_small.eac3 | 5.1(side) / 5.1(side) | 72192 / 72192 | FAIL rms 1.4e-03 max 2.6e-02 | FAIL rms 1.4e-03 max 2.7e-02 | FAIL rms 1.4e-03 max 2.5e-02 (SPX noise) |
| csi_miami_stereo_128_spx.eac3 | stereo / stereo | 999936 / 999936 | FAIL rms 5.0e-04 max 1.0e-02 | FAIL rms 5.0e-04 max 1.0e-02 | FAIL rms 4.2e-04 max 9.7e-03 (SPX/AHT noise) |
| csi_miami_stereo_128_spx_small.eac3 | stereo / stereo | 749568 / 749568 | FAIL rms 4.5e-04 max 7.5e-03 | FAIL rms 4.5e-04 max 7.2e-03 | FAIL rms 3.7e-04 max 7.9e-03 (SPX/AHT noise) |
| diatonis_invisible_order_anfos_ac3-small.wav (AC-3 track, 44.1 kHz) | 5.1(side) / 5.1(side) | 64512 / 64512 | pass rms 6.2e-06 max 3.1e-05 | pass rms 6.1e-06 max 3.5e-05 | pass rms 7.3e-11 max 9.3e-10 |
| matrix2_commentary1_stereo_192_small.eac3 | stereo / stereo | 199680 / 199680 | FAIL rms 2.6e-04 max 5.4e-03 | FAIL rms 2.5e-04 max 5.3e-03 | pass rms 3.8e-09 max 8.9e-08 |
| millers_crossing_4.0.ac3 | 4.0 / 4.0 | 95232 / 95232 | FAIL rms 4.9e-04 max 5.8e-03 | FAIL rms 4.8e-04 max 5.0e-03 | pass rms 4.1e-09 max 6.0e-08 |
| monsters_inc_2.0_192_small.ac3 (last frame truncated → concealed) | stereo / stereo | 201216 / 201216 | FAIL rms 2.1e-04 max 2.1e-03 | FAIL rms 2.1e-04 max 2.2e-03 | pass rms 1.7e-09 max 4.5e-08 |
| monsters_inc_5.1_448_small.ac3 (truncated last frame) | 5.1(side) / 5.1(side) | 86016 / 86016 | FAIL rms 8.8e-06 max 6.5e-05 | FAIL rms 8.9e-06 max 5.9e-05 | pass rms 3.2e-12 max 2.9e-11 |
| mp3ac325-4864-small.ts track 0 (truncated last frame) | 5.1(side) / 5.1(side) | 7680 / 7680 | FAIL rms 4.8e-05 max 2.5e-04 | FAIL rms 4.7e-05 max 2.4e-04 | pass rms 4.0e-09 max 3.7e-08 |
| mp3ac325-4864-small.ts track 1 (truncated last frame) | stereo / stereo | 7680 / 7680 | FAIL rms 1.2e-04 max 5.9e-04 | FAIL rms 1.2e-04 max 6.8e-04 | pass rms 4.3e-09 max 6.0e-08 |
| serenity_english_5.1_1536_small.eac3 (1-block frames) | 5.1(side) / 5.1(side) | 24832 / 24832 | pass rms 3.1e-06 max 1.7e-05 | pass rms 3.0e-06 max 1.8e-05 | pass rms 1.8e-12 max 1.5e-11 |
| the_great_wall_7.1.eac3 (AC-3 bsid 6 + E-AC-3 dependent) | 7.1 / 7.1 | 241152 / 241152 | FAIL rms 2.0e-04 max 2.8e-03 | FAIL rms 2.0e-04 max 2.9e-03 | pass rms 4.7e-09 max 7.5e-08 |

Frame counts (ours = FFmpeg):

| vector | frames |
|---|---|
| millers | 62 |
| monsters 2.0 | 131, incl. 1 concealed truncated frame |
| monsters 5.1 | 56 |
| mp3ac325 tracks | 5 each |
| diatonis | 42 |
| csi 5.1 | 47 |
| csi stereo | 651, with a 352-byte leading partial frame skipped, as FFmpeg does |
| csi stereo small | 488 |
| matrix2 | 130 |
| serenity | 97 × 256 samples |
| great wall | 157 |

On the three SPX/AHT streams, column D is still noise-limited, because SPX blending noise cannot be
stripped. Those streams were therefore checked in the MDCT domain instead: FFmpeg's per-block
coefficients were recovered exactly by forward MDCT (TDAC). The scripts are in `analysis/catcmp.py` and
`analysis/noise.py`. Results:

* **Deterministic bins** (every bin whose value involves no noise): match to 1 Q23 LSB, max 1.3e-7.
  That is 393168 + 110036 bins.
* **SPX region**: I predicted FFmpeg's own translated, notch-filtered and blended values from its
  baseband using our SPX parameters. The normalised residual is uniform on exactly [-√3, √3], with mean
  0.0005 and std 1.0011 over 140 400 bins. So translation, wrap, attenuation notch, blend factors and
  coordinates are exact, and the noise is zero-mean with unit variance, as Annex E specifies.
* **AHT hebap-0 bins**: FFmpeg fills each of the 6 DCT coefficients with uniform noise in [-0.5, 0.5),
  ignoring dithflag. We do the same, statistically.

### Self-generated streams (FFmpeg's own ac3/eac3 encoders, lavfi test signal, 3 s)

These cover every acmod with and without LFE, 32/44.1/48 kHz, coupling, and E-AC-3. They are generated
into `target/agent-ac3/syn` with commands of the form
`ffmpeg -f lavfi -i aevalsrc=... -af pan=<layout>|c0=c0.. -c:a ac3|eac3 -b:a <rate>`, then
dither-stripped and compared.

| stream | rate,layout | samples ours ffmpeg | failing ch | worst rms | worst max |
|---|---|---|---|---|---|
| syn_2.1.ac3 | 48000,2.1  | 144384 144384 | 0 | 2.303e-08 | 1.192e-07 |
| syn_3.0.ac3 | 48000,3.0  | 144384 144384 | 0 | 2.324e-08 | 1.192e-07 |
| syn_3.0back.ac3 | 48000,3.0(back)  | 144384 144384 | 0 | 2.324e-08 | 1.192e-07 |
| syn_3.1.ac3 | 48000,3.1  | 144384 144384 | 0 | 2.320e-08 | 1.192e-07 |
| syn_4.0.ac3 | 48000,4.0  | 144384 144384 | 0 | 2.330e-08 | 1.490e-07 |
| syn_4.1.ac3 | 48000,4.1  | 144384 144384 | 0 | 2.319e-08 | 1.490e-07 |
| syn_5.0.ac3 | 48000,5.0(side)  | 144384 144384 | 0 | 2.332e-08 | 1.192e-07 |
| syn_5.1.ac3 | 48000,5.1(side)  | 144384 144384 | 0 | 2.300e-08 | 1.490e-07 |
| syn_51_192k.ac3 | 48000,5.1(side)  | 144384 144384 | 0 | 2.359e-08 | 1.490e-07 |
| syn_51_32000.ac3 | 32000,5.1(side)  | 96768 96768 | 0 | 2.275e-08 | 1.490e-07 |
| syn_51_44100.ac3 | 44100,5.1(side)  | 133632 133632 | 0 | 2.336e-08 | 1.490e-07 |
| syn_mono.ac3 | 48000,mono  | 144384 144384 | 0 | 2.350e-08 | 1.490e-07 |
| syn_quad.ac3 | 48000,quad(side)  | 144384 144384 | 0 | 2.331e-08 | 1.490e-07 |
| syn_stereo.ac3 | 48000,stereo  | 144384 144384 | 0 | 2.298e-08 | 1.192e-07 |
| syn_stereo_64k.ac3 | 48000,stereo  | 144384 144384 | 0 | 2.308e-08 | 1.490e-07 |
| syn_stereo_96k.ac3 | 48000,stereo  | 144384 144384 | 0 | 2.348e-08 | 1.490e-07 |
| syn_eac3_51.eac3 | 48000,5.1(side)  | 144384 144384 | 0 | 2.299e-08 | 1.490e-07 |
| syn_eac3_mono.eac3 | 48000,mono  | 144384 144384 | 0 | 2.319e-08 | 1.490e-07 |
| syn_eac3_quad.eac3 | 48000,quad(side)  | 144384 144384 | 0 | 2.346e-08 | 1.490e-07 |
| syn_eac3_stereo.eac3 | 48000,stereo  | 144384 144384 | 0 | 2.338e-08 | 1.490e-07 |

(The undithered versions of these streams also fail the contract only through dither, the same as the
FATE vectors.)

### AC-3 bsid 9/10 (reduced sample rate)

**Sources.** No normative description exists that I could find:

* ATSC A/52:2018 5.4.2.1 says a decoder shall mute if bsid > 8. Annex E 2.3.1.6 says Annex E
  decoders "are not able to decode bit streams with bsid=9 or 10" and shall mute on them. Table 7.15
  (hth) has only the three fscod columns.
* Public, non-normative descriptions, found by web search (search summaries only; no code opened):
  * the half/quarter-rate meaning comes from RealAudio's DolbyNet ("dnet") AC-3 variant:
    [FFmpeg-devel 2007 "fix ac3 parser with realaudio"](https://lists.ffmpeg.org/pipermail/ffmpeg-devel/2007-February/025463.html),
    [FFmpeg-devel 2022 movenc bsid ≤ 8 limit](https://www.ffmpeg.org/pipermail/ffmpeg-devel/2022-June/297937.html);
  * bsid 9 → 24/22.05/16 kHz and bsid 10 → 12/11.025/8 kHz:
    [FFmpeg trac #6207](https://ffmpeg.org/pipermail/ffmpeg-trac/2024-December/071914.html),
    [blog "bsid 9 and 10 not in Dolby AC3"](https://multimediacomputingguru.blogspot.com/2014/11/bsid-9-and-10-not-in-dolby-ac3.html),
    [GStreamer ac3parse "use bsid 9 and 10 to control sample rate"](https://git.fasttube.de/FaSTTUBe/GST-Tensordecoder-ov_ep/commit/d17d13219c3293e0eb586ef54cb43187060dca92).

  None of these describes the bit allocation. That part was derived **black-box** from FFmpeg 9.0.2.

**FFmpeg 9.0.2 behaviour, derived black-box** (`/opt/homebrew/bin/ffmpeg`, no source read):

* Syntax is the bsid ≤ 8 syntax, with no Annex D xbsi (the downmix follows the plain AC-3 rules;
  check_downmix.py gives 0 mismatching frames on 6 patched multichannel streams).
* The frame size is Table 5.18 in bytes, unchanged. The sample rate is fscod's rate >> (bsid − 8).
  The nominal bit rate is halved/quartered too (ffprobe). There are 1536 samples per frame.
* The bit allocation differs in exactly two places, with s = bsid − 8:
  * sdecay and fdecay are shifted right by s;
  * hth is read as hth[fscod][band >> s] (Table 7.15 band index shifted).

  Each part is necessary. I fitted this per bin on probe streams, recovering FFmpeg's coefficients by
  forward MDCT and comparing bap classes:
  * hth[band] unshifted: 35 of 253 bins wrong in frame 0;
  * hth shift only: 26 wrong;
  * decay shift only: 10 wrong;
  * both: 0 wrong, on all frames.
* bsid 0–8 decode bit-identically to each other, and so do E-AC-3 bsid 11–16 (FFmpeg
  output md5s are identical on dither-stripped millers and matrix2). bsid 17–31 are refused
  ("invalid bitstream id", no output). We return `Unsupported(BitstreamId)`.

**Implementation.** `header.rs` accepts bsid 9/10 as `Syntax::Ac3` with the divided rate.
`access_unit_len` therefore splits these streams: one syncframe is one AU, and the core plus
dependents are grouped the same way FFmpeg packetises them. `Substream::rate_shift` feeds
`bitalloc::stage_mask`. The `Feature::ReducedRateAc3` refusal is gone.

**Results.** Reproduce with `PYTHON=python3.14 research/d4/ac3/run_reduced_rate.sh`.

1. Probe streams from `gen_reduced_rate.py`. They are self-authored, bit by bit, from the 5.4 syntax: random
   D15 exponents, bit-allocation parameters, SNR offsets, coupling (range, band structure, coordinates,
   leaks) and rematrixing; dithflag = 0; all mantissa bits zero. Each bin therefore decodes to the code-0 level
   of its bap, so the output exposes the allocation. The bsid 8 rows are the control.

   | probe (200 frames each) | rates | samples/ch ours = FFmpeg | worst rms | worst max |
   |---|---|---|---|---|
   | 1/0+LFE, fscod 0/1/2, bsid 8 | 48000/44100/32000 | 307200 | 1.8e-07 | 3.8e-06 |
   | 1/0+LFE, fscod 0/1/2, bsid 9 | 24000/22050/16000 | 307200 | 1.8e-07 | 3.8e-06 |
   | 1/0+LFE, fscod 0/1/2, bsid 10 | 12000/11025/8000 | 307200 | 1.8e-07 | 5.7e-06 |
   | 2/0+LFE cpl+remat, fscod 0/1/2, bsid 8 | 48000/44100/32000 | 307200 | 1.7e-07 | 5.7e-06 |
   | 2/0+LFE cpl+remat, fscod 0/1/2, bsid 9 | 24000/22050/16000 | 307200 | 2.5e-07 | 1.0e-05 |
   | 2/0+LFE cpl+remat, fscod 0/1/2, bsid 10 | 12000/11025/8000 | 307200 | 1.8e-07 | 5.7e-06 |

   Amplitudes reach 18–89, so the absolute max is float rounding: ≤ 2.3e-7 relative to the peak,
   the same as the bsid 8 controls. These all pass the contract's absolute limits.
2. Real content patched to bsid 9/10. The 6 FATE AC-3 streams and 16 FFmpeg-encoded syn_*.ac3 streams are
   dither-stripped and then patched with `patch_bsid.py`, which fixes crc1/crc2. Those bits were allocated for bsid 8, so under the
   bsid 9/10 rule both decoders misparse and conceal, frame by frame in the same places:
   * millers 4.0: all 62 frames concealed, max 6e-8;
   * monsters 5.1: all 56 frames, max 4e-11;
   * diatonis 44.1 kHz: 0 concealed, max 9e-10;
   * mp3ac325 5.1: max 6e-8;
   * all 5.1/5.0/4.x/3.x syn streams: max ≤ 8.3e-7 on noise-free frames.

   Totals: 3910 frames, 356 mismatching. 331 are explained by noise, because our noise-on and noise-off outputs differ there:
   misparsed dithflags switch dither on. The other **25 are unexplained, all in mono/stereo/2.1 garbage**
   (monsters 2.0 bsid 9 frames 128–130, syn_mono bsid 10, syn_stereo*, syn_2.1). This is a pre-existing
   error-path divergence, not a bsid 9/10 one: bit-flipped *bsid 8* syn_2.1 shows the same kind of
   unexplained frames (3 of 94; scratch bit-flip check, 2 random flips per frame). It is listed under known gaps.

**E-AC-3 corner cases checked black-box:**

| case | FFmpeg 9.0.2 | ours (`ffmpeg_compat`) |
|---|---|---|
| E-AC-3 independent with fscod = 3 (fscod2 24/22.05/16 kHz), 4 syn_eac3 streams patched | "Reduced sampling rate is not implemented", every packet fails, no output | `Unsupported(Eac3ReducedSampleRate)` per AU, no output. Decoded only with `ffmpeg_compat = false`. A/52 defines no hth column for it, so that path is unverified. |
| bsid 6 core (48 kHz) + dependent at 44.1 kHz / 3 blocks / fscod2 (great wall patched) | "Ignoring non-compatible dependent frame". Outputs the 5.1 core, bit-identical to the core-only stream, labelled 48000 | same: dependent dropped; 5.1, 48000; max 7.5e-8 |
| bsid 9/10 core + 48 kHz / 44.1 kHz / 3-block dependent | dependent dropped; 5.1 core decoded at the reduced-rate allocation, **but frames are labelled with the dependent's rate** (48000/44100) | same, including the label; output max 8.9e-8 on noise-free frames |
| bsid 10 core (12 kHz) + fscod2 24 kHz dependent | dropped (rate differs), labelled 24000 | same |
| bsid 9 core (24 kHz) + fscod2 24 kHz dependent (compatible rate) | whole packet refused (reduced rate not implemented), no output | `Unsupported(Eac3ReducedSampleRate)` |

### Bit-patched syntax that no vector exercises

These use `insert_bits.py` / `patch_bits.py`, with positions from our own parser (`examples/trace.rs`):
the bits are inserted, padding is dropped, and the CRC is fixed. Results on matrix2, frames 10–60:

| syntax | result |
|---|---|
| Phase flags (phsflginu=1 + alternating phsflg) | max 8.9e-8 ✓ |
| Delta bit allocation, 4 variants: coupling + fbw segments, ±dB, a segment past band 49, mantissa misparse/overrun | max 8.9e-8 ✓ |
| Delta bit allocation band ≥ 50 | rejected and concealed in exactly the same frames as FFmpeg ("error in bit allocation") |
| Transient pre-noise processing data (transproce=1, chintransproc=1 with location/length) | FFmpeg output is bit-identical with and without it, so FFmpeg parses but does not apply TPNP. Ours does the same: max 8.9e-8 ✓ |
| Enhanced coupling (ecplinu=1) | FFmpeg logs "Enhanced coupling is not implemented" and conceals the whole frame by repeating the previous last block. Ours does the same (`Unsupported(EnhancedCoupling)` in `FrameInfo::concealed`) |

## Feature coverage (A/52:2018)

**Implemented and verified against the oracle:**

* Syncinfo/bsi for bsid 0–8, including the Annex D alternate syntax (bsid 6 xbsi).
* AC-3 bsid 9/10 (half/quarter sample rate, outside A/52:2018), using FFmpeg's black-box rule:
  rate >> s, decays >> s, hth[band >> s]. See the bsid 9/10 section.
* All acmod 1/0–3/2, with and without LFE.
* 32/44.1/48 kHz.
* Frame size table 5.18.
* CRC1/CRC2 (default: report only, which is FFmpeg's behaviour; `CrcPolicy::Reject` returns a typed
  error).
* Exponents D15/D25/D45 with reuse.
* Parametric bit allocation, including delta bit allocation and the all-zero SNR special case.
* All mantissa quantizers, with grouping.
* Coupling (band structure, coordinates, phase flags).
* Rematrixing for all band counts.
* Block switching (256/512 transforms; the window is KBD α=5, regenerated and checked against Table 7.33).
* Dynamic range: dynrng per block applied to coefficients at scale 1.0 (dynrng2 is wired for 1+1). This
  matches FFmpeg, so it is FFmpeg's default.
* Dialnorm and compr are not applied (also matches FFmpeg).
* E-AC-3: bsi/audfrm, independent substream 0, 1- and 6-block frames, frame exponent strategy
  (Table E2.10), SNR offset strategies, AHT (VQ Tables E4.1–E4.7, GAQ Tables E3.3–E3.6, 6-point IDCT),
  SPX (bands, copy/wrap, attenuation notch, blending, coordinates), dependent substreams with
  chanmap → 7.1 (plane order equals FFmpeg's), and AC-3 + E-AC-3 dependent in one access unit.

**Implemented, no oracle coverage:**

* Dual mono 1+1 (with dynrng2). FFmpeg's encoder cannot produce it.
* E-AC-3 numblkscod 1/2 (2/3-block frames).
* E-AC-3 reduced sample rates (fscod2), only with `ffmpeg_compat = false`. The hearing threshold is
  taken from the fscod2 column, which is a guess: A/52 has no such column. FFmpeg refuses these streams.
* E-AC-3 blkswe=1.
* AHT on the coupling and LFE channels.
* Dependents without chanmap.
* Converter fields.
* blkstrtinfo skip.
* `mixdef == 3`: the length is read as covering the mixdata field from mixdeflen onward (Annex E
  2.3.1.22 / 3.10.4). The syntax table is ambiguous here.

**Not implemented (typed errors):**

* Enhanced coupling (`Feature::EnhancedCoupling`). FFmpeg refuses it too.
* E-AC-3 reduced sample rates under `ffmpeg_compat` (`Feature::Eac3ReducedSampleRate`). FFmpeg
  refuses them too.
* bsid > 16.
* Reserved fscod2 and strmtyp 3.

**Not applied:**

* TPNP (it is parsed). This matches FFmpeg.

**Other independent programs** (substream id ≠ 0) are skipped; program 0 is decoded.

## FFmpeg 9.0.2 conventions reproduced (`ffmpeg_compat = true`)

I found each of these black-box, by patching bits and fitting the oracle's output. They are owner-review
items, because several look like FFmpeg defects rather than A/52 behaviour.

1. **First mixed-transform block.** On the first audio block of the stream in which the fbw channels
   use different transform lengths, FFmpeg rewrites the delay lines before the IMDCT, as if up-mixing a
   down-mixed delay. For 2/0 the right channel is replaced by the left; for 3/x, R ← C and C, S, Ls, Rs
   ← 0. It happens once per decoder, probably from an initial "downmixed" state. Without this, millers
   frame 38 and monsters frame 122 differ by up to 5e-2.
2. **hebap 4 VQ (Table E4.4).** FFmpeg decodes index i with row i+1, and index 31 as an all-zero
   vector. I checked this over 748 bins of csi_miami: FFmpeg's index is consistently +1. The PDF table
   (verified against the page image) has 32 rows starting at 0x5903. This is likely an FFmpeg table
   defect.
3. **Q23 fixed-point coefficients.** Mantissas are Q23 integers and `coefficient = mantissa >> exponent`
   is an arithmetic shift (floor), exactly as 7.3.2/7.3.3 write it. Coupling products are floored, and
   phase-flag negation is applied after flooring. This took deterministic error from ~2e-5 to 1e-7.
4. **Coupling delta bit allocation offsets** count from the coupling channel's first band. This follows
   the 5.4.3.51 wording; the 7.2.2.6 pseudo code starts at 0. Segments reaching band ≥ 50 are an error,
   and the frame is concealed.
5. **Error behaviour.**
   * A truncated AC-3 frame outputs 6 copies of the last decoded block (A/52 7.10 "block repeat").
   * A truncated E-AC-3 frame produces no output.
   * An error inside a block repeats the previous block for the rest of the frame.
   * Reading past the frame end is not an error: zero bits are read. With `ffmpeg_compat = false` this
     becomes an error.
   * Invalid ungrouped quantizer codes (bap 3 code 7, bap 5 code 15) decode as 0. Invalid grouped codes
     use the level formula; this part is unverified.
6. **AHT hebap-0 noise** ignores dithflag. LFE is never dithered (FFmpeg's LFE output is identical
   across noise seeds).
7. **CRC failures** are decoded and reported, not rejected (FFmpeg does not check CRCs by default).
8. **Non-compatible dependents.** A dependent substream whose sample rate or block count differs from
   the independent one is dropped, and the core is output alone. Behind an AC-3 bsid 9/10 core, the AU is
   then labelled with the dropped dependent's rate. This looks like an FFmpeg defect: the samples are
   the reduced-rate core decode. With `ffmpeg_compat = false` such an AU is `InvalidData`.
9. **E-AC-3 fscod2** (reduced sample rate) in any decoded substream refuses the AU
   (`Unsupported(Eac3ReducedSampleRate)`), as FFmpeg does.

## Downmix metadata and FFmpeg's `-ac 2` device path (coordinator request)

`FrameInfo::downmix: Option<DownmixInfo>` mirrors FFmpeg's `AV_FRAME_DATA_DOWNMIX_INFO`. The linear
gains are center/surround Lo/Ro and Lt/Rt mix levels, the LFE mix level and the preferred downmix type.
Each field is an `Option`, all `Some` when exported. The raw codes are also available in
`FrameInfo::downmix_codes`. FFmpeg's rules, derived black-box:

* Exported only when the program has more than two channels (acmod > 2).
* Values come from the **last syncframe of the access unit**. For the 7.1 vector this is the E-AC-3
  dependent, which has no mixmdate.
* **AC-3:**
  * Center: cmixlev → 0.707107 / 0.594604 / 0.5 (reserved and absent → 0.594604).
  * Surround: surmixlev → 0.707107 / 0.5 / 0 (reserved and absent → 0.5).
  * Lt/Rt: fixed 0.707107. The Annex D xbsi1 levels are ignored, even for bsid 6.
  * LFE: 0. Preferred: unknown.
* **E-AC-3:**
  * Levels come from mixmdate (Annex D 3-bit tables; -4.5 dB is 2^-0.75 = 0.594604, not 0.595662).
  * Absent fields: Lo/Ro center 0.594604, Lo/Ro surround 0.5, Lt/Rt center and surround 1.414214
    (code 0).
  * LFE: 10^((10 − lfemixlevcod)/20), or 0 when absent.
  * Preferred type: dmixmod 0/1/2/3 → unknown / LtRt / LoRo / DplII.
* **Verified per frame** (`python3 -I research/d4/ac3/check_downmix.py <decode> <files>` against
  `ffprobe -show_frames`): 0 mismatching frames on all 34 streams (12 FATE + 22 synthetic). This covers
  5.1, 4.0, 3/0, 2/1, 3/1, 2/2, 7.1, AC-3 bsid 6, E-AC-3 with and without mixmdate, LtRt, DplII, and
  LFE +7 dB / 0 dB. Examples:

  | vector | preferred | center | center Lt/Rt | surround | surround Lt/Rt | LFE |
  |---|---|---|---|---|---|---|
  | monsters_inc_5.1 | Unknown | 0.707107 | 0.707107 | 0.5 | 0.707107 | 0 |
  | the_great_wall_7.1 | Unknown | 0.594604 | 1.414214 | 0.5 | 1.414214 | 0 |
  | csi_miami_5.1 | LtRt | 0.707107 | 0.707107 | 0.707107 | 0.707107 | 2.238721 |
  | millers_crossing_4.0 | (all) | 0.707107 | 0.707107 | 0.707107 | 0.707107 | 0 |

* **Observed `-ac 2` behaviour** (informational). The decoder outputs all channels. ffmpeg's
  `auto_aresample` (swresample) then rematrixes using these levels; it logs, for example,
  "Mix levels: center 0.594604 - surround 0.500000 - lfe 0.000000". I reproduced it numerically from
  FFmpeg's own multichannel float output:
  * **Lo/Ro or unknown:**
    * L = FL + c·FC + s·SL + s·BL + (s/√2)·BC + (lfe/√2)·LFE
    * R is symmetric.
  * **LtRt:**
    * L = FL + c_ltrt·FC + (lfe/√2)·LFE − (s_ltrt/√2)·ΣS
    * R = FR + c_ltrt·FC + (lfe/√2)·LFE + (s_ltrt/√2)·ΣS
  * **Output formats:**
    * **f32:** the matrix is **not normalised**. Max error 1.6e-7 on great wall, csi, monsters and
      millers.
    * **s16 (the pulse/alsa device path):** the same matrix **divided by its row sum**: 2.5946 (7.1),
      2.2071 (5.1 at −3/−6 dB), 4.2901 (csi LtRt with LFE). It is then rounded, and matches within
      **1 LSB** on all checked vectors.
  * **DplII** (serenity) is close to the LtRt formula but not exact (1.3e-5). swresample's Pro Logic II
    weights are not derived here.

## API (adapted from the brief)

* `Decoder::decode(&[u8], &mut AudioFrame) -> Result<FrameInfo, DecodeError>`. `decode` appends one
  access unit to the planes. An access unit is one AC-3 syncframe, or an independent substream 0 plus
  its dependents; `access_unit_len` splits a stream the way FFmpeg's parser does.
* `reset()` drops the delay lines, the concealment history and the quirk state.
* Planes are ordered by FFmpeg speaker bit (`Speaker`, `ChannelLayout::ffmpeg_name`).
* `DecodeError` variants: NoSync, Truncated, Empty, InvalidData(&str), CrcMismatch, Unsupported(Feature),
  OutputLayoutMismatch, MissingIndependentSubstream.
* There is no steady-state allocation. The output `Vec`s are reused after the first frame, and a
  dependent substream's state is allocated once on first use.
* Memory: about 80 KB per substream (37 KB state + 43 KB AHT buffer) plus about 30 KB of decoder tables
  and buffers.

## Examples and tools

| tool | purpose |
|---|---|
| `examples/decode.rs` | elementary stream → interleaved f32le in plane order, plus one JSON line on stderr (rate, channels, layout, speakers, frames, samples, concealed/CRC counts, features, downmix) |
| `examples/strip_dither.rs`, `examples/trace.rs` | test tooling |
| `examples/bench.rs` | development timing |
| `research/d4/ac3/gen_tables.py` | regenerates `src/spec_tables.rs` from the spec text (`pdftotext -layout`) |
| `run_vectors.sh`, `cmp_f32.py` | vector table |
| `patch_bits.py`, `insert_bits.py` | bitstream experiments |
| `patch_bsid.py` | rewrite bsid (and fscod) of every AC-3 / E-AC-3 syncframe, fixing crc1/crc2 |
| `gen_reduced_rate.py` | self-authored bsid 8/9/10 probe streams (zero mantissas, random allocation parameters) |
| `run_reduced_rate.sh` | bsid 9/10 tables (outputs under `target/agent-ac3b/rrv`) |
| `check_downmix.py` | downmix check |
| `analysis/*.py` | MDCT-domain, noise and delay-map analysis; uses the `debug-dump` feature build in `target/agent-ac3/dbg` |

Unit and integration tests: `cargo test -p triergon-codec-ac3`, 30 tests. They cover:

* tables, CRC, IMDCT against the forward transform definition, and short-block TDAC;
* synthetic frames for CRC policy, truncation, invalid data, typed header errors, and E-AC-3
  substream errors;
* `tests/reduced_rate.rs`: bsid 9/10 headers and framing, two probe frames (bsid 9 1/0+LFE; bsid 10
  2/0+LFE with coupling) against FFmpeg-captured samples (≤ 1e-5), sensitivity to the rule
  (decoded as bsid 8 they differ by > 1e-3), the non-compatible dependent and E-AC-3 fscod2 refusal;
* FATE frame counts, layouts and coarse values (these skip when the media is absent);
* a corruption no-panic test, run in debug at 3 corruption rates.

## Development timing (Mac M-series, NOT evidence; machine load average 7–12 from other lanes)

| input | ours: decode only, in-process, best of 7 | ffmpeg -threads 1 -f null, wall incl. startup/demux |
|---|---|---|
| csi_miami_stereo_128_spx ×60 | 440 ms | 0.64–0.66 s |
| millers_crossing_4.0 ×60 | 89 ms | 0.13–0.14 s |
| monsters_inc_5.1 ×60 | 102 ms | 0.13–0.14 s |
| the_great_wall_7.1 ×60 | 558 ms | 0.61–0.74 s |

Peak RSS for one full vector: ours 1.7–2.4 MB (including the file buffer), ffmpeg 11.4–11.8 MB. The
profile is roughly mantissa unpacking 30%, IMDCT 25% and bit allocation 15%; allocation is staged, so
blocks that only change SNR offsets skip PSD and masking. Not done: hand-tuned radix-4/NEON FFT.

## Known gaps / requests

* Owner decision on noise. This is the only blocker for the numeric contract on dithered and SPX
  content; see the headline.
* Owner review of the `ffmpeg_compat` items above. Items 1 and 2 are probably FFmpeg defects that we
  reproduce on purpose.
* No 1+1, E-AC-3 2/3-block, blkswe or AHT-CPL/LFE vectors exist. Synthesising them would
  need a real AC-3 encoder. No *encoded* bsid 9/10 (RealAudio dnet) sample is available locally. The
  bsid 9/10 rule rests on self-authored probe streams plus patched content. It holds bin-exact,
  but a real dnet stream should still be run once one is obtained with reviewed rights.
* Garbage-parse divergence: 25 of 3910 frames of bsid-patched (misparsed) mono/stereo/2.1 streams,
  and likewise bit-flipped bsid 8 streams, differ from FFmpeg without noise involvement. This is an
  error-concealment path on invalid data, not bsid-specific. Not investigated yet. The likely candidates
  are the unverified invalid grouped-mantissa handling (convention 5) and concealment after short blocks.
* Mid-stream bsid changes (8 ↔ 9) are handled like an fscod change: delay lines reset on the
  configuration key. This is not compared against FFmpeg; its CLI would reconfigure or resample.
* No shared-code changes needed. The crate adds only an optional `debug-dump` feature and no
  dependencies.
