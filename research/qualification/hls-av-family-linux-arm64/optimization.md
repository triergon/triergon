# HLS transport and audio optimization on Linux ARM64

The native worker at `225cdaacc66bdafa55bf6b490e9c165a5c643bc1` matched all 71 frozen packet and clock literals.
10 of ten workloads passed
the 1.25× runtime ceiling and 10 of ten used strictly
lower peak live heap and total peak RSS. The 0.80× optimization target
passed in 8 workloads.

| Workload | Triergon median µs | FFmpeg median µs | Runtime upper 95% | Heap upper 95% | RSS upper 95% |
| --- | ---: | ---: | ---: | ---: | ---: |
| replay-avc-aac-06 | 135.0 | 333.2 | 0.4068× | 0.0067× | 0.6451× |
| replay-hevc-aac-06 | 159.3 | 340.0 | 0.4700× | 0.0084× | 0.6620× |
| replay-avc-ac3-06 | 133.6 | 185.1 | 0.7233× | 0.0095× | 0.6708× |
| replay-hevc-ac3-06 | 152.9 | 192.2 | 0.7982× | 0.0123× | 0.6989× |
| replay-avc-eac3-06 | 133.4 | 184.8 | 0.7241× | 0.0095× | 0.6708× |
| replay-hevc-eac3-06 | 152.6 | 192.1 | 0.7987× | 0.0123× | 0.6997× |
| dvb-avc-ac3-44100 | 353.6 | 289.1 | 1.2254× | 0.0197× | 0.6544× |
| dvb-hevc-eac3-32000 | 315.8 | 265.1 | 1.1970× | 0.0213× | 0.7069× |
| entry-hevc-aac-44100-01 | 146.8 | 273.8 | 0.5377× | 0.0100× | 0.6707× |
| entry-avc-eac3-32000-01 | 105.5 | 159.5 | 0.6608× | 0.0086× | 0.6719× |

The code skips unused trace delivery, bounds shared input, validates
PES continuations in place and batches header checks. The workload still
folds every selected payload and clock through EOF. Both workers kept the
same operation and reference SDK as the initial run.

| Candidate | Runtime passes | Both memory gates | AVC/AC-3 DVB | HEVC/E-AC-3 DVB |
| --- | ---: | ---: | ---: | ---: |
| `af33eb4` | 2/10 | 10/10 | 3.2944× | 3.1735× |
| `e3101ae` | 8/10 | 10/10 | 2.0609× | 1.9850× |
| `82931b7` | 8/10 | 10/10 | 1.7632× | 1.6983× |
| `ebf58ec` | 8/10 | 10/10 | 1.6986× | 1.6471× |
| `944bffb` | 8/10 | 10/10 | 1.5056× | 1.4426× |
| `106fa48` | 8/10 | 10/10 | 1.4581× | 1.3855× |
| `9a8d998` | 9/10 | 10/10 | 1.2804× | 1.2312× |
| `5f468e6` | 9/10 | 10/10 | 1.2618× | 1.2201× |
| `fc731a7` | 9/10 | 10/10 | 1.2622× | 1.2218× |
| `225cdaa` | 10/10 | 10/10 | 1.2254× | 1.1970× |

Each row binds a distinct candidate to the frozen input, reference worker,
30 isolated pairs, 50 operations per timing sample, one operation per
memory sample and the existing 95% calculations. The source and raw
archives were restored from private storage and checked by SHA-256.
The receipt retains first-clean claims and the recorded failures.
[Initial result](README.md) · [Revision-specific results](optimization-results.json)

The optimized library still needs its own Streem dependency pin and
application replays. The remaining TS/M2TS and elementary profiles are
tracked separately under O31.
