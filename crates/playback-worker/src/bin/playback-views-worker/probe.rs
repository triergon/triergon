//! Container selection and the bytes replayed to a consumer after probing.

mod lifecycle;

use playback_worker::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, SourceCapabilities, SourceId, Step, WorkBudget,
    signature::TransportSync,
};
use triergon_framing::nal::{Codec, RawAccessUnit, RawAnnexBLimits, RawAnnexBReader};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes},
    md5,
};
use triergon_mpegts::PacketFormat;
use triergon_playback::{
    ByteSource, Format, Source, SourceOwner,
    probe::{Probe, ProbeProgress, SelectedInput},
};
use triergon_raw_video::{
    PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, RawVideoProbeLimits,
    RawVideoSource,
};

const ID: SourceId = SourceId(1);
const WORK: u64 = 1 << 16;

pub(super) fn run_lifecycle(args: &[String]) -> Result<()> {
    lifecycle::run(args)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, "probe-", " playback|raw PREFIX WINDOW", 3..=3, true)?;
    let raw = match invocation.arguments[0].as_str() {
        "playback" => false,
        "raw" => true,
        _ => return Err("expected playback or raw probe".into()),
    };
    let mut data = invocation.input(MAX_INPUT)?;
    let length = if invocation.arguments[1] == "whole" {
        data.len()
    } else {
        invocation.arguments[1].parse::<usize>()?
    };
    data.truncate(length);
    let window = invocation.arguments[2].parse::<usize>()?;
    if !(1..=65536).contains(&window) {
        return Err("probe window must be in 1..=65536".into());
    }
    let pool = Budget::new(256 << 20)?;
    let data = Bytes::try_from_vec(&pool, data).map_err(|error| error.error)?;
    let trace = invocation.mode == Mode::Trace;
    measure(&invocation, "playback-probe", Some(&pool), || {
        compare(&pool, &data, window, raw, trace)
    })
}

fn compare(pool: &Budget, data: &Bytes, window: usize, raw: bool, trace: bool) -> Result<Totals> {
    let cancel = AtomicBool::new(false);
    let caps = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let source = ByteSource::new(ID, data.clone(), window)?;
    let (verdict, width, replay) = if raw {
        let mut probe = PlaybackProbe::new(
            pool,
            ID,
            caps,
            window,
            source,
            RawVideoProbeLimits::default(),
        )?;
        let detection = loop {
            match probe.advance(&mut WorkBudget::new(WORK), &cancel) {
                Ok(PlaybackProbeStep::Detected(kind)) => break kind,
                Ok(PlaybackProbeStep::NeedInput(_) | PlaybackProbeStep::Yield) => {}
                Err(error) => return refused(error, data, trace),
            }
        };
        if trace {
            eprintln!("probe detection: {detection:?}");
        }
        match (detection, probe.finish(RawAnnexBLimits::default())?) {
            (PlaybackInputKind::Container(format), PlaybackInput::Container(selected))
                if format == selected.format =>
            {
                handoff(selected, &cancel)?
            }
            (PlaybackInputKind::RawVideo(detected), PlaybackInput::RawVideo { codec, source })
                if detected == codec =>
            {
                let reader =
                    RawAnnexBReader::new(pool, codec, ID, caps, RawAnnexBLimits::default())?;
                let direct =
                    RawVideoSource::new(reader, ByteSource::new(ID, data.clone(), window)?);
                check_raw_replay(source, direct, data, &cancel)?;
                (
                    match codec {
                        Codec::Avc => 9,
                        Codec::Hevc => 10,
                    },
                    0,
                    None,
                )
            }
            _ => return Err("combined probe detection differs from its handoff".into()),
        }
    } else {
        let mut probe = Probe::new(pool, ID, caps, window, source)?;
        let format = loop {
            match probe.advance(&mut WorkBudget::new(WORK), &cancel) {
                Ok(ProbeProgress::Selected(format)) => break format,
                Ok(ProbeProgress::NeedInput(_) | ProbeProgress::Yield) => {}
                Err(error) => return refused(error, data, trace),
            }
        };
        let selected = probe.finish()?;
        if format != selected.format {
            return Err("container probe selection differs from its handoff".into());
        }
        handoff(selected, &cancel)?
    };
    let width = if verdict == 8 {
        width
    } else {
        observed_width(data)
    };
    Ok(record(verdict, width, replay, trace))
}

fn raw_step<S: Source>(
    source: &mut RawVideoSource<S>,
    cancel: &AtomicBool,
) -> triergon_core::Result<Step<RawAccessUnit>> {
    loop {
        match source.poll(&mut WorkBudget::new(WORK), cancel)? {
            Step::Yield => {}
            Step::NeedInput(_) => return Err(Error::Invalid("preloaded raw source went pending")),
            step => return Ok(step),
        }
    }
}

fn check_raw_replay<S: Source>(
    mut replay: RawVideoSource<S>,
    mut direct: RawVideoSource<ByteSource>,
    original: &[u8],
    cancel: &AtomicBool,
) -> Result<()> {
    loop {
        match (raw_step(&mut replay, cancel), raw_step(&mut direct, cancel)) {
            (Ok(Step::Ready(a)), Ok(Step::Ready(b))) => {
                let start = usize::try_from(b.range.start)?;
                let end = usize::try_from(b.range.end)?;
                let bytes = original
                    .get(start..end)
                    .ok_or("raw unit outside its source")?;
                if a.source != b.source
                    || a.codec != b.codec
                    || a.range != b.range
                    || a.has_vcl != b.has_vcl
                    || a.random_access != b.random_access
                    || !a.segments().flatten().copied().eq(bytes.iter().copied())
                {
                    return Err("raw probe handoff changed the original access unit".into());
                }
            }
            (Ok(Step::End(a)), Ok(Step::End(b))) if a == b => return Ok(()),
            (Err(a), Err(b))
                if a == b && a != Error::Invalid("preloaded raw source went pending") =>
            {
                return Ok(());
            }
            _ => return Err("raw probe handoff changed the terminal result".into()),
        }
    }
}

fn refused(error: Error, data: &[u8], trace: bool) -> Result<Totals> {
    match error {
        Error::Unsupported(_) | Error::Invalid(_) | Error::Truncated | Error::Limit { .. } => {
            if trace {
                eprintln!("probe refusal: {error}");
            }
            Ok(record(0, observed_width(data), None, trace))
        }
        _ => Err(error.into()),
    }
}

fn observed_width(data: &[u8]) -> u64 {
    let ts = TransportSync::Ts188.observed(data) == Some(true);
    let m2ts = TransportSync::M2ts192.observed(data) == Some(true);
    match (ts, m2ts) {
        (true, false) => 188,
        (false, true) => 192,
        _ => 0,
    }
}

type Replay = (u64, [u8; 16]);

fn handoff<S: Source>(
    selected: SelectedInput<S>,
    cancel: &AtomicBool,
) -> Result<(u64, u64, Option<Replay>)> {
    let (verdict, width) = match selected.format {
        Format::Matroska => (5, 0),
        Format::Mp4 => (6, 0),
        Format::Ts(PacketFormat::Ts188) => (8, 188),
        Format::Ts(PacketFormat::M2ts192) => (8, 192),
    };
    Ok((
        verdict,
        width,
        Some(read_replay(selected.input, selected.source, cancel)?),
    ))
}

fn read_replay<S: Source>(
    mut input: InputCursor,
    mut source: SourceOwner<S>,
    cancel: &AtomicBool,
) -> Result<Replay> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let mut work = WorkBudget::new(WORK);
        match input.read_into(&mut buffer, &mut work, cancel)? {
            Step::Ready(count) => bytes.extend_from_slice(&buffer[..count]),
            Step::NeedInput(request) => {
                source
                    .poll(&request, &mut work, cancel)?
                    .provide_to(&mut input)?;
            }
            Step::Yield => {}
            Step::End(_) => return Ok((bytes.len() as u64, md5::digest(&bytes))),
        }
    }
}

fn record(verdict: u64, width: u64, replay: Option<Replay>, trace: bool) -> Totals {
    let mut digest = 0;
    for word in [3, verdict, width, u64::from(replay.is_some())] {
        fold(&mut digest, word);
    }
    if trace {
        print!("3 {verdict} {width} {}", u8::from(replay.is_some()));
    }
    let mut bytes = 0;
    if let Some((size, hash)) = replay {
        fold(&mut digest, size);
        fold_bytes(&mut digest, &hash);
        bytes = 16;
        if trace {
            print!(" {size} ");
            for byte in hash {
                print!("{byte:02x}");
            }
        }
    }
    if trace {
        println!();
    }
    Totals {
        packets: 1,
        bytes,
        digest,
        refusal: None,
    }
}
