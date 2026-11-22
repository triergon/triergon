use std::io::{self, BufWriter, Cursor, Write};
use std::sync::atomic::AtomicBool;

use triergon::elementary::{self, Ac3, Adts, Flac, Format, MpegAudio};
use triergon::ogg;
use triergon::{
    Budget, Bytes, ContainerReader, Error, Event, InputRequest, InputWindow, PacketInfo,
    SeekMethod, SeekPolicy, SeekRequest, SourceCapabilities, SourceId, Step, Time, WorkBudget,
};

use crate::packets::{field, finish, number, time_base, track_line};

/// Emit the established packet row format without changing the measured worker.
fn row(
    packet: &impl PacketInfo,
    track: usize,
    position: u64,
    complete: bool,
    extra: &str,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut line = format!("{{\"track\":{track}");
    field(&mut line, "time_base", &time_base(packet.time_base()));
    let trim = packet.trim();
    for (key, value) in [
        ("pts", packet.pts()),
        ("dts", packet.dts()),
        ("duration", packet.duration()),
        ("trim_start", trim.start),
        ("trim_end", trim.end),
    ] {
        field(&mut line, key, &number(value));
    }
    field(&mut line, "size", &packet.payload().len().to_string());
    field(&mut line, "position", &position.to_string());
    field(&mut line, "complete", &complete.to_string());
    writeln!(out, "{line}{extra}}}")
}

fn ogg_event(event: &Event<ogg::Packet, ogg::ReaderEvent>, out: &mut impl Write) -> io::Result<()> {
    use std::fmt::Write as _;

    match event {
        Event::Packet(packet) => {
            let mut extra = String::new();
            if let Some(granule) = packet.granule() {
                field(&mut extra, "granule", &granule.to_string());
            }
            let compat = ogg::compat::clock::packet_timing(packet);
            let _ = write!(
                extra,
                ",\"compat\":{{\"pts\":{},\"duration\":{},\"skip_samples\":{},\"discard_padding\":{}}}",
                number(compat.pts),
                number(compat.duration),
                compat.skip_samples,
                compat.discard_padding,
            );
            row(
                packet,
                packet.track_index(),
                packet.page_offset(),
                true,
                &extra,
                out,
            )
        }
        Event::Other(ogg::ReaderEvent::Resync { offset, bytes }) => writeln!(
            out,
            "{{\"event\":\"resync\",\"offset\":{offset},\"bytes\":{bytes}}}"
        ),
        Event::Other(ogg::ReaderEvent::Lost { track, page }) => writeln!(
            out,
            "{{\"event\":\"lost\",\"track\":{track},\"page\":{page}}}"
        ),
        _ => Ok(()),
    }
}

fn ogg_tracks(tracks: &[ogg::Track]) -> Vec<String> {
    tracks
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
        .collect()
}

fn elementary_event(
    event: &Event<elementary::Packet, elementary::ReaderEvent>,
    out: &mut impl Write,
) -> io::Result<()> {
    if let Event::Packet(packet) = event {
        row(packet, 0, packet.offset(), packet.is_complete(), "", out)?;
    }
    Ok(())
}

fn elementary_tracks<F: Format>(reader: &elementary::Reader<F>) -> Vec<String> {
    reader
        .stream()
        .map(|track| {
            let extra = format!(",\"time_base\":{}", time_base(track.time_base()));
            track_line(0, track, &extra)
        })
        .into_iter()
        .collect()
}

pub(super) fn stream(input: &[u8]) -> io::Result<()> {
    // Slice implements Read without implementing Seek.
    let mut out = BufWriter::new(io::stdout().lock());
    let mut reader = match ogg::BlockingReader::open_stream(input, ogg::Limits::default()) {
        Ok(reader) => reader,
        Err(error) => return finish(&mut out, &[], Err(error)),
    };
    let cancel = AtomicBool::new(false);
    let end = loop {
        match reader.next_event(&cancel) {
            Ok(Some(event)) => ogg_event(&event, &mut out)?,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    finish(&mut out, &ogg_tracks(reader.tracks()), end)?;
    out.flush()
}

#[derive(Default)]
struct Drive {
    windows: u64,
    yields: u64,
    backpressure: u64,
    packets: u64,
}

fn same_request(reported: &InputRequest, requested: &InputRequest) -> bool {
    reported.source == requested.source
        && reported.token == requested.token
        && reported.offset == requested.offset
        && reported.maximum == requested.maximum
}

fn supply(
    reader: &mut impl ContainerReader,
    pool: &Budget,
    input: &[u8],
    window: usize,
    request: InputRequest,
) -> triergon::Result<()> {
    let at = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
    match input.get(at..).filter(|rest| !rest.is_empty()) {
        Some(rest) => {
            let bytes =
                Bytes::copy_from_slice(pool, &rest[..rest.len().min(request.maximum).min(window)])?;
            reader.provide(&InputWindow::new(
                request.source,
                request.token,
                request.offset,
                bytes,
            )?)
        }
        None => reader.finish(request.source, request.token),
    }
}

struct PushSource<'a> {
    pool: &'a Budget,
    input: &'a [u8],
    window: usize,
    retain: bool,
}

fn drive<R: ContainerReader>(
    reader: &mut R,
    request: impl Fn(&R) -> InputRequest,
    source: PushSource<'_>,
    counts: &mut Drive,
    mut emit: impl FnMut(&Event<R::Packet, R::Other>) -> io::Result<()>,
) -> io::Result<triergon::Result<()>> {
    let cancel = AtomicBool::new(false);
    let mut held = Vec::new();
    loop {
        let step = reader.step(&mut WorkBudget::new(256), &cancel);
        let result = match step {
            Ok(Step::Ready(event)) => {
                emit(&event)?;
                if let Event::Packet(packet) = event {
                    counts.packets += 1;
                    if source.retain {
                        held.push(packet);
                    }
                }
                continue;
            }
            Ok(Step::NeedInput(need)) => {
                let wanted = request(reader);
                if !same_request(&wanted, &need) {
                    return Err(io::Error::other("request() differs from NeedInput"));
                }
                match supply(reader, source.pool, source.input, source.window, wanted) {
                    Ok(()) => {
                        counts.windows += 1;
                        continue;
                    }
                    Err(error) => error,
                }
            }
            Ok(Step::Yield) => {
                counts.yields += 1;
                continue;
            }
            Ok(Step::End(_)) => return Ok(Ok(())),
            Ok(Step::Backpressure(pressure)) => {
                if !held.is_empty() {
                    held.clear();
                    counts.backpressure += 1;
                    continue;
                }
                Error::Backpressure(pressure)
            }
            Err(error) => error,
        };
        // Also release held packets if allocating the next input window
        // needs its storage. Only a reader's Step counts as a resume.
        if matches!(result, Error::Backpressure(_)) && !held.is_empty() {
            held.clear();
            continue;
        }
        return Ok(Err(result));
    }
}

fn ogg_push(
    input: &[u8],
    window: usize,
    pool: &Budget,
    caps: SourceCapabilities,
    small: bool,
    counts: &mut Drive,
    out: &mut Vec<u8>,
) -> io::Result<triergon::Result<()>> {
    let mut reader = match ogg::Reader::new(pool, SourceId(1), caps, ogg::Limits::default(), 65536)
    {
        Ok(reader) => reader,
        Err(error) => return Ok(Err(error)),
    };
    let end = drive(
        &mut reader,
        ogg::Reader::request,
        PushSource {
            pool,
            input,
            window,
            retain: small,
        },
        counts,
        |event| ogg_event(event, out),
    )?;
    if end.is_err() && counts.packets == 0 {
        // Opening the blocking reader buffers events until the first packet.
        out.clear();
        finish(out, &[], end)?;
    } else {
        finish(out, &ogg_tracks(reader.tracks()), end)?;
    }
    Ok(end)
}

fn elementary_push<F: Format>(
    input: &[u8],
    window: usize,
    pool: &Budget,
    caps: SourceCapabilities,
    small: bool,
    counts: &mut Drive,
    out: &mut Vec<u8>,
) -> io::Result<triergon::Result<()>> {
    let mut reader = match elementary::Reader::<F>::new(
        pool,
        SourceId(1),
        caps,
        elementary::Limits::default(),
        65536,
    ) {
        Ok(reader) => reader,
        Err(error) => return Ok(Err(error)),
    };
    let end = drive(
        &mut reader,
        elementary::Reader::request,
        PushSource {
            pool,
            input,
            window,
            retain: small,
        },
        counts,
        |event| elementary_event(event, out),
    )?;
    finish(out, &elementary_tracks(&reader), end)?;
    Ok(end)
}

pub(super) fn push(format: &str, input: &[u8], window: usize, small: bool) -> io::Result<()> {
    let maximum = if format == "ogg" {
        ogg::Limits::default().live_storage_bytes
    } else {
        elementary::Limits::default().live_storage_bytes
    };
    let caps = SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: true,
    };
    let mut limit = if small { 4096 } else { maximum };
    loop {
        let pool = Budget::new(limit).map_err(io::Error::other)?;
        let mut counts = Drive::default();
        let mut rows = Vec::new();
        let run = match format {
            "ogg" => ogg_push,
            "flac" => elementary_push::<Flac>,
            "mp3" => elementary_push::<MpegAudio>,
            "aac" => elementary_push::<Adts>,
            "ac3" | "eac3" => elementary_push::<Ac3>,
            _ => return Err(io::Error::other("unknown reader format")),
        };
        let end = run(input, window, &pool, caps, small, &mut counts, &mut rows)?;
        if matches!(end, Err(Error::Limit { .. } | Error::Backpressure(_))) && limit < maximum {
            limit = (limit * 2).min(maximum);
            continue;
        }
        if rows.is_empty() {
            finish(&mut rows, &[], end)?;
        }
        let mut out = BufWriter::new(io::stdout().lock());
        writeln!(
            out,
            "{{\"pool\":{limit},\"window\":{window},\"windows\":{},\"yields\":{},\"backpressure_resumes\":{}}}",
            counts.windows, counts.yields, counts.backpressure
        )?;
        out.write_all(&rows)?;
        return out.flush();
    }
}

pub(super) fn seek(
    format: &str,
    input: &[u8],
    track: &str,
    seconds: &str,
    policy: &str,
) -> Result<(), String> {
    if track != "0" {
        return Err("the elementary stream has only track 0".into());
    }
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let numerator = format!("{whole}{fraction}")
        .parse()
        .map_err(|_| "bad seek time")?;
    let denominator = 10u128
        .checked_pow(fraction.len() as u32)
        .ok_or("bad seek time scale")?;
    let target = Time::new(numerator, denominator).map_err(|error| error.to_string())?;
    let policy = match policy {
        "exact" => SeekPolicy::PreviousRandomAccess,
        "preroll" => SeekPolicy::PlaybackPreroll,
        _ => return Err("unknown seek policy".into()),
    };
    let request = SeekRequest {
        track: elementary::TRACK,
        target,
        policy,
    };
    let run = match format {
        "flac" => elementary_seek::<Flac>,
        "mp3" => elementary_seek::<MpegAudio>,
        "aac" => elementary_seek::<Adts>,
        "ac3" | "eac3" => elementary_seek::<Ac3>,
        _ => return Err("unknown seek format".into()),
    };
    run(input, request).map_err(|error| error.to_string())
}

fn elementary_seek<F: Format>(input: &[u8], request: SeekRequest) -> io::Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    let cancel = AtomicBool::new(false);
    let mut reader = match elementary::BlockingReader::<F, _>::open(
        Cursor::new(input),
        elementary::Limits::default(),
    ) {
        Ok(reader) => reader,
        Err(error) => return finish(&mut out, &[], Err(error)),
    };
    let end = match reader.seek(request, &cancel) {
        Ok(receipt) => {
            let detail = reader
                .reader()
                .last_seek()
                .ok_or_else(|| io::Error::other("missing seek detail"))?;
            if detail.receipt != receipt {
                return Err(io::Error::other("seek detail has a different receipt"));
            }
            let method = match receipt.method {
                SeekMethod::Index => "index",
                SeekMethod::BoundedScan => "bounded-scan",
                SeekMethod::Restart => "restart",
            };
            let time = |t: Time| format!("[{},{}]", t.numerator(), t.denominator());
            writeln!(
                out,
                "{{\"event\":\"seek\",\"requested\":{},\"landing\":{},\"discard_until\":{},\"method\":\"{method}\",\"scanned_bytes\":{},\"basis\":\"{:?}\",\"probes\":{},\"preroll_frames\":{}}}",
                time(receipt.requested),
                time(receipt.landing),
                time(receipt.discard_until),
                receipt.scanned_bytes,
                detail.basis,
                detail.probes,
                F::PREROLL,
            )?;
            loop {
                match reader.next_event(&cancel) {
                    Ok(Some(event)) => elementary_event(&event, &mut out)?,
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                }
            }
        }
        Err(error) => Err(error),
    };
    finish(&mut out, &elementary_tracks(reader.reader()), end)?;
    out.flush()
}
