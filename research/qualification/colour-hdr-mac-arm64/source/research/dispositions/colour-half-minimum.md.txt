# Colour rational boundary investigation

The source value `0x1.0000000200000p-32` is the representable result of
`0.5 / i32::MAX`. Exact binary closest-rational arithmetic chooses 0/1; pinned
FFmpeg's public view chooses 1/2147483647 at that rounded boundary. Adjacent
representable values choose 0/1 below and 1/2147483647 above. The original raw
mismatch is preserved in `.research/colour-rational-boundary-results.json`.

The optional FFmpeg view now models this exact boundary explicitly. Source
floats/IEEE bits are unchanged and independently required; no epsilon comparison,
universal normalization, case exclusion or numeric gate change was introduced.
The original 32 profiles are retained and `colour-master-half-minimum` is added as
the 33rd required profile. The native adapter reads public metadata directly; it
has no correction for this case. Seeded binary32/binary64 comparisons remain.
