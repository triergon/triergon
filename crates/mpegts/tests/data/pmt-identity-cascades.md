# PMT positional identity observations

Reference: FFmpeg revision `d32b387f2b0a484599d4587d651891f0c63c4238`.
The ffprobe executable SHA-256 is
`d9d0840413169e2ec50eb1a2e3c9bebf3aa5a15ab97217ac987ca9a901b33896`;
reference build metadata SHA-256 is
`685fea471abe92bda3a518ee337071fc80f517cf4b7dbe4adc0491d603eccf24`.

`identity_black_box_cascades` in `../demux_streams.rs` authors nine cascades,
24 cumulative-prefix TS188 inputs and 48 probe invocations. The fixtures are
original synthetic structures under the repository licence. Every PMT version
has a CRC, followed by finite marker PES on its listed PIDs. Each PID has its
own continuity counter. No downloaded media is part of this fixture.

Reproduce from the checkout root with a pinned executable:

```sh
CARGO_INCREMENTAL=0 TRIERGON_PMT_FFPROBE=/path/to/ffprobe \
  cargo test -p triergon-mpegts --test demux_streams identity_black_box -- --ignored
```

To compare another local ffprobe against every recorded input and output, run:

```sh
CARGO_INCREMENTAL=0 cargo xtask transport-fate pmt-identity /path/to/ffprobe
```

This prints the executable SHA-256 and checks all 48 complete stdout and stderr
hashes and packet assignments against the recorded observations. A passing output
comparison does not identify the executable as the original pinned build. The
original test verifies the executable hash before invoking it. The argument list is
`-v error -bitexact -f mpegts -fflags +noparse+nofillin -merge_pmt_versions 0|1
-show_entries stream=index,id,codec_tag:packet=stream_index,pos,size
-show_packets -of compact -i INPUT`. Marker packet positions identify the wire
PID independently of the reference's stream index. Parser framing is disabled
so codec parsing cannot combine successive marker PES. These marker bytes do
not establish codec decoding or the default parser's packet boundaries.

`pmt-identity-cascades.txt` records each input hash, complete stdout/stderr hashes,
all emitted packet PID/index assignments and global stream IDs. The test compares
them exactly. Inputs and raw output remain in ignored `.research/pmt-identity`.
The ordinary original-input probe is separately pinned in `pmt-merge-probe.txt`
and `../original_program.rs`.

Complete replacements reuse positions even when type or language changes.
Elementary `0x52` tag swaps exchange the two packet assignments. A surviving
PID can cause two later positions to select one identity; a removed position
can allocate a new identity when it reappears. Those observations are outside
`PositionalStreamIds`' accepted scope, and its tests require explicit refusals.
No general descriptor matching rule is inferred from these probes.
