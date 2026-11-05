# Matroska audio startup and forward clocks on Linux ARM64

Source commit `2e8cab493af770ee48526cac1f258e776163bb0b` matched all
ten selected inputs and all 75 packets. The inputs are the four frozen
audio discovery controls and six authored Dolby timing controls. Each
operation opens the same entity, discovers the streams, and folds every
selected AAC-LC, AC-3 and E-AC-3 packet through EOF. The comparison covers
track order, PTS, DTS, duration and every packet payload byte. The
reference uses FFmpeg revision `d32b387f2b0a484599d4587d651891f0c63c4238`.

A separate native Linux ARM64 observer compared the ordered track index,
PTS, DTS, duration, payload length and SHA-256 of each of the 75 packets
against the frozen FFprobe packet records. All ten inputs reached EOF and
matched. This comparison supplies packet-level parity evidence alongside
the original timing and memory run. The original workers and results are
retained. The receipt lists hashes for the trace files, observer, checker
and verified private archive.

| Gate | Passing cases | Worst paired upper ratio |
|---|---:|---:|
| Selected parity | 10/10 | |
| Runtime at most 1.25× | 10/10 | 0.170229× |
| Strictly lower peak live heap | 10/10 | 0.069728× |
| Strictly lower total peak RSS | 10/10 | 0.597827× |
| Runtime at most 0.80× | 10/10 | 0.170229× |

Each case used 30 isolated pairs, 50 timing iterations per pair member,
and the separate heap and total resident peak observers. Ratios are
rounded up. The prior host refusal of the damaged AC-3 packet remains
in the retained history. The successful run includes that original
packet and its bytes. Syncframe admission remains a separate operation.

See the [per-case results and worker, source, input and archive
identities](results.json). The evidence covers this selected startup and
forward clock operation. Audio seek, device playback and Streem adoption
remain open.
