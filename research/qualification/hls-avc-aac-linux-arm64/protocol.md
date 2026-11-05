# HlsAvcAacParserOnlyFeedFoldV1

Both workers preload identical input before the observed window. Each
operation opens and probes one forward-only TS188 segment, retains every
selected AVC and AAC output in decoder delivery order through clean EOF,
resolves the application AAC presentation clock, folds the complete output,
and releases all operation storage. The reference uses the pinned trimmed
d32 SDK with the stock AAC parser, without the AAC decoder, and an explicit
application adapter. The raw reference and the separate full-FFmpeg clock
proposal retain their original outputs and identities.

For AVC the operation reports every feed byte, raw PTS/DTS in 90 kHz ticks,
IDR and raster. If an IDR needs cached parameter sets the selected feed
prepends them. AAC retains each ADTS frame. A positive reference packet
duration supplies the audio duration. The admitted parser-only reference
uses ADTS-derived integer nanoseconds when its duration is absent. The
application clock uses the original PTS or DTS when available and resolves
holes using those durations. Compare absolute presentation nanoseconds,
duration nanoseconds, maximum video PTS and final audio presentation time.
The playlist timeline, HTTP policy and sink queue are caller-owned.

For each segment begin the rolling fold at zero and mix unsigned words as
wrapping multiply by 31 plus the word. Video mixes tag 1, PTS, DTS, IDR, width,
height, feed length and all feed bytes. Audio mixes tag 2, presentation
nanoseconds, duration nanoseconds, feed length and all ADTS bytes. Finally
mix maximum video PTS and final audio PTS, using -1 for absent audio.
Repeated runs mix each segment digest into a separate zero-initialized fold.
All output retention and clock recovery are within the measurement. Source
file loading and check-mode text serialization are outside it.

The four selected fixture hashes and one separate calibration control are
frozen in MANIFEST.json. All must match literally before native admission.
H0 uses 50 repetitions for time, 30 isolated pairs for time and memory, and
seed 280. Initial delivery requires paired upper95 time at most 1.25 and
strictly lower peak live heap and total resident peak RSS. The 0.80 runtime
optimization target and original RSS growth evidence remain separate.
