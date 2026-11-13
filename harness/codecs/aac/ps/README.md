# Parametric Stereo table recovery

ISO/IEC 14496-3 is not available, and the open publications do not print the `ps_data()`
syntax, the PS Huffman books or most of the signal tables. These scripts recover them
from the pinned FFmpeg 9.0.2 decoder used as a black box: they craft HE-AAC v2 ADTS
streams, decode them with the reference binary and fit the tables from its output and
its messages. No reference source is read.

```sh
cargo xtask codecs aac-ps-tables           # rerun the recovery, write the tables
cargo xtask codecs aac-ps-tables --check   # rerun every step, compare with the committed tables
```

The command builds the `ps_input` example (feature `inspect`, which dumps the QMF slots
this decoder hands to PS), runs `recover_tables.py` and writes
`crates/codec-aac/src/huffman_probed.rs` and `crates/codec-aac/src/sbr/ps/tables.rs`.
A write reuses the steps whose JSON an earlier run left in the work directory
(`target/aac-ps-tables`, or `--work DIR`); `--check` empties the step directories and
reruns every step, then compares the books exactly and each fitted value within the
precision its doc states. Where a fit reports standard errors (the hybrid prototypes,
the decorrelator's linear and ducking constants) or fits a value more than once (the IID
levels and ICC values of every matrix set), the generator stops unless three standard
errors and every fit lie within that precision. A full run takes about 11 minutes; it
needs Python 3 with numpy.

## Method

- **Streams** (`psgen.py`, `pssyntax.py`): a mono SCE with random codebook-1 spectra, SBR
  with kx = 32, and one PS extension per frame with the payload bits under test.
- **Length oracle** (`psprobe.py`): an extension whose size field says one byte makes the
  reference report how many PS bits it read. With the range oracle (the indices it
  refuses) it recovers the syntax and the books (`recover_huff.py`, `recover_ipdopd.py`,
  `recover_sbr_envbal.py`).
- **Signal fits** (`signal/pslib.py`): the reference output of each channel is the 64-band
  QMF synthesis (`psmodel.py`, this decoder's synthesis in float64) of the PS output
  slots. Models linear in their unknowns are fitted exactly in the time domain, from the
  PS input that `ps_input` dumps (with neutral PS the reference output equals its
  synthesis to -134 dB). Fit residuals reach the float32 floor, about 1e-8 rms on 0.03 rms
  signals.

## Signal side (`signal/`)

Each directory holds a model of one part, the recovery that fits it and checks it on
fresh streams, and writes its results as JSON.

| Part | Recovered | Check |
|---|---|---|
| `hyb` | hybrid prototypes for 20 bands (Q = 8: TS 26.405 Table 1 g0, confirmed to 4e-9; Q = 4: differs from Table 1 g1,2 by up to 8e-3) and 34 bands (Q = 12, 8, 4), written to 1e-8; the split-band and QMF-band to parameter-band maps; the configuration rule (34 bands when an enabled mode is 2 or 5); the 10 -> 20 and 20 -> 34 value maps (pairs: index mean truncated toward zero) | one-hot IID and ICC streams, model at -133 dB |
| `dec` | decorrelator: a lead delay of 2 slots with phase exp(-i pi 0.39 f), three all-pass sections (-a + Q z^-d)/(1 - a Q z^-d) of 3, 4, 5 slots, Q = exp(-i pi q f), q = 0.43, 0.75, 0.347, a = 0.6514391, 0.56471813, 0.48954166 (scaled by 1 - 0.05 (k - split) in QMF bands), a 14-slot delay from QMF band 23 and a 1-slot delay from band 35; the decorrelator's band layout (71 and 91 bands) and frequencies; transient ducking per parameter band (peak decay 0.7659283, smoothing 0.25, threshold 1.5) | tones, noise, bursts, chirps: -125 to -132 dB |
| `mix` | the first mixing procedure (Purnhagen 2004 eq. 5 with eq. 8) and ICC values; the second procedure (Breebaart 2005 rotation and scale, ICC floored at 0.05; the output matches mu² and mu rounded to single precision); IPD/OPD as row phases, smoothed as arg(u0 + u1/2 + u2/4), their bands and on/off rules; per-envelope storage; image sub-bands that ramp from the conjugated matrix | every index pair, both procedures; random streams at -129 dB |
| `time` | envelope borders, entry-wise linear ramps, start matrices, the first PS frame (ramps from zero), frames without PS (repeat the last), headers, value storage across band counts, error frames (mono, state kept), 20 <-> 34 changes (start conversion, decorrelator restart) | composed model of all parts on random streams with every frame kind: 1.3e-8 rms |

## Sources by element

The form of each element comes from a publication where one gives it ("paper"), else from
probes alone ("probes"); the probes establish every value. Publications: H. Purnhagen,
"Low complexity parametric stereo coding in MPEG-4", DAFx 2004; J. Breebaart, S. van de
Par, A. Kohlrausch, E. Schuijers, "Parametric coding of stereo audio", EURASIP JASP
2005:9; 3GPP TS 26.405 V19.0.0. Probe ids: d, h, m and t are the probes of the `dec`,
`hyb`, `mix` and `time` parts, which the stages of `signal/*/recover_*.py` name and
repeat; e are probes of the syntax pass, p probes of the decoder's interface and of the
open cases, duck-p those of the transient ducking (below). The probe scripts and their
outputs are in the evidence set
`codec-aac-parametric-stereo-560f2225` (`cargo xtask evidence fetch
codec-aac-parametric-stereo-560f2225`).

| Element | Form | Paper | Probes |
|---|---|---|---|
| Hybrid split 8/4/4 (20 bands) | paper | Purnhagen §3.2 eq. 11, Fig. 4; TS 26.405 §5.2.2, Table 1; Breebaart §7 eq. 35, Table 2 | h01, h02 (maps, Q = 4 prototype; Table 1 g0 confirmed to 4e-9); d15, d16 (QMF-0 sub-bands 0, 1, 6, 7 kept apart) |
| Hybrid split 12/8/4/4/4 (34 bands) | paper | Purnhagen §3.2; Breebaart §7 Table 2 | h03, h04 (maps, prototypes); d26, d27 (all 32 sub-bands apart) |
| Zero-delay alignment, 6 look-ahead slots | paper | TS 26.405 §5.2.2 | e12, p02 |
| Decorrelator lead delay (2 slots) and phase factor | probes | none | d06, d08, d10, d25 |
| Three all-pass links of 3, 4, 5 slots with fractional delays | paper | Purnhagen §4.1 | d06–d08 (form), d08, d25 (values), d16, d26, d27 (band frequencies) |
| Coefficient taper 1 − 0.05 (k − split) | probes | none | d08, d10, d14, d16 |
| Plain-delay borders (all-pass below QMF 23, 14 slots to 34, 1 slot above) | paper | Purnhagen §4.1; Breebaart §7 | e16, d10–d12 |
| Transient ducking | probes: per parameter band K = max(a K′, P), E = E′ + c (P − E′), Q = Q′ + c (K − P − Q′), g = E / (γ Q) when γ Q > E, else 1; a = exp(−4/15), c = 1/4, γ = 3/2 (see Transient ducking below) | Purnhagen §4.2 names transient reduction in the decoder, no form | duck-p01–duck-p13; d20–d25 |
| Mixing, ICC modes 0..2 | paper | Purnhagen §2.2 eq. 4–6 with eq. 8; Breebaart §6 eq. 31 | m02–m04 (eq. 8, not eq. 7; ICC values) |
| Mixing, ICC modes 3..5 | paper | Breebaart §6 eq. 24–30 | m04, m05 (ICC floor 0.05; the output matches μ² and μ rounded to single precision) |
| IPD/OPD as row phases OPD and OPD − IPD | paper | Breebaart §6 eq. 32 | m06–m09, m21, m29; p07, p08, p12 |
| IPD/OPD smoothing arg(u₀ + u₁/2 + u₂/4) | probes | none | m07, m27, m28, m30, m31 |
| Envelope ramps | paper | Purnhagen §3.3; Breebaart §7 | t02, t03, t07, t09, t21, t23; m20, m26 |
| Behaviour over frames (first frame, repeats, error frames, start index, 20 ↔ 34 changes) | probes | none | t01–t33, v_rust |

## Transient ducking

The decoder attenuates the decorrelated signal after a fall in level. For each parameter
band b and QMF slot t, with P the power of the PS input summed over b's decorrelator
bands (hybrid bands below the split, QMF bands above it) and the state zero at the first
PS frame and after a 20 <-> 34 restart:

- K = max(a K', P), a decaying peak;
- E = E' + c (P - E'), the smoothed power;
- Q = Q' + c (K - P - Q'), the smoothed excess of the peak over the power;
- g = E / (γ Q) when γ Q > E, else 1, multiplying the decorrelated output of b's bands
  (all-pass and delay regions alike) in the same slot.

| Constant | Fit | Precision (step that doubles the residual energy) | Value |
|---|---|---|---|
| a | 0.7659282906 | ±4.7e-7 | exp(−4/15) = 0.7659283384 |
| c | 0.2499999672 | ±4e-7 | 1/4 |
| γ | 1.500001033 | ±5e-6 | 3/2 |

The three closed forms together give 1.0032 times the residual energy of the free fit. A
five-constant fit (separate smoothing for E and Q, an exponent on the gain) converges back
to this form. With it the model matches the reference to the float floor on tones, noise,
steps, bursts, clicks, gated and amplitude-modulated signals in both configurations (the
probe kit's streams at 3e-7 to 1e-6 relative). `Synth::ducking` implements it in single precision.

Probes (duck-p): p01 gains per parameter band, real and at most 1; p02 per-slot gains of
one band under tone envelopes; p03 fits of every candidate; p04 two tones in one or in two
parameter bands; p05 further level steps, modulations and bands; p06 the form against every
stream, with variants; p07 the residual on noise; p08 the last frame and deep ducks; p09
the constants' precision and extra freedoms; p10 every stream, 20 and 34 bands; p11 the
floor of deep ducks; p12 the closed forms in single precision; p13 per-slot residuals
around a 40 dB drop.

| Rejected candidate | Ruled out by |
|---|---|
| Instantaneous ratio γ (P / K)^p | p03: best 6.7e-2, p02 burst 0.15; the measured dip deepens over 23 slots, the ratio is deepest at once |
| Attack/release envelope follower against the smoothed power | p03: best 7.5e-3; p02 down4, down100, burst about 1e-2 |
| Fast smoother against slow smoother | p03: best 8.6e-3; p02 down4, down100, burst 1.1e-2 to 1.4e-2 |
| Smoothed power over smoothed peak | p03: best 1.8e-3; p02 burst 3.7e-3 |
| Gain one slot late or early | p03: 4.7e-3 and 4.0e-3 after a refit; kit down-100 1.7e-2 with the constants kept |
| Power of the decorrelator output instead of its input | p06: as far off as no ducking (burst 0.34) |
| Power per decorrelator band or per QMF band | p04: two tones in one parameter band, 7.2e-2; noise streams 0.12 |
| QMF-0 mirror sub-band pairs summed before squaring | p07: noise streams 3e-3 |
| Separate smoothing constants for E and Q, or an exponent on the gain | p09: c_E = c_Q within 1.6e-6 relative, exponent 1 within 5e-6 |
| Square-root gain, or the amplitude sum instead of the power | p09: residual energy 6.5e9 and 1e9 times the fit's |
| Constants rounded to 4 or 5 digits | p09: residual energy 3,500 and 13 times the fit's |

Without ducking the same streams are off by 7e-3 to 6.5. A steady tone in the same
parameter band as a falling one is not ducked; rises and slow decays are not ducked.

Recall: both derivations of this form, the first synthesis pass (d20–d25) and a later
re-derivation from probes alone (duck-p01–duck-p13), remembered FFmpeg's reducer of this
kind with roughly these constants. Neither seeded a fit with them; the re-derivation's
fits started from a = 0.9, c = 0.5, γ = 1 and random starts. The memory may have put the
form on the candidate list. The form rests on the probes: they reject every other
candidate, and this form fits every probe to the float floor.

## Not recovered

- A change between 20 and 34 bands registered while PS is off after an error (by a
  header-less frame read then, or a frame that overran its extension), followed by a
  frame that runs: no tested rule fits. The reference differs from the model in the low
  hybrid bands by 8e-5 to 5e-3 rms in the next frames. The decoder converts the start
  matrices and restarts the decorrelator only when a frame's configuration differs from
  that of the frame read before it.
- IPD/OPD across a change from 34 to 20 bands after IID last ran 34 bands: the steady
  frames are recovered (p07, p08: the 17 coded values map onto 20 bands as the truncated
  means of the `CONV_34_20` rows; p12: bands above the 11 IPD bands keep their stored
  imaginary part), the change frame and the three after it are not (up to 3.9e-2).
- A PS frame without its SBR element: the difference from the reference output lies in QMF
  bands 31..35 of that frame (up to 5.2e-4); an HE-AAC v1 stream with the same frame
  matches.

The crafted streams of these cases are the open cases of
[../ps-open-manifest.json](../ps-open-manifest.json) (files in `open/`).
