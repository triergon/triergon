//! `packets mov FILE [TRACK_ID@MICROSECONDS]`: every packet the neutral ISO BMFF
//! reader (`triergon_isobmff::Reader`) emits, for `cargo xtask codecs isobmff-reader`.
//!
//! Each packet line holds what the reader reports: the track index and `track_id`,
//! `time_base`, `pts`, `dts` and `duration` in that base, the payload `size`, the
//! sample's source `position`, `sync` (1 for a stated sync sample, 0 for a stated
//! non-sync sample, `null` where the source states nothing), the payload's MD5 as
//! `hash`, the sample `ordinal` and its sample entry (`description`). With a seek, the
//! reader is opened, `request_seek` asks for the track's time `MICROSECONDS` / 10^6
//! seconds with `SeekPolicy::PreviousRandomAccess`, and the receipt is a line of its own
//! (`{"event":"seek", ...}`, its times as `[numerator, denominator]` seconds) before the
//! packets that follow it. The last line lists the tracks, each with its edit list as
//! `[segment_duration, media_time, rate_integer, rate_fraction]` rows (`null` without
//! one) and the movie timescale, and how reading ended: `eof`, or `error` with the
//! reader's message. Reader errors are results, so the process exits 0 whenever it
//! could read the file.

use std::fs::File;
use std::io::{BufReader, Write};

use triergon::{
    Blocking, Budget, Event, PacketInfo, RandomAccess, SeekPolicy, SeekRequest, SourceCapabilities,
    SourceId, Time, TimeBase, TrackId, TrackInfo,
};
use triergon_harness_protocol::md5;
use triergon_isobmff::{Reader, ReaderEvent, ReaderLimits, Track};

use super::packets::{field, number, string, time_base};

/// The reader's memory pool: every input window and retained box.
const POOL: usize = 1 << 30;
/// Bytes per input window.
const WINDOW: usize = 1 << 16;

/// A seek: the track by its `track_ID` and the target in microseconds.
#[derive(Clone, Copy)]
pub(crate) struct Seek {
    track: u64,
    micros: i64,
}

impl Seek {
    /// `TRACK_ID@MICROSECONDS`.
    pub(crate) fn parse(text: &str) -> Result<Seek, String> {
        let (track, micros) = text
            .split_once('@')
            .ok_or_else(|| format!("seek {text}: expected TRACK_ID@MICROSECONDS"))?;
        Ok(Seek {
            track: track.parse().map_err(|_| format!("seek track {track}"))?,
            micros: micros.parse().map_err(|_| format!("seek time {micros}"))?,
        })
    }
}

fn time(value: Time) -> String {
    format!("[{},{}]", value.numerator(), value.denominator())
}

fn packet_line(reader: &Reader, packet: &triergon_isobmff::Packet) -> String {
    let id = packet.track();
    let index = reader
        .tracks()
        .iter()
        .position(|track| track.id() == id)
        .map_or_else(|| "null".to_owned(), |at| at.to_string());
    let sample = packet.sample();
    let mut line = format!("{{\"track\":{index}");
    field(&mut line, "track_id", &id.0.to_string());
    field(&mut line, "time_base", &time_base(packet.time_base()));
    for (name, value) in [
        ("pts", packet.pts()),
        ("dts", packet.dts()),
        ("duration", packet.duration()),
    ] {
        field(&mut line, name, &number(value));
    }
    field(&mut line, "size", &packet.payload().len().to_string());
    field(&mut line, "position", &sample.range.start.to_string());
    let sync = match packet.random_access() {
        RandomAccess::Sync => "1",
        RandomAccess::NotSync => "0",
        RandomAccess::Unknown => "null",
    };
    field(&mut line, "sync", sync);
    field(
        &mut line,
        "hash",
        &string(&md5::hex_digest(packet.payload())),
    );
    field(&mut line, "ordinal", &sample.ordinal.to_string());
    field(
        &mut line,
        "description",
        &sample.description_index.to_string(),
    );
    line.push('}');
    line
}

fn track_line(index: usize, track: &Track, movie_timescale: Option<u32>) -> String {
    let mut extra = format!(
        ",\"id\":{},\"kind\":{},\"time_base\":{}",
        track.id().0,
        string(&format!("{:?}", track.kind())),
        time_base(track.header().time_base().unwrap_or(TimeBase::MICROSECONDS))
    );
    let edits = track.edits().map(|list| {
        let rows: Vec<String> = (0..list.len())
            .filter_map(|at| list.entry(at).ok())
            .map(|entry| {
                format!(
                    "[{},{},{},{}]",
                    entry.segment_duration,
                    entry.media_time,
                    entry.rate_integer,
                    entry.rate_fraction
                )
            })
            .collect();
        format!("[{}]", rows.join(","))
    });
    field(&mut extra, "edits", edits.as_deref().unwrap_or("null"));
    field(
        &mut extra,
        "movie_timescale",
        &movie_timescale.map_or_else(|| "null".to_owned(), |t| t.to_string()),
    );
    super::packets::track_line(index, track, &extra)
}

/// Read every event, seeking once the movie is open when `seek` asks to; `Err` ends
/// reading with the reader's error.
fn read(
    blocking: &mut Blocking<Reader, BufReader<File>>,
    seek: Option<Seek>,
    out: &mut impl Write,
) -> std::io::Result<Result<(), triergon::Error>> {
    let mut pending = seek;
    loop {
        let event = match blocking.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => return Ok(Ok(())),
            Err(error) => return Ok(Err(error)),
        };
        match event {
            Event::Packet(packet) => writeln!(out, "{}", packet_line(blocking.reader(), &packet))?,
            Event::Seek(receipt) => writeln!(
                out,
                "{{\"event\":\"seek\",\"requested\":{},\"landing\":{},\"discard_until\":{},\"method\":{},\"replays_from_start\":{}}}",
                time(receipt.requested),
                time(receipt.landing),
                time(receipt.discard_until),
                string(&format!("{:?}", receipt.method)),
                receipt.replays_from_start
            )?,
            Event::Other(ReaderEvent::Movie) => {
                if let Some(seek) = pending.take() {
                    let request =
                        Time::new(i128::from(seek.micros), 1_000_000).map(|target| SeekRequest {
                            track: TrackId(seek.track),
                            target,
                            policy: SeekPolicy::PreviousRandomAccess,
                        });
                    if let Err(error) = request.and_then(|request| blocking.seek(request)) {
                        return Ok(Err(error));
                    }
                }
            }
            Event::Other(ReaderEvent::Box(_)) => {}
        }
    }
}

/// Print the packets of `path`, seeking first when `seek` is given.
pub(crate) fn packets(file: File, seek: Option<Seek>, out: &mut impl Write) -> std::io::Result<()> {
    let opened = Budget::new(POOL).and_then(|pool| {
        let capabilities = SourceCapabilities {
            length: Some(file.metadata()?.len()),
            random_access: true,
        };
        let reader = Reader::new(
            &pool,
            SourceId(1),
            capabilities,
            ReaderLimits::default(),
            WINDOW,
        )?;
        Ok(Blocking::seekable(reader, BufReader::new(file), &pool))
    });
    let mut blocking = match opened {
        Ok(blocking) => blocking,
        Err(error) => return super::packets::finish(out, &[], Err(error)),
    };
    let end = read(&mut blocking, seek, out)?;
    let reader = blocking.reader();
    let timescale = reader.movie_header().map(|header| header.timescale);
    let tracks: Vec<String> = reader
        .tracks()
        .iter()
        .enumerate()
        .map(|(index, track)| track_line(index, track, timescale))
        .collect();
    super::packets::finish(out, &tracks, end)
}
