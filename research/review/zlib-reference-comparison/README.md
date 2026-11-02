# Proposed disposition of two FFmpeg reference differences

Status: **owner review pending; neither case is officially accepted**. No threshold,
required case or source-frame assertion has been removed. The original source-frame
comparisons remain failing and their receipts remain available.

## Concrete comparison proposal

1. Keep the production source-frame API and its complete default trace unchanged.
   An empty decoded source frame, its timestamp and codec-private data still have
   to pass the independent Python EBML/zlib and exact source-time oracles.
2. For the empty PCM case, compare decoder input in the explicitly optional
   `codec-*` worker view: empty little-endian PCM without packet trim side data
   carries zero samples, so it delivers no decoder packet. The source event stays
   visible in the default trace and source clock view. Other codecs/side-only
   events are not universally filtered. This projection runs only after complete
   decompression and checksum validation, including in timed work.
3. For the selected mixed header/zlib chain, use the visibly identified reference
   pipeline **pinned FFmpeg 9.0 plus native combined-encoding restoration**. Stock
   FFmpeg explicitly reports that combined encodings are unsupported; its raw
   compressed output is preserved as evidence. The independent C tooling reads the
   encoding plan from source metadata, applies its stages in descending order using
   the same system zlib FFmpeg already uses, and restores headers. All metadata,
   inflate/checksum, allocation, payload production and teardown work is timed.
   This changes the reference for that case; it is not a claim that unmodified
   FFmpeg supplies the missing feature. The production graph stays dependency-free
   safe Rust. This prototype supports combined scope 1 only and is never called
   proof of combined private-data or complete-module parity.
4. Keep >=30 paired measurements, the runtime upper 95% ratio <=0.80, and both
   lower-memory upper ratios <1.0, per required case. Keep FFmpeg as the ordinary
   reference everywhere its native operation supplies the selected work.

## Review evidence

The prototype matches the complete default source-frame trace on 48 combinations:
three DEFLATE types, four lacing choices, empty/nonempty content, and both storage
orders of the two encoding stages. Independent Python/zlib tests check normative
restored bytes; no output is taken from the candidate to repair the reference.
The optional empty-PCM view has a regression proving the source event remains
present, and that an unrelated codec is not filtered. All 34 Rust and 19 Python
tests plus lint, shipping-feature, release, safety and design checks passed.

`measurements.json` preserves 30 raw paired timing and memory observations for
each proposed case, input and binary hashes, the system zlib/FFmpeg pins and all
checksummed work counters. `summary.json` gives the unchanged-threshold results.
Both proposed comparisons meet those numerical gates. Their adoption remains a
separate decision because it changes the comparison view/reference of two frozen
cases. It does not qualify a complete module, Streem or the TV.
