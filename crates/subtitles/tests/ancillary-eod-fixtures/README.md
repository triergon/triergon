# Original selected-page EOD sources

Six authored cases cover shared ancillary-page EOD, same-PTS complete-segment PES
splitting, nonempty-EOD rejection and three selection/tolerance controls. Original
source/index/RGBA bytes are unchanged from the parent-observed red batch:3 failed,
3 controls passed. One control deliberately uses a nonconforming composition-page
EOD after shared data and tests receiver tolerance only; foreign-service
completeness is not asserted by the filtering control.

Primary: [EN300743v1.6.1 §7.2.6](https://www.etsi.org/deliver/etsi_en/300700_300799/300743/01.06.01_60/en_300743v010601p.pdf),
with §§6.2/8 for framing and page dependency/order. PDF SHA-256:
`1a5b211303397c52b771eb11e8f9f844d88727ecccd3fe0a5e8a6ac961d93e87`.
Current ODS are transmitted; no cached-compressed-object replay is inferred.
Original author/provenance and red proof remain in the ignored
`.research/parallel-subtitles/ancillary-eod-batch-1/` lane. The parent subsequently
passed all65 promoted regressions, all-target Clippy with `-D warnings`, and the
release library build with unchanged source; historical evidence is preserved
in `.research/subtitles-eod-host-1793453041434070000/`. The text96 private-v2 green
result and its subsequent public161 validation are separate identities. No native
parity or numerical claim follows from these EOD results.
