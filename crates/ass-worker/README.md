# ass-worker

ASS parsing measurement worker for the harness. It reads whole scripts, runs
Matroska packet streams and walks events through
[triergon-ass](../ass/README.md), as the libass reference worker
(`harness/reference_ass.h`) does with libass 0.17.5. It is tooling
(`publish = false`), not part of the library.

## Command line

```text
ass-worker ass-fold   FILE MODE
ass-worker ass-bench  FILE ITERATIONS MODE
ass-worker ass-memory FILE MODE
ass-worker ass-dump   FILE MODE
```

| MODE | FILE | Operation |
|---|---|---|
| `script` | a script | read whole; the track is recorded |
| `chunks` | a stream fixture | its commands run on a new track; the track is recorded at every `@dump` and at the end |
| `script events`, `chunks events` | either | every event of the script or stream walked at the fixture's times |

A fixture's directives (`;@force`, `;@extract-fonts`, `;@time`) and a stream's
commands (`@private`, `@data`, `@styles` blocks, `@chunk`, `@dump`, `@flush`,
`@prune`, `@check-readorder`) are described in `src/fixture.rs`. A record is a
kind and integers, doubles, strings or absent values (`src/record.rs`): the
track, its formats, styles, events and embedded fonts; per walk every glyph's
state, drawing outlines, the event state, the vector clip and karaoke results.
The fold, bench and memory modes print the harness protocol's one JSON line,
whose `digest` folds every record in order; the bench mode adds the elapsed time
of the timed loop and the memory mode brackets one operation with the
allocation-window markers. Reading the file, its directives and commands, and
for a walk the script, are outside both. The dump mode prints the same records
as lines, identical to the reference's dump. The features are
`harness/features/ass-parse-*/`.
