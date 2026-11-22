# M2TS192 program inputs

M2TS192 copies of three synthetic HLS segments in [`../hls`](../hls/README.md), for the
`ts-program` contract (`harness/features/ts-program/`). The fixture writer prefixes each
188-byte transport packet with a four-byte TP_extra_header carrying copy permission zero and
an arrival time stamp that advances by 2,700 ticks of the 27 MHz clock per packet from
zero. The transport packets are unchanged.

```sh
cargo xtask fixture m2ts ../hls/replay-avc-aac-00.ts replay-avc-aac-00.m2ts
cargo xtask fixture m2ts ../hls/replay-hevc-aac-00.ts replay-hevc-aac-00.m2ts
cargo xtask fixture m2ts ../hls/entry-hevc-aac-44100-01.ts entry-hevc-aac-44100-01.m2ts
```

| File | SHA-256 |
|---|---|
| `entry-hevc-aac-44100-01.m2ts` | `2a723565db7523bf01498ceb8e316b82dfb9d99347caf06080c6755979b36c77` |
| `replay-avc-aac-00.m2ts` | `c259510f2149db3bb931c3238897e1acf45d62f5bce4b5c3cbd397473cb8f609` |
| `replay-hevc-aac-00.m2ts` | `92bcbc9665074e78d5dbbd7e84ace5c8beb8bfcdc277f5c3ad0ca9f2965a5c54` |

The complete AVC/AAC program control selects both PID 256 and PID 257 from
`replay-avc-aac-00.m2ts` in one forward-only operation. It compares the ordered
33 packets (24 AVC, 9 AAC), 26,592 payload bytes, clocks, flags and EOF against
the pinned no-parser reference. Record it reproducibly with
`cargo xtask fixture m2ts-program --record PATH/ffmpeg-formats-worker`; the
reference's sibling `identity.json` must bind its executable and pinned revision.
The command verifies every existing case and pin, regenerates the same fixture
bytes with the M2TS writer, and appends only the combined program control.
