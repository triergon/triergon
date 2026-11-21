//! `packets FORMAT FILE`: every packet a container reader emits, one JSON line each, for
//! the reader comparison of `cargo xtask codecs readers`.
//!
//! FORMAT names the reader as FFmpeg is told the demuxer: `ogg` opens
//! `triergon_ogg::BlockingReader`; `flac`, `mp3`, `aac`, `ac3` and `eac3` open the
//! `triergon_elementary` reader of that format; `mov` opens the neutral ISO BMFF reader
//! and prints what [`super::isobmff_packets`] describes. Each packet line holds exactly what the
//! reader reports: the track index, `pts`, `dts`, `duration` and `trim_start`/`trim_end`
//! in the packet's time base, the payload size, the source position (Ogg: the page that
//! completes the packet; elementary streams: the first byte) and whether the access unit
//! is complete; an Ogg packet that ends its page also gives the page's `granule`. Ogg
//! packets also carry `compat`, the reader's FFmpeg-compatible view
//! (`triergon_ogg::compat::clock::packet_timing`: `pts`, which is also the decode time,
//! `duration`, `skip_samples` and `discard_padding`), and the reader's `resync` and
//! `lost` events are lines of their own (`{"event": ...}`) where they occur. The last
//! line lists the tracks and how reading ended: `eof`, or `error` with the reader's
//! message (an error opening the file leaves no tracks). Reader errors are results, so
//! the process exits 0 whenever it could read the file.
//!
//! `seek ogg FILE TRACK SECONDS POLICY` opens the file the same way, seeks track TRACK
//! (its index once the reader is open) to SECONDS (a decimal number) with POLICY
//! (`exact`: `SeekPolicy::PreviousRandomAccess`; `preroll`: `SeekPolicy::PlaybackPreroll`),
//! prints the receipt as a `{"event":"seek",...}` line (its times as `[numerator,
//! denominator]` seconds) and then every event to the end of the file as `packets` does.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::AtomicBool;

use triergon::elementary::{self, Ac3, Adts, Flac, Format, MpegAudio};
use triergon::ogg::{self, compat::clock};
use triergon::{
    CodecId, Event, PacketInfo, SeekMethod, SeekPolicy, SeekReceipt, SeekRequest, Time, TimeBase,
    TrackInfo,
};

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
    /// The granule position of the Ogg page the packet ends, when it is the last packet
    /// completed there.
    granule: Option<i64>,
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
            granule: None,
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
        if let Some(granule) = self.granule {
            field(&mut line, "granule", &granule.to_string());
        }
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

pub(super) fn field(line: &mut String, name: &str, value: &str) {
    let _ = write!(line, ",\"{name}\":{value}");
}

pub(super) fn number(value: Option<i64>) -> String {
    value.map_or_else(|| "null".to_owned(), |v| v.to_string())
}

pub(super) fn time_base(base: TimeBase) -> String {
    format!("[{},{}]", base.numerator(), base.denominator())
}

/// A JSON string literal.
pub(super) fn string(text: &str) -> String {
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

pub(super) fn track_line(index: usize, track: &dyn TrackInfo, extra: &str) -> String {
    // An unnamed codec prints with its tag.
    let codec = match track.codec() {
        CodecId::Other => format!("Other({:?})", track.codec_tag()),
        codec => format!("{codec:?}"),
    };
    format!(
        "{{\"index\":{index},\"codec\":{},\"codec_tag\":{}{extra}}}",
        string(&codec),
        string(&String::from_utf8_lossy(track.codec_tag())),
    )
}

pub(super) fn finish(
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

/// A seek of `ogg_packets`: the track index, the target and the policy.
pub(crate) struct Seek {
    track: usize,
    target: Time,
    policy: SeekPolicy,
}

impl Seek {
    /// `TRACK SECONDS POLICY` as `seek` takes them.
    pub(crate) fn parse(track: &str, seconds: &str, policy: &str) -> Result<Seek, String> {
        let track = track.parse().map_err(|_| format!("track {track}"))?;
        let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
        let digits = format!("{whole}{fraction}");
        let numerator: i128 = digits.parse().map_err(|_| format!("seconds {seconds}"))?;
        let denominator = 10u128
            .checked_pow(u32::try_from(fraction.len()).map_err(|_| "seconds")?)
            .ok_or("seconds")?;
        let target = Time::new(numerator, denominator).map_err(|e| e.to_string())?;
        let policy = match policy {
            "exact" => SeekPolicy::PreviousRandomAccess,
            "preroll" => SeekPolicy::PlaybackPreroll,
            other => return Err(format!("policy {other}")),
        };
        Ok(Seek {
            track,
            target,
            policy,
        })
    }
}

fn time(value: Time) -> String {
    format!("[{},{}]", value.numerator(), value.denominator())
}

fn receipt_line(receipt: &SeekReceipt) -> String {
    let method = match receipt.method {
        SeekMethod::Index => "index",
        SeekMethod::BoundedScan => "bounded-scan",
        SeekMethod::Restart => "restart",
    };
    format!(
        "{{\"event\":\"seek\",\"requested\":{},\"landing\":{},\"discard_until\":{},\"method\":\"{method}\",\"scanned_bytes\":{}}}",
        time(receipt.requested),
        time(receipt.landing),
        time(receipt.discard_until),
        receipt.scanned_bytes
    )
}

fn ogg_packets(file: File, seek: Option<&Seek>, out: &mut impl Write) -> std::io::Result<()> {
    let cancel = AtomicBool::new(false);
    let mut reader = match ogg::BlockingReader::open(BufReader::new(file), ogg::Limits::default()) {
        Ok(reader) => reader,
        Err(error) => return finish(out, &[], Err(error)),
    };
    let sought = seek.map(|seek| {
        let track = reader
            .tracks()
            .get(seek.track)
            .map(TrackInfo::id)
            .ok_or(triergon::Error::Invalid("seek track"))?;
        let request = SeekRequest {
            track,
            target: seek.target,
            policy: seek.policy,
        };
        reader.seek(request, &cancel)
    });
    match sought {
        Some(Ok(receipt)) => writeln!(out, "{}", receipt_line(&receipt))?,
        Some(Err(error)) => return finish(out, &[], Err(error)),
        None => {}
    }
    let end = loop {
        match reader.next_event(&cancel) {
            Ok(Some(Event::Packet(packet))) => {
                let mut row = Row::of(&packet, packet.track_index(), packet.page_offset(), true);
                row.compat = Some(clock::packet_timing(&packet));
                row.granule = packet.granule();
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
    let cancel = AtomicBool::new(false);
    let mut reader = match elementary::BlockingReader::<F, _>::open(
        BufReader::new(file),
        elementary::Limits::default(),
    ) {
        Ok(reader) => reader,
        Err(error) => return finish(out, &[], Err(error)),
    };
    let end = loop {
        match reader.next_packet(&cancel) {
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

/// Print the packets of `path` read as `format`, after the seek `seek` names (only
/// `mov` seeks this way; Ogg seeks through [`seek`]); `Err` for an unknown format, an
/// unreadable file or a bad seek.
pub(crate) fn run(format: &str, path: &str, seek: Option<&str>) -> Result<(), String> {
    if let Some(seek) = seek
        && format != "mov"
    {
        return Err(format!("{format} does not seek ({seek})"));
    }
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut out = BufWriter::new(std::io::stdout().lock());
    let written = match format {
        "mov" => {
            let seek = seek.map(super::isobmff_packets::Seek::parse).transpose()?;
            super::isobmff_packets::packets(file, seek, &mut out)
        }
        "ogg" => ogg_packets(file, None, &mut out),
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

/// Print the receipt of `seek` in the Ogg file `path` and every event after it;
/// `Err` for another format or an unreadable file.
pub(crate) fn seek(format: &str, path: &str, seek: &Seek) -> Result<(), String> {
    if format != "ogg" {
        return Err(format!("seek: no seeking reader for {format}"));
    }
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let mut out = BufWriter::new(std::io::stdout().lock());
    ogg_packets(file, Some(seek), &mut out)
        .and_then(|()| out.flush())
        .map_err(|e| format!("stdout: {e}"))
}
