# Cued E-AC-3 source seek

The fixture contains the first eight 768-byte syncframes from
`harness/fixtures/encoded/avc-eac3.mkv` (see its manifest for provenance), in
two Cued Clusters. Generate it with `cargo run -p triergon-matroska --example
cued_aac_fixture -- harness/features/matroska-cued-eac3/inputs/coded-cues.mkv eac3`.
Its SHA-256 is `5dee8e8b4b0e217d1de4e32beaf71820f37674e460bf31bb3a82840fda63bc9e`.
Each header specifies six blocks, or 1,536 samples at 48 kHz, with one channel.

The [source-view comparison](contract.json) uses preloaded seekable input,
NOPARSE and NOFILLIN, a backward seek after flooring the target to the stream
time base, and every packet field and byte through EOF. At 130 ms both workers
return four packets, 3,072 bytes and digest 18072471727007900406; at 127.6 ms
both return eight packets, 6,144 bytes and digest 9717410312991101686. The
130 ms source traces are byte-identical. The playback projection produces PTS,
DTS and 32 ms durations at 128, 160, 192 and 224 ms.

Other track configurations and malformed packets are outside this comparison.
RFC 9559 specifies the Cues and track conventions; ATSC A/52:2018 specifies
syncframe sample counts. Native timing and memory are unmeasured.
