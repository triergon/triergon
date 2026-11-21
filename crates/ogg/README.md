# triergon-ogg

An Ogg (RFC 3533) reader with the Vorbis, Opus (RFC 7845), FLAC, Theora and Speex
mappings, implementing the core `ContainerReader`. It reads multiplexed and chained
physical streams from incremental input, times packets from granule positions, and seeks
by bisection.

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::container::{PacketInfo, TrackInfo};
use triergon_ogg::{BlockingReader, Limits};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let mut file = Vec::new();
# triergon_ogg::page::write_page(2, 0, 1, 0, &[4], b"head", &mut file);
let cancel = AtomicBool::new(false);
let mut reader = BlockingReader::open(std::io::Cursor::new(file), Limits::default())?;
for track in reader.tracks() {
    let _ = (track.codec(), track.configuration(), track.sample_rate());
}
while let Some(packet) = reader.next_packet(&cancel)? {
    let _ = (packet.track(), packet.pts(), packet.duration(), packet.trim(), packet.payload());
}
# Ok(())
# }
```

`BlockingReader::open` takes `Read + Seek` (seeking bisects); `open_stream` takes a
sequential `Read`. `Reader` is the push reader: it performs no I/O, accepts windows of
any size down to one byte, steps under a `WorkBudget` and a cancellation flag, and
charges its storage to the caller's `Budget`.

## What it reads

- **Pages**: capture-pattern search, CRC check, resynchronisation. A page with a bad
  CRC, garbage between pages and a trailing partial page are skipped and reported as
  `ReaderEvent::Resync`; a sequence gap drops the packet it broke (`ReaderEvent::Lost`).
- **Packets**: any length across any number of pages, empty packets and 255-multiple
  lengths. A packet inside one page shares the input window; only a packet split
  across pages is gathered.
- **Logical streams**: interleaved serials of one link, each a track. A new group of
  beginning-of-stream pages after media starts a chained link (`ReaderEvent::Link`)
  whose tracks follow the previous link on one timeline. Track ids are the serial in the
  low 32 bits and the link above them.
- **Mappings** (Ogg has no configuration field; the header packets form it):

  | Mapping | `codec_tag` | `configuration()` |
  |---|---|---|
  | Vorbis | `\x01vorbis` | identification, comment and setup headers, Xiph-laced |
  | Opus | `OpusHead` | the `OpusHead` packet (`OpusTags` is `Track::comments`) |
  | FLAC | `\x7fFLAC` | `fLaC` with STREAMINFO and every further metadata block |
  | Theora | `\x80theora` | identification, comment and setup headers, Xiph-laced |
  | Speex | `Speex   ` | the Speex header packet (the comment header is `Track::comments`) |
  | anything else | its known magic (Daala, VP8, Skeleton, ...) or first 8 bytes | the first packet |

  Header packets are never delivered as packets (Speex: the header, the comment header
  and as many more as its `extra_headers` says). Other mappings are `CodecId::Other`;
  their packets carry no time.

## Timing

Packet times are ticks of 1/sample rate (Opus 1/48000), and for Theora of one frame
(FRD/FRN seconds, `Track::frame_rate`). Each page's packets are timed backwards from its
granule position with the samples each packet completes: Vorbis from the block sizes
the setup header's modes give (the first packet completes none), Opus from its TOC,
FLAC from its frame header, Speex from the header's frame size times its frames per
packet; a Theora packet is one frame, and its granule position is split at the
identification header's KFGSHIFT into the frame count up to the last keyframe and the
frames since (Theora I appendix A.2.3; bitstreams before 3.2.1 count from 0). The last
page is timed forwards; for Vorbis, Opus and Speex the samples its granule position
leaves out are the last packets' `Trim::end`, and samples before time zero (Opus
pre-skip, a start offset) are `Trim::start`. Every audio packet is a random-access
point; of Theora, the intra frames. DTS equals PTS.
`Track::end` holds the last granule position, found by reading at most
`Limits::end_scan_bytes` at the end of a seekable source of known length.

`compat::clock::packet_timing` reports FFmpeg's clock instead (skip samples, discard
padding and FFmpeg's Vorbis durations); its documentation lists the differences.
For the first nonterminal Opus audio page, it uses the granule-derived start
if later than the header anchor. This includes cropped streams and the first
surviving page after initial audio loss.

## Seeking

`request_seek` lands on the packet containing the target (Vorbis: the packet before
it; Theora: the keyframe the target frame is decoded from), or for `PlaybackPreroll` on
the one containing the target less the preroll (Opus 80 ms), discarding output up to
the target. The target is on the physical stream's timeline: in a chained source the
seek lands in the link holding it, on the track at the requested track's position in
that link, following into links reading has not reached yet. Seekable sources of known
length are bisected by granule position within that link, each probe reading at most
`Limits::seek_probe_bytes` (a Theora target that is not a keyframe is bisected again
for the keyframe its granule positions name); other sources are read again from the
start (`SeekMethod::Restart`).

## Limits

`Limits::default()`: packets of up to 16 MiB gathered across pages, 16 MiB of headers per
track (cover art in a comment header), 64 logical streams per link, 1024 tracks, and
two maximum pages (`page::MAX_PAGE`, 65,307 bytes) of skipped bytes in a row, four per
seek probe and two for the end scan, in a 64 MiB pool. Charged to the budget: gathered
packets and split pages, track configurations, and the track, stream and link tables.
The scratch lists of one page are fixed at its 255 lacing values. A limit violation is an
error; a full pool is `Step::Backpressure`, and the step resumes once output is released.

## Differences from FFmpeg

The reader's own timing differs from FFmpeg 9.0.2 where FFmpeg starts the first Vorbis
packet as many samples before zero as it states alone (this reader times the overlap it
primes as no output), starts a short Vorbis block after a long one late by the
difference of their quarter blocks, times chained links' starts and packets after a lost
page from the page rather than the granule position, and reports no Speex trims,
shortening the last packet instead. `compat::clock` reproduces those, and still differs
where FFmpeg:

- prints a negative last duration modulo 2^32;
- times the first packet after a page that fails its CRC from its own page
  (`compat::clock` times it from the stream start);
- times the Vorbis packets of leading pages stating granule position 0 from the next
  page's;
- reads a chained link that reuses the previous link's serial number as the same stream,
  its headers as packets;
- seeks in a chained file within one link only.

The reviewed differences are in
[reader-differences.json](../../harness/codecs/reader-differences.json); results in
[docs/status.md](../../docs/status.md).

The `read_packets` helper reads every packet of a whole file in memory, headers
included, for tests and tools.
