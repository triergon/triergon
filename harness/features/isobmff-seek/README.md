# ISO BMFF seek and range plans

This contract compares the ISO BMFF worker's seeks and source range plans with the
`ffmpeg-isobmff-native` reference worker, through the same seek modes as the open
and index suite (`seek-trace`, `seek-bench`, `seek-memory`): a track ID, a time in
that track's media clock, a packet count of at most 64 and whether to read to the
end of file first, then the window, retention, I/O mode and work quantum.

- A seek runs the candidate's FFmpeg-compatible `GlobalSeekJob` and the reference's
  public backward seek. Their ordered traces must be byte-identical.
- A range plan runs the candidate with `--range-plan` before the mode:
  `MovieSourceSeekPlanner` with published `roll` recovery at the requested time,
  then the plan's samples from its decode start through its target. The reference
  runs the same seek and reads as many packets as the plan holds. The packets must
  have the same track, source positions, sizes and payloads in order; each role
  folds its own work digest from them.
- A refusal is an input whose open both workers refuse with the contract's
  diagnostics. It is never timed.

`corpus.json` lists the 476 cases in order with their inputs, arguments and each
role's expected work for one operation and for the fifty timed iterations, which the
suite checks compose from one. Inputs come from two directories:

- `harness/fixtures/isobmff-seek`: eight authored movies, written by
  `cargo xtask isobmff-seek fixtures --out DIR` from their box structure
  (`xtask/src/isobmff_seek/fixtures.rs`) and checked byte for byte by the suite's
  tests. They are regular and fragmented, with one track or two at the same clock,
  sync samples every 28 to 48 samples, `stco` and `co64`, front and end movie
  boxes, composition offsets, a media-time edit and a leading empty edit.
- `harness/fixtures/isobmff-open-index`: 36 inputs of the open and index corpus,
  two of each family whose first track the seek reproduces at the start, the middle
  and beyond the end, one of each family for range plans, and the refused opens.

Each case was kept only when both workers' outputs compared equal for it. The
requests that differ, and those the candidate refuses as unsupported, are listed in
the crate README as not compared.
