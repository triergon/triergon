//! Compare clocked session publication with the coded input and unclocked canvas.

use std::{collections::BTreeMap, ffi::OsString, io::Write, sync::Arc};

use triergon::framing::nal::Codec;
use triergon::{Budget, Error};
use triergon_harness_protocol::digest::signed;
use triergon_session::{
    Canvas, FeedError, OnRefusal, QueueLimits, SessionState, VideoCodec, VideoSession,
    video_decoder, video_decoder_in,
};

use triergon_harness_runner::{Operation, Totals};

const START: i64 = -4_000_000;
const PERIOD: i64 = 3_000_000;
const RESTART: i64 = 100_000_000;

fn data<'a>(
    input: &'a [u8],
    codec: Codec,
    positions: &BTreeMap<u64, i64>,
) -> Result<Vec<(i64, &'a [u8])>, String> {
    let mut data = Vec::new();
    if let Some(error) = super::units(input, codec, |unit| {
        let start = usize::try_from(unit.range.start).map_err(|e| e.to_string())?;
        let end = usize::try_from(unit.range.end).map_err(|e| e.to_string())?;
        let pts = *positions
            .get(&unit.range.start)
            .ok_or_else(|| format!("unmapped access unit at {start}"))?;
        data.push((
            pts,
            input.get(start..end).ok_or("access unit exceeds source")?,
        ));
        Ok(())
    })? {
        return Err(error.to_string());
    }
    if data.len() != positions.len() {
        return Err("picture positions do not describe the entire source".into());
    }
    Ok(data)
}

fn enqueue(
    session: &mut VideoSession,
    units: &[(i64, &[u8])],
    reset: i64,
    bounded: bool,
) -> Result<(), String> {
    for (pts, data) in units {
        session
            .feed(data, Some(pts + reset))
            .map_err(|e| e.to_string())?;
    }
    if bounded && session.feed(units[0].1, Some(units[0].0)) != Err(FeedError::Backpressure) {
        return Err("a full video queue accepted another access unit".into());
    }
    session.end_of_stream();
    if session.feed(units[0].1, Some(units[0].0)) != Err(FeedError::Invalid) {
        return Err("new input entered after end of stream".into());
    }
    Ok(())
}

fn record(
    work: &mut Totals,
    trace: &mut Option<&mut dyn Write>,
    fields: &[u64],
) -> Result<(), String> {
    work.record(fields[0], &fields[1..], &[]);
    if let Some(out) = trace {
        let row = fields
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(out, "{row}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn produce(
    input: &[u8],
    coding: (Codec, VideoCodec),
    canvas: Canvas,
    positions: &BTreeMap<u64, i64>,
    work: &mut Totals,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let (codec, video) = coding;
    let units = data(input, codec, positions)?;
    let mut plain = VideoSession::with_canvas(QueueLimits::VIDEO, canvas);
    plain.load(Some(video_decoder(video)));
    enqueue(&mut plain, &units, 0, false)?;
    let mut canvases = Vec::new();
    while plain.step(None) {
        let picture = plain
            .latest()
            .ok_or("unclocked session did not publish a picture")?;
        if picture.rgba.len() != canvas.bytes() {
            return Err("unclocked session published the wrong canvas".into());
        }
        canvases.push((picture.pts, picture.rgba.clone()));
    }
    if plain.state() != SessionState::Ended || canvases.len() != units.len() {
        return Err("unclocked session did not publish all pictures".into());
    }

    let pool = Budget::new(64 << 20).map_err(|e| e.to_string())?;
    let limits = QueueLimits {
        units: units.len(),
        ..QueueLimits::VIDEO
    };
    let mut clocked = VideoSession::with_canvas(limits, canvas);
    if clocked.state() != SessionState::Idle || clocked.feed(&[0], None) != Err(FeedError::Idle) {
        return Err("an unloaded lane accepted a packet".into());
    }
    clocked.load(Some(video_decoder_in(video, &pool)));
    enqueue(&mut clocked, &units, 0, true)?;

    for (index, (expected_pts, baseline)) in canvases.iter().enumerate() {
        let pts = expected_pts.ok_or("unclocked decoder lost an input timestamp")?;
        let prior = clocked.latest().map(|picture| picture.sequence);
        if clocked.step(Some(pts - 1))
            || clocked.latest().map(|picture| picture.sequence) != prior
            || clocked.state() != SessionState::Running
            || !clocked.step(Some(pts))
        {
            return Err(format!(
                "picture {index} did not respect its publication clock"
            ));
        }
        let picture = clocked.latest().ok_or("no released picture")?;
        if picture.rgba.len() != clocked.canvas().bytes()
            || picture.rgba.as_ref() != baseline.as_ref()
        {
            return Err(format!("clocked picture {index} changed its canvas"));
        }
        record(
            work,
            &mut trace,
            &[
                1,
                picture.sequence,
                picture.generation,
                signed(picture.pts.ok_or("clocked picture PTS absent")?),
                clocked.canvas().width() as u64,
                clocked.canvas().height() as u64,
            ],
        )?;
    }
    if clocked.step(None)
        || clocked.state() != SessionState::Ended
        || clocked.refusals() != 0
        || clocked.last_refusal().is_some()
    {
        return Err("the clocked lane did not finish cleanly".into());
    }

    let old = clocked
        .latest()
        .ok_or("latest picture missing at EOF")?
        .clone();
    clocked.flush();
    if clocked.state() != SessionState::Starved
        || !clocked
            .latest()
            .is_some_and(|picture| Arc::ptr_eq(&old.rgba, &picture.rgba))
    {
        return Err("flush discarded the on-screen picture".into());
    }
    enqueue(&mut clocked, &units, RESTART, true)?;
    let next = START + RESTART;
    if clocked.step(Some(next - 1)) || !clocked.step(Some(next)) {
        return Err("the restarted lane ignored the new clock".into());
    }
    let picture = clocked
        .latest()
        .ok_or("restart did not publish a picture")?;
    if picture.rgba.as_ref() != canvases[0].1.as_ref() {
        return Err("the restarted lane changed the first picture".into());
    }
    record(
        work,
        &mut trace,
        &[
            2,
            picture.sequence,
            picture.generation,
            signed(picture.pts.ok_or("restart picture PTS absent")?),
        ],
    )?;
    clocked.load(None);
    if clocked.state() != SessionState::Idle
        || clocked.latest().is_some()
        || clocked.feed(units[0].1, Some(next)) != Err(FeedError::Idle)
    {
        return Err("stop kept the previous timeline or picture".into());
    }
    record(work, &mut trace, &[3, clocked.generation()])?;
    refusal(
        video,
        codec,
        &units,
        canvas,
        canvases[0].1.as_ref(),
        work,
        &mut trace,
    )
}

fn refusal(
    video: VideoCodec,
    codec: Codec,
    units: &[(i64, &[u8])],
    canvas: Canvas,
    first_rgba: &[u8],
    work: &mut Totals,
    trace: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    let invalid = match codec {
        Codec::Avc => &[0, 0, 1, 0xe5, 0x80][..],
        Codec::Hevc => &[0, 0, 1, 0x26, 1, 0xa0][..],
    };
    let mut failed = VideoSession::new(QueueLimits::VIDEO);
    failed.load(Some(video_decoder(video)));
    if failed.canvas() != Canvas::default() || failed.feed(&[], None) != Err(FeedError::Invalid) {
        return Err("invalid default canvas or empty access-unit admission".into());
    }
    failed.feed(invalid, None).map_err(|e| e.to_string())?;
    if failed.step(None) || failed.refusals() != 1 || failed.latest().is_some() {
        return Err("a malformed NAL entered the visible timeline".into());
    }
    let SessionState::Failed(error @ Error::Invalid(_)) = failed.state() else {
        return Err(format!("malformed NAL returned {:?}", failed.state()));
    };
    if failed.last_refusal() != Some(error) {
        return Err("fail policy lost the typed decoder error".into());
    }
    record(work, trace, &[4, failed.refusals()])?;

    let mut skipping =
        VideoSession::with_canvas(QueueLimits::VIDEO, canvas).on_refusal(OnRefusal::Skip);
    skipping.load(Some(video_decoder(video)));
    skipping.feed(invalid, None).map_err(|e| e.to_string())?;
    enqueue(&mut skipping, units, 0, false)?;
    if !skipping.step(Some(START))
        || skipping.refusals() != 1
        || skipping.last_refusal() != Some(error)
        || skipping
            .latest()
            .is_none_or(|picture| picture.rgba.as_slice() != first_rgba)
    {
        return Err("skip policy did not preserve the first valid picture or refusal".into());
    }
    record(work, trace, &[5, skipping.refusals()])
}

pub(super) fn prepare<'a>(input: &'a [u8], args: &[OsString]) -> Result<Operation<'a>, String> {
    let [codec, width, height, positions] = args else {
        return Err("expected AVC|HEVC WIDTH HEIGHT PICTURE_POSITIONS".into());
    };
    let coding = match codec.to_str() {
        Some("avc") => (Codec::Avc, VideoCodec::H264),
        Some("hevc") => (Codec::Hevc, VideoCodec::Hevc),
        _ => return Err("unknown session codec".into()),
    };
    let number = |value: &OsString| {
        value
            .to_str()
            .and_then(|s| s.parse().ok())
            .ok_or("invalid canvas size")
    };
    let canvas = Canvas::new(number(width)?, number(height)?).ok_or("invalid canvas geometry")?;
    let positions = positions
        .to_str()
        .ok_or("invalid picture positions")?
        .split(',')
        .enumerate()
        .map(|(index, value)| {
            let position = value.parse::<u64>().map_err(|e| e.to_string())?;
            let index = i64::try_from(index).map_err(|e| e.to_string())?;
            let pts = index
                .checked_mul(PERIOD)
                .and_then(|ticks| ticks.checked_add(START))
                .ok_or("picture timestamp overflow")?;
            Ok((position, pts))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    if positions.is_empty() {
        return Err("empty picture position list".into());
    }
    Ok(Operation {
        name: "video-session-clock",
        run: Box::new(move |work, trace| produce(input, coding, canvas, &positions, work, trace)),
    })
}
