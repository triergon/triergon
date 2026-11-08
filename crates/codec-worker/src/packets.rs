//! `packets FORMAT FILE`: every packet a container reader emits, one JSON line each, for
//! the reader comparison of `cargo xtask codecs readers`.
//!
//! FORMAT names the reader as FFmpeg is told the demuxer: `ogg` opens
//! `triergon_ogg::BlockingReader`; `flac`, `mp3`, `aac`, `ac3` and `eac3` open the
//! `triergon_elementary` reader of that format. Each packet line holds exactly what the
//! reader reports: the track index, `pts`, `dts`, `duration` and `trim_start`/`trim_end`
//! in the packet's time base, the payload size, the source position (Ogg: the page that
//! completes the packet; elementary streams: the first byte) and whether the access unit
//! is complete. Ogg packets also carry `compat`, the reader's FFmpeg-compatible view
//! (`triergon_ogg::compat::clock::packet_timing`: `pts`, which is also the decode time,
//! `duration`, `skip_samples` and `discard_padding`), and the reader's `resync` and
//! `lost` events are lines of their own (`{"event": ...}`) where they occur. The last
//! line lists the tracks and how reading ended: `eof`, or `error` with the reader's
//! message (an error opening the file leaves no tracks). Reader errors are results, so
//! the process exits 0 whenever it could read the file.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::AtomicBool;

use triergon::elementary::{self, Ac3, Adts, Flac, Format, MpegAudio};
use triergon::ogg::{self, compat::clock};
use triergon::{Event, PacketInfo, TimeBase, TrackInfo};

/// One packet as the reader reports it.
struct Row {
    track: usize,
    time_base: TimeBase,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    trim: triergon::Trim,
    size: usize,
    position: u64,
    complete: bool,
    compat: Option<clock::ReferenceTiming>,
}

impl Row {
    fn of(packet: &impl PacketInfo, track: usize, position: u64, complete: bool) -> Row {
        Row {
            track,
            time_base: packet.time_base(),
            pts: packet.pts(),
            dts: packet.dts(),
            duration: packet.duration(),
            trim: packet.trim(),
            size: packet.payload().len(),
            position,
            complete,
            compat: None,
        }
    }

    fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut line = format!("{{\"track\":{}", self.track);
        field(&mut line, "time_base", &time_base(self.time_base));
        for (name, value) in [
            ("pts", self.pts),
            ("dts", self.dts),
            ("duration", self.duration),
            ("trim_start", self.trim.start),
            ("trim_end", self.trim.end),
        ] {
            field(&mut line, name, &number(value));
        }
        field(&mut line, "size", &self.size.to_string());
        field(&mut line, "position", &self.position.to_string());
        field(&mut line, "complete", &self.complete.to_string());
        if let Some(compat) = self.compat {
            let _ = write!(
                line,
                ",\"compat\":{{\"pts\":{},\"duration\":{},\"skip_samples\":{},\"discard_padding\":{}}}",
                number(compat.pts),
                number(compat.duration),
                compat.skip_samples,
                compat.discard_padding
            );
        }
        line.push('}');
        writeln!(out, "{line}")
    }
}

fn field(line: &mut String, name: &str, value: &str) {
    let _ = write!(line, ",\"{name}\":{value}");
}

fn number(value: Option<i64>) -> String {
    value.map_or_else(|| "null".to_owned(), |v| v.to_string())
}

fn time_base(base: TimeBase) -> String {
    format!("[{},{}]", base.numerator(), base.denominator())
}

/// A JSON string literal.
fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn track_line(index: usize, track: &dyn TrackInfo, extra: &str) -> String {
    format!(
        "{{\"index\":{index},\"codec\":{},\"codec_tag\":{}{extra}}}",
        string(&format!("{:?}", track.codec())),
        string(&String::from_utf8_lossy(track.codec_tag())),
    )
}

fn finish(
    out: &mut impl Write,
    tracks: &[String],
    end: Result<(), triergon::Error>,
) -> std::io::Result<()> {
    let end = match end {
        Ok(()) => "\"end\":\"eof\"".to_owned(),
        Err(error) => format!("\"end\":\"error\",\"error\":{}", string(&error.to_string())),
    };
    writeln!(out, "{{\"tracks\":[{}],{end}}}", tracks.join(","))
}

fn ogg_packets(file: File, out: &mut impl Write) -> std::io::Result<()> {
    let cancel = AtomicBool::new(false);
    let mut reader = match ogg::BlockingReader::open(BufReader::new(file), ogg::Limits::default()) {
        Ok(reader) => reader,
        Err(error) => return finish(out, &[], Err(error)),
    };
    let end = loop {
        match reader.next_event(&cancel) {
            Ok(Some(Event::Packet(packet))) => {
                let mut row = Row::of(&packet, packet.track_index(), packet.page_offset(), true);
                row.compat = Some(clock::packet_timing(&packet));
                row.write(out)?;
            }
            Ok(Some(Event::Other(ogg::ReaderEvent::Resync { offset, bytes }))) => writeln!(
                out,
                "{{\"event\":\"resync\",\"offset\":{offset},\"bytes\":{bytes}}}"
            )?,
            Ok(Some(Event::Other(ogg::ReaderEvent::Lost { track, page }))) => writeln!(
                out,
                "{{\"event\":\"lost\",\"track\":{track},\"page\":{page}}}"
            )?,
            Ok(Some(_)) => {}
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let tracks: Vec<String> = reader
        .tracks()
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let extra = format!(
                ",\"link\":{},\"serial\":{},\"time_base\":{}",
                track.link(),
                track.serial(),
                time_base(track.time_base())
            );
            track_line(index, track, &extra)
        })
        .collect();
    finish(out, &tracks, end)
}

fn elementary_packets<F: Format>(file: File, out: &mut impl Write) -> std::io::Result<()> {
    let mut reader = match elementary::BlockingReader::<F, _>::open(
        BufReader::new(file),
        elementary::Limits::default(),
    ) {
        Ok(reader) => reader,
        Err(error) => return finish(out, &[], Err(error)),
    };
    let end = loop {
        match reader.next_packet() {
            Ok(Some(packet)) => {
                Row::of(&packet, 0, packet.offset(), packet.is_complete()).write(out)?;
            }
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let tracks: Vec<String> = reader
        .reader()
        .stream()
        .map(|track| {
            let extra = format!(",\"time_base\":{}", time_base(track.time_base()));
            track_line(0, track, &extra)
        })
        .into_iter()
        .collect();
    finish(out, &tracks, end)
}

/// Print the packets of `path` read as `format`; `Err` for an unknown format or an
/// unreadable file.
pub fn run(format: &str, path: &str) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut out = BufWriter::new(std::io::stdout().lock());
    let written = match format {
        "ogg" => ogg_packets(file, &mut out),
        "flac" => elementary_packets::<Flac>(file, &mut out),
        "mp3" => elementary_packets::<MpegAudio>(file, &mut out),
        "aac" => elementary_packets::<Adts>(file, &mut out),
        "ac3" | "eac3" => elementary_packets::<Ac3>(file, &mut out),
        _ => return Err(format!("unknown format {format}")),
    };
    written
        .and_then(|()| out.flush())
        .map_err(|e| format!("stdout: {e}"))
}
