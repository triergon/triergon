//! Preparation: one subtitle track's packets in demux order, read through the
//! Triergon container readers before any timed interval.

use std::sync::atomic::AtomicBool;
use triergon::{
    AnyReader, Budget, Bytes, CodecId, ContainerReader, Event, InputWindow, PacketInfo,
    SourceCapabilities, SourceId, Step, TimeBase, WorkBudget,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One demuxed packet, copied out of the reader.
#[derive(Clone, Debug)]
pub struct Packet {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
    pub data: Vec<u8>,
}

/// One selected track: its configuration, packet clock and packets.
#[derive(Clone, Debug)]
pub struct Track {
    pub configuration: Vec<u8>,
    pub time_base: TimeBase,
    pub packets: Vec<Packet>,
}

/// The `ordinal`th track (from 0) whose codec is `codec`, read from a Matroska or
/// ISO BMFF file through [`AnyReader`].
pub fn read_track(bytes: &[u8], codec: CodecId<'_>, ordinal: usize) -> Result<Track> {
    let pool = Budget::new(1 << 30)?;
    let capabilities = SourceCapabilities {
        length: Some(bytes.len() as u64),
        random_access: true,
    };
    let mut reader = AnyReader::open(
        &bytes[..bytes.len().min(64)],
        &pool,
        SourceId(1),
        capabilities,
    )?;
    let cancel = AtomicBool::new(false);
    let mut all = Vec::new();
    loop {
        match reader.step(&mut WorkBudget::new(1 << 20), &cancel)? {
            Step::Ready(Event::Packet(p)) => all.push((
                p.track(),
                p.time_base(),
                Packet {
                    pts: p.pts(),
                    dts: p.dts(),
                    duration: p.duration(),
                    data: p.payload().to_vec(),
                },
            )),
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(r) => {
                let at = usize::try_from(r.offset)?;
                if at >= bytes.len() {
                    reader.finish(r.source, r.token)?;
                } else {
                    let end = bytes.len().min(at + r.maximum);
                    let window = Bytes::copy_from_slice(&pool, &bytes[at..end])?;
                    reader.provide(&InputWindow::new(r.source, r.token, r.offset, window)?)?;
                }
            }
            Step::Backpressure(p) => return Err(format!("reader backpressure {p:?}").into()),
            Step::End(_) => break,
        }
    }
    let track = (0..reader.track_count())
        .filter_map(|i| reader.track(i))
        .filter(|t| t.codec() == codec)
        .nth(ordinal)
        .ok_or("no such track")?;
    let id = track.id();
    let configuration = track.configuration().to_vec();
    let mut time_base = None;
    let packets = all
        .into_iter()
        .filter(|(track, ..)| *track == id)
        .map(|(_, base, packet)| {
            time_base = Some(base);
            packet
        })
        .collect();
    Ok(Track {
        configuration,
        time_base: time_base.unwrap_or(TimeBase::NANOSECONDS),
        packets,
    })
}

/// `value` in `base` units as microseconds, rounded to nearest with ties away
/// from zero, as the reference's packet-time rescale does.
pub fn microseconds(value: i64, base: TimeBase) -> Option<i64> {
    let num = i128::from(value) * i128::from(base.numerator()) * 1_000_000;
    let den = i128::from(base.denominator());
    let half = den / 2;
    let q = if num >= 0 {
        (num + half) / den
    } else {
        (num - half) / den
    };
    i64::try_from(q).ok()
}

/// The `sub-input` lines for `track`, byte for byte as the reference prints them.
pub fn write_input(out: &mut impl std::io::Write, track: &Track) -> std::io::Result<()> {
    let hex = triergon_harness_protocol::hex;
    writeln!(
        out,
        "{{\"schema\":1,\"operation\":\"subtitle-input\",\"time_base\":[{},{}],\"configuration\":\"{}\",\"packets\":{}}}",
        track.time_base.numerator(),
        track.time_base.denominator(),
        hex(&track.configuration),
        track.packets.len()
    )?;
    let value = |v: Option<i64>| v.map_or("null".to_owned(), |v| v.to_string());
    for (i, p) in track.packets.iter().enumerate() {
        writeln!(
            out,
            "{{\"input\":{i},\"pts\":{},\"dts\":{},\"duration\":{},\"data\":\"{}\"}}",
            value(p.pts),
            value(p.dts),
            p.duration.unwrap_or(0),
            hex(&p.data)
        )?;
    }
    Ok(())
}
