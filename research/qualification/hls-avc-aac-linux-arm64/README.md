# Independent AVC/AAC HLS segments on native Linux ARM64

All four frozen cases match the pinned FFmpeg reference and pass runtime,
peak live heap and total peak RSS at Triergon revision
e499beacb4132ccbc40f6cb6235e0414e7cc8cc9. This is the development revision
used by Streem's opt-in HLS integration. It is the head of [PR #8](https://github.com/triergon/triergon/pull/8);
the result does not qualify a different revision of main.

| Frozen case | Triergon median µs | FFmpeg median µs | Time upper 95% | Heap upper 95% | Total RSS upper 95% |
| --- | ---: | ---: | ---: | ---: | ---: |
| bframes-multislice | 172.11 | 210.34 | 0.82118× | 0.00569× | 0.60314× |
| baseline-stereo | 115.86 | 159.98 | 0.72683× | 0.00478× | 0.62513× |
| media-00 | 329.36 | 432.97 | 0.76292× | 0.00453× | 0.59789× |
| media-01 | 326.52 | 435.36 | 0.75206× | 0.00435× | 0.59962× |

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
[Earlier results](historical.json) apply to the earlier archived source
with overlays. That run also passed delivery in all four cases, with
different source and candidate binary identities. Its original cold
control failure remains in the archived run.
