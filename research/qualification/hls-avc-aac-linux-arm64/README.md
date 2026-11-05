# Independent AVC/AAC HLS segments on native Linux ARM64

All four frozen cases match the pinned FFmpeg reference and pass runtime,
peak live heap and total peak RSS at Triergon revision
3f7547922e7be74ab21baf12f92851a161388c33. Streem's current opt-in
integration uses this development revision from [PR #10](https://github.com/triergon/triergon/pull/10).
The native runner rebuilt the candidate and collected fresh measurements.

| Frozen case | Triergon median µs | FFmpeg median µs | Time upper 95% | Heap upper 95% | Total RSS upper 95% |
| --- | ---: | ---: | ---: | ---: | ---: |
| bframes-multislice | 172.27 | 209.12 | 0.82437× | 0.00569× | 0.58783× |
| baseline-stereo | 116.12 | 160.75 | 0.72310× | 0.00478× | 0.60941× |
| media-00 | 329.78 | 430.20 | 0.76865× | 0.00453× | 0.58309× |
| media-01 | 326.91 | 434.17 | 0.75414× | 0.00435× | 0.58373× |

Delivery passed 4/4 at 1.25× time and strictly lower memory. Three cases
met the 0.80× optimization target. Time and memory each use 30 isolated
pairs; each time sample runs 50 fresh operations. The harness checked the
frozen allocation and RSS observer controls before scoring. The allocating
reference control retained 4,064 bytes of first-use process state in the
earlier cold run, which stopped memory admission. The supplemental control
accounts for that state before measurement. Scored operations remain cold
and retain the original observer and RSS bound.

The [comparison protocol](protocol.md) covers complete segment packet
retention, AVC bytes and timestamps, ADTS AAC frames and the resolved AAC
clock. The reference is the pinned trimmed FFmpeg build with its AAC parser
and the app's duration fallback. Playlist and network behavior, decoder
output and hardware playback are separate application checks. Other TS
and HLS profiles remain open.

[Current per-case results and artifact identities](results.json) bind the
source, input hashes, workers, observers and retained native archive.
The [previous Streem pin results](e499beac-results.json) preserve the
separate native run of e499beac. [Earlier results](historical.json)
apply to the archived source
with overlays. That run also passed delivery in all four cases, with
different source and candidate binary identities. Its original cold
control failure remains in the archived run.
