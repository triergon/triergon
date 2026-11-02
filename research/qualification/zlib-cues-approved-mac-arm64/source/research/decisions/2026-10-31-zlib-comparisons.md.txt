# Approved treatment of two zlib reference differences

The owner approved the [measured proposal](../review/zlib-reference-comparison/README.md)
after its reference/benchmark implications were explained, on 2026-10-31:

> you are saying we are going to support more what ffmpeg does in that case? I approve then

The proposal snapshot retains its original pending-review status as historical
provenance. This decision and `harness/comparison-rules.json` record subsequent
approval. No old failed receipt is changed retroactively.

- **Empty PCM:** retain and independently verify the complete source-frame event,
  restored bytes/private data and exact timing. Compare decoder input through an
  explicit optional view, which emits no sample packet for empty little-endian PCM
  without packet trim side data. The production source API/default trace retain the
  event; other codecs and side-only events are not universally filtered. Complete
  decompression and checksum validation precede the projection inside timed work.
- **Combined encoding:** compare the selected scope-1 header/zlib chain against
  pinned FFmpeg 9.0 plus the identified native restoration supplement. The pinned
  library lacks this feature; the installed 9.0.2 also reports it unsupported. The
  supplement independently reads the source encoding plan and uses the same system
  zlib as FFmpeg. Its metadata parsing, decompression/checksum, allocations, output
  and teardown are included. This is an augmented reference, not a stock-FFmpeg
  performance result for that case. Combined private-data scopes are explicitly
  unsupported by this reference supplement and are not claimed qualified.

All ten K2 and eight K3 required profiles remain. The >=30-pair time upper bound
<=0.80 and both lower-memory upper bounds <1.0 remain unchanged. Source content and
exact source time are separate mandatory assertions. The native output differences
remain in each case's raw evidence; an unexpected difference is a failure.

Rules bind exact fixture hashes, profile IDs and the FFmpeg source revision. Each
case records its comparison view and selected reference kind/hash. Timing and memory
certificates must identify that selected worker, with unchanged inputs. The rules
cannot become a global normalizer or substitute an arbitrary slower worker.

Production remains two owned safe-Rust packages with no third-party dependencies.
The native supplement is external test tooling. No Streem, TV or full-module
qualification follows from these two dispositions.
