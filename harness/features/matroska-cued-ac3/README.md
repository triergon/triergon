# Cued AC-3 source seek

The authored fixture contains the first eight 768-byte syncframes from
`harness/fixtures/encoded/avc-ac3.mkv` (see its manifest for provenance),
in two Cued Clusters. Its SHA-256 is
`0db686445de85d104857544800abfc0b3368e47524a8a2ed06333f7d302c8837`.
The generator is `cued_aac_fixture` with the argument `ac3`.

The [source-view comparison](contract.json) uses preloaded seekable input,
`NOPARSE` and `NOFILLIN`, a backward seek after flooring the target to the
stream time base, and every packet field and byte through successful EOF.
Both workers use `cued-ac3-fold`. For target 130 ms
both return 4 packets, 3,072 bytes and digest 4603375696001153562. For target
127.6 ms both return 8 packets, 6,144 bytes and digest 7084967468992935401.
The 130 ms complete source traces are byte-identical. Pinned FFprobe reports
128, 160, 192 and 224 ms for both PTS and DTS and 32 ms durations for the
four-packet suffix, matching the playback clock projection.

Native timing and memory are unmeasured. Other AC-3 configurations, malformed
packets and E-AC-3 are outside this comparison. RFC 9559 §§5.1.5, 10–11 and 22
specify the Cues and track conventions. ATSC A/52:2018 specifies syncframe
sample counts.
