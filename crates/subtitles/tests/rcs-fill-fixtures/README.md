# Original inactive-background source art

Four independently authored two-display sources exercise RCS fill/background
semantics. The first display explicitly initializes every pixel in a4x2 region.
The two inactive-background change sources violate EN300743 §5.1.5; they are
receiver-tolerance tests, not source-conformance tests. The earlier claim that
these changes were conforming was wrong. Their bytes and runtime proof remain
preserved; the two controls do not violate that background rule.
The second changes the encoded background while fill is clear, or provides an
unchanged-background/active-fill control. Object updates supply current ODS;
partial CLUT updates retain the original pixels and source CDS facts. No cached
compressed-object replay is required or claimed.

The index and SourceNearest RGBA files were specified before the correction.
The tests separately retain exact opacity128/256 and StudioUnrepaired alpha127
for the partial CLUT update. They do not replace original source RGBA, native
observations, palette words or region order.

Primary: [ETSI EN300743v1.6.1](https://www.etsi.org/deliver/etsi_en/300700_300799/300743/01.06.01_60/en_300743v010601p.pdf),
§§5.4.3,7.2.3–5. Cached PDF SHA-256:
`1a5b211303397c52b771eb11e8f9f844d88727ecccd3fe0a5e8a6ac961d93e87`.
Original generator and immutable source hashes are retained in the ignored
`.research/parallel-subtitles/rcs-fill-batch-1/` lane. The actual pre-fix parent
run recorded2 passed/2 failed; the two failures were the inactive-background
rejection. The parent then ran all47 subtitle tests with0 failures and Clippy
with `-D warnings` passed, preserving identical source before/after. Logs are
retained in `.research/dvb-rcs-fill-green-1793444038077152000/`. This correctness
result does not establish native parity or numerical acceptance.

No FFmpeg or other implementation code supplied bytes or expected art. These
fixtures do not establish native parity or full selected M05 qualification.
