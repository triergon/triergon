# Independent HLS AVC, HEVC and Dolby audio on Linux ARM64

The exact native workers matched 70 functional segments and one separate
allocation control. Ten selected workloads have complete first-clean time
and memory observations. Two meet the delivery thresholds and all ten
use strictly lower peak live heap and total peak RSS. All eight runtime
misses remain in the receipt.

| Case | Triergon median µs | FFmpeg median µs | Runtime upper 95% | Heap upper 95% | RSS upper 95% | Delivery |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| replay-avc-aac-06 | 334.5 | 333.9 | 1.0036× | 0.0072× | 0.6478× | Pass |
| replay-hevc-aac-06 | 371.4 | 340.4 | 1.0929× | 0.0088× | 0.6657× | Pass |
| replay-avc-ac3-06 | 328.8 | 185.4 | 1.7736× | 0.0102× | 0.6739× | Runtime miss |
| replay-hevc-ac3-06 | 364.2 | 192.2 | 1.8960× | 0.0131× | 0.7029× | Runtime miss |
| replay-avc-eac3-06 | 328.0 | 184.8 | 1.7788× | 0.0102× | 0.6746× | Runtime miss |
| replay-hevc-eac3-06 | 363.8 | 192.4 | 1.8972× | 0.0131× | 0.7025× | Runtime miss |
| dvb-avc-ac3-44100 | 952.7 | 289.9 | 3.2944× | 0.0202× | 0.6573× | Runtime miss |
| dvb-hevc-eac3-32000 | 841.5 | 265.7 | 3.1735× | 0.0216× | 0.7093× | Runtime miss |
| entry-hevc-aac-44100-01 | 363.6 | 274.9 | 1.3234× | 0.0104× | 0.6726× | Runtime miss |
| entry-avc-eac3-32000-01 | 283.4 | 159.5 | 1.7795× | 0.0095× | 0.6750× | Runtime miss |

The candidate commit `af33eb4742a87aaf923147f86babbc6a8a77c132` has the
same source tree as the reviewed Streem dependency pin
`4d4c4cfb5326700f01b432344db10cece0e3ac23`. The reference uses the pinned
trimmed d32 FFmpeg SDK and nofillin parsers. The operation retains all
selected units and compares complete packet bytes and clocks in per-lane
order. The timing window includes acquisition from the preloaded TS
source, packet assembly, clocks, folding and release.

Time uses 30 isolated pairs with 50 operations per sample. Memory uses
30 isolated pairs with one operation per sample and separate heap and RSS
observations. All cases use seed 280. The observers passed 28 runtime
calibration records and three kernel calibration records on the native
runner. The private archive was restored and hash-verified, and the raw
samples reproduce the recorded arithmetic. [Exact identities and hashes](results.json)

The [earlier AVC/AAC qualification](../hls-avc-aac-linux-arm64/README.md)
keeps its original workers, inputs, protocol and verdicts. The expanded
HLS runtime gate remains open.
