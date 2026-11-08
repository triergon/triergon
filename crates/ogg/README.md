# triergon-ogg

An Ogg (RFC 3533) reader with the Vorbis, Opus (RFC 7845) and FLAC mappings, implementing
the core `ContainerReader`. It reads multiplexed and chained physical streams from
incremental input, times packets from granule positions, and seeks by bisection.

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
  | anything else | its known magic (Theora, Speex, Skeleton, ...) or first 8 bytes | the first packet |

  Header packets are never delivered as packets. Other mappings are `CodecId::Other`;
  their packets carry no time.

## Timing

Packet times are ticks of 1/sample rate (Opus 1/48000). Each page's packets are timed
backwards from its granule position with the samples each packet completes: Vorbis from
the block sizes the setup header's modes give (the first packet completes none), Opus
from its TOC, FLAC from its frame header. The last page is timed forwards; samples its
granule position leaves out are the last packets' `Trim::end`. Samples before time zero
(Opus pre-skip, a Vorbis or Opus start offset) are `Trim::start`. DTS equals PTS.
`Track::end` holds the last granule position, found by reading at most
`Limits::end_scan_bytes` at the end of a seekable source of known length.

`compat::clock::packet_timing` reports FFmpeg's clock instead (skip samples, discard
padding and FFmpeg's Vorbis durations); its documentation lists the differences.

## Seeking

`request_seek` lands on the packet containing the target (Vorbis: the packet before
it), or for `PlaybackPreroll` on the one containing the target less the preroll (Opus
80 ms), discarding output up to the target. Seekable sources of known length are
bisected by granule position within the track's link, each probe reading at most
`Limits::seek_probe_bytes`; other sources are read again from the start
(`SeekMethod::Restart`).

## Limits

`Limits::default()`: packets of up to 16 MiB gathered across pages, 16 MiB of headers per
track (cover art in a comment header), 64 logical streams per link, 1024 tracks, 128 KiB
of skipped bytes in a row, 256 KiB per seek probe, 128 KiB end scan. Charged to the
budget: gathered packets and split pages, track configurations, and the track, stream
and link tables. The scratch lists of one page are fixed at its 255 lacing values. A
limit violation is an error; a full pool is `Step::Backpressure`, and the step resumes
once output is released.

## Comparison with FFmpeg

`cargo xtask codecs readers` compares every packet `BlockingReader` delivers with the
pinned `ffprobe -show_packets` of FFmpeg 9.0.2 in two views: the reader's own timing
(track, pts, dts, duration, trims, size) and `compat::clock`, the FFmpeg-compatible view
of the same packets. The rules, the reviewed dispositions and their frozen difference
lists are [harness/codecs/reader-rules.json](../../harness/codecs/reader-rules.json) and
[reader-differences.json](../../harness/codecs/reader-differences.json); gates:
[docs/quality.md](../../docs/quality.md#gates). On the 21 FATE Vorbis samples and 13
generated Opus, Vorbis, FLAC, multiplexed, chained, corrupted and truncated files:

- `compat::clock`: 31/34 pass. FFmpeg prints a negative last duration modulo 2^32
  (`b_vorbis.ogg`, `chain_vorbis.ogg`), and times the first packet after a page that
  fails its CRC from its own page where `compat::clock` times it from the stream start
  (`crc_bad.opus`).
- The reader's own timing: 7/34 pass; 25 differ only where `compat::clock` reproduces
  FFmpeg, and 2 by the negative last duration. The differences are FFmpeg's: it starts
  the first Vorbis packet as many samples before zero as it states alone, where this
  reader times the overlap it primes as no output; it starts a short Vorbis block after
  a long one late by the difference of their quarter blocks; and it times chained links'
  starts and packets after a lost page from the page rather than the granule position.

The `read_packets` helper reads every packet of a whole file in memory, headers
included, for tests and tools.
