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
the decorrelator's linear constants) or fits a value more than once (the IID levels and
ICC values of every matrix set), the generator stops unless three standard errors and
every fit lie within that precision. A full run takes about 11 minutes; it needs Python 3 with numpy.

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
open cases. The probe scripts and their outputs are in the evidence set
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
| Transient ducking | re-derived by probes, see `Synth::ducking` | Purnhagen §4.2 names transient reduction in the decoder, no form | d20–d25 |
| Mixing, ICC modes 0..2 | paper | Purnhagen §2.2 eq. 4–6 with eq. 8; Breebaart §6 eq. 31 | m02–m04 (eq. 8, not eq. 7; ICC values) |
| Mixing, ICC modes 3..5 | paper | Breebaart §6 eq. 24–30 | m04, m05 (ICC floor 0.05; the output matches μ² and μ rounded to single precision) |
| IPD/OPD as row phases OPD and OPD − IPD | paper | Breebaart §6 eq. 32 | m06–m09, m21, m29; p07, p08, p12 |
| IPD/OPD smoothing arg(u₀ + u₁/2 + u₂/4) | probes | none | m07, m27, m28, m30, m31 |
| Envelope ramps | paper | Purnhagen §3.3; Breebaart §7 | t02, t03, t07, t09, t21, t23; m20, m26 |
| Behaviour over frames (first frame, repeats, error frames, start index, 20 ↔ 34 changes) | probes | none | t01–t33, v_rust |

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
