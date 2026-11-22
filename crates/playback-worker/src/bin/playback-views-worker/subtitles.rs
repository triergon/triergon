//! Containers played through the public subtitle bridges.
mod bitmap;
mod captions;

use playback_worker::runner::{Invocation, MAX_INPUT, Mode, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_bridge::text::{TextBridge, TextSource, TextStep};
use triergon_core::{
    Budget, Bytes, InputCursor, Rounding, SourceCapabilities, SourceId, Time, TimeBase, WorkBudget,
};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes},
    hex,
};
use triergon_mpegts::{
    PacketFormat,
    clock::SourceTimePolicy,
    psi::{DescriptorNamespace, DvbNamespaceEvidence},
};
use triergon_playback::{
    ByteSource, Engine, Event, Format, Output, Player, PlayerPolicy, Progress, Selection,
    SourceOwner, TrackKey,
    mp4::{
        Mp4Event, Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits, ProgressiveLimits,
    },
    probe::SelectedInput,
};
use triergon_subtitles::text;

const ID: SourceId = SourceId(1);
const WINDOW: usize = 32 * 1024;
const PACKET_LIMIT: usize = 4 * 1024 * 1024;
const GRANT: u64 = 1 << 20;
const MAX_POLLS: usize = 4_000_000;
static CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Mov,
    Mkv,
    Ts,
    M2ts,
}

impl Container {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "mov" => Self::Mov,
            "mkv" => Self::Mkv,
            "ts" => Self::Ts,
            "m2ts" => Self::M2ts,
            _ => return Err("subtitle container must be mov, mkv, ts or m2ts".into()),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    Tx3g,
    Utf8,
    Ass,
    Vtt,
    Pgs,
    Dvd,
    Dvb,
    Cc,
    Teletext,
}

struct Case<'a> {
    kind: &'a str,
    container: Container,
    codec: Codec,
    ordinal: usize,
    program: u16,
    options: &'a [String],
}

impl<'a> Case<'a> {
    fn parse(args: &'a [String], is_text: bool) -> Result<Self> {
        let container = Container::parse(&args[1])?;
        let codec = match args[2].as_str() {
            "tx3g" => Codec::Tx3g,
            "utf8" => Codec::Utf8,
            "ass" => Codec::Ass,
            "vtt" => Codec::Vtt,
            "pgs" => Codec::Pgs,
            "dvd" => Codec::Dvd,
            "dvb" => Codec::Dvb,
            "cc" => Codec::Cc,
            "teletext" => Codec::Teletext,
            _ => return Err("unknown subtitle codec".into()),
        };
        let kind = args[0].as_str();
        let valid = match kind {
            "text" => matches!(
                (container, codec),
                (Container::Mov, Codec::Tx3g)
                    | (Container::Mkv, Codec::Utf8 | Codec::Ass | Codec::Vtt)
            ),
            "pgs" => container == Container::Mkv && codec == Codec::Pgs,
            "dvd" => container == Container::Mkv && codec == Codec::Dvd,
            "mkv-bitmap" => {
                container == Container::Mkv && matches!(codec, Codec::Pgs | Codec::Dvd | Codec::Dvb)
            }
            "mkv-bitmap-probe" => {
                container == Container::Mkv && matches!(codec, Codec::Pgs | Codec::Dvb)
            }
            "mp4-dvd" => container == Container::Mov && codec == Codec::Dvd,
            "ts-dvb" => matches!(container, Container::Ts | Container::M2ts) && codec == Codec::Dvb,
            "ts-pgs" => matches!(container, Container::Ts | Container::M2ts) && codec == Codec::Pgs,
            "cc" => codec == Codec::Cc,
            "teletext" => {
                matches!(container, Container::Ts | Container::M2ts) && codec == Codec::Teletext
            }
            _ => false,
        };
        if !valid || is_text != (kind == "text") {
            return Err("unsupported bridge, container and codec combination".into());
        }
        Ok(Self {
            kind,
            container,
            codec,
            ordinal: args[3].parse()?,
            program: args[4].parse()?,
            options: &args[5..],
        })
    }

    fn option(&self, key: &str) -> Option<&str> {
        self.options.iter().find_map(|word| {
            let (name, value) = word.strip_prefix("opt:")?.split_once('=')?;
            (name == key).then_some(value)
        })
    }
}

struct Records {
    totals: Totals,
    trace: bool,
}

impl Records {
    fn record(&mut self, kind: u64, words: &[u64], payload: &[u8]) {
        fold(&mut self.totals.digest, kind);
        for &word in words {
            fold(&mut self.totals.digest, word);
        }
        fold_bytes(&mut self.totals.digest, payload);
        self.totals.packets += 1;
        self.totals.bytes += payload.len() as u64;
        if self.trace {
            print!("{kind}");
            for word in words {
                print!(" {word}");
            }
            if !payload.is_empty() {
                print!(" {}", hex(payload));
            }
            println!();
        }
    }

    fn cue(&mut self, cue: &text::Cue) -> Result<()> {
        let body = cue.decoded_utf8.as_ref().unwrap_or(&cue.input.payload);
        self.record(
            1,
            &[micros(cue.start)?, micros(cue.duration)?, body.len() as u64],
            body,
        );
        for (kind, field) in [(2, &cue.identifier), (3, &cue.settings)] {
            if let Some(field) = field {
                let bytes = field.view(&cue.input)?;
                self.record(kind, &[bytes.len() as u64], &bytes);
            }
        }
        Ok(())
    }
}

const ABSENT: u64 = i64::MIN as u64;

fn micros(time: Option<Time>) -> Result<u64> {
    Ok(time
        .map(|time| time.to_i64_ticks(TimeBase::new(1, 1_000_000)?, Rounding::NearestAway))
        .transpose()?
        .map_or(ABSENT, |time| time as u64))
}

fn player(case: &Case<'_>, pool: &Budget, raw: &Bytes) -> Result<Player<ByteSource>> {
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let source = SourceOwner::new(ByteSource::new(ID, raw.clone(), WINDOW)?);
    let input = InputCursor::new(pool, ID, capabilities, WINDOW)?;
    let engine = match case.container {
        Container::Mov => {
            let selected = SelectedInput {
                input,
                source,
                format: Format::Mp4,
            };
            let progressive = case.option("progressive") == Some("1");
            let mut open = if progressive {
                Mp4Open::new_progressive(selected, OpenLimits::default())?
            } else {
                Mp4Open::new(selected, OpenLimits::default())?
            };
            loop {
                if matches!(
                    open.advance(&mut WorkBudget::new(GRANT), &CANCEL)?,
                    OpenProgress::Ready
                ) {
                    break;
                }
            }
            let opened = open.finish()?;
            let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default())?;
            while !index.poll(&mut WorkBudget::new(GRANT), &CANCEL)? {}
            let plan = index.finish()?;
            let engine = if progressive {
                Engine::mp4_progressive(
                    opened.input,
                    plan,
                    PACKET_LIMIT,
                    ProgressiveLimits::default(),
                )?
            } else {
                Engine::mp4(opened.input, plan, PACKET_LIMIT)?
            };
            return Ok(Player::new(
                engine,
                opened.source,
                Selection::All,
                PlayerPolicy::default(),
            )?);
        }
        Container::Mkv => Engine::matroska(triergon_matroska::Reader::new(
            pool,
            ID,
            capabilities,
            triergon_matroska::Limits::default(),
            WINDOW,
        )?),
        Container::Ts | Container::M2ts => Engine::ts(
            input,
            if case.container == Container::Ts {
                PacketFormat::Ts188
            } else {
                PacketFormat::M2ts192
            },
            case.program,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
        )?,
    };
    Ok(Player::new(
        engine,
        source,
        Selection::All,
        PlayerPolicy::default(),
    )?)
}

fn next(player: &mut Player<ByteSource>) -> Result<Option<Output>> {
    for _ in 0..MAX_POLLS {
        if let Some(output) = player.pop() {
            return Ok(Some(output));
        }
        match player.advance(&mut WorkBudget::new(GRANT), &CANCEL)? {
            Progress::Queued | Progress::Yield | Progress::NeedInput(_) => {}
            Progress::Ended => return Ok(player.pop()),
            state => return Err(format!("subtitle player stopped at {state:?}").into()),
        }
    }
    Err("subtitle player poll limit".into())
}

fn drain_text(bridge: &mut TextBridge, records: &mut Records) -> Result<()> {
    loop {
        match bridge.step(&mut WorkBudget::new(GRANT), &CANCEL)? {
            TextStep::Cue(event) => records.cue(&event.cue)?,
            TextStep::Yield => {}
            TextStep::NeedPacket | TextStep::Finished => return Ok(()),
        }
    }
}

fn cue_from(bridge: &mut TextBridge, source: TextSource, records: &mut Records) -> Result<()> {
    bridge.provide(source).map_err(|failure| failure.error)?;
    drain_text(bridge, records)
}

fn text_track(
    case: &Case<'_>,
    pool: &Budget,
    player: &mut Player<ByteSource>,
    records: &mut Records,
) -> Result<()> {
    let mut target = None;
    let mut bridge = None;
    let mut mov_tracks = Vec::new();
    while let Some(output) = next(player)? {
        match output.event() {
            Event::Matroska(triergon_matroska::Event::Configuration(config))
                if target.is_none() =>
            {
                let names: &[&str] = match case.codec {
                    Codec::Utf8 => &["S_TEXT/UTF8"],
                    Codec::Ass => &["S_TEXT/ASS", "S_TEXT/SSA"],
                    Codec::Vtt => &["S_TEXT/WEBVTT"],
                    _ => return Err("unsupported Matroska text mapping".into()),
                };
                target = config
                    .tracks()
                    .iter()
                    .filter(|track| names.contains(&track.codec.as_ref()))
                    .nth(case.ordinal)
                    .map(|track| TrackKey::Matroska(track.number));
            }
            Event::Mp4(Mp4Event::Packet(packet))
                if target.is_none() && packet.configuration.description.format == *b"tx3g" =>
            {
                let id = packet.configuration.track.track_id;
                if !mov_tracks.contains(&id) {
                    if mov_tracks.len() == case.ordinal {
                        target = Some(TrackKey::Mp4(id));
                    }
                    mov_tracks.push(id);
                }
            }
            _ => {}
        }
        let track = match target {
            Some(TrackKey::Matroska(track)) => track,
            Some(TrackKey::Mp4(track)) => u64::from(track),
            _ => continue,
        };
        if output.event().track() == target && output.packet().is_some() {
            let bridge = bridge.get_or_insert_with(|| {
                TextBridge::new(
                    pool,
                    ID,
                    track,
                    output.generation(),
                    text::Limits::default(),
                )
            });
            cue_from(
                bridge,
                TextSource::from_output(output).map_err(|failure| failure.error)?,
                records,
            )?;
        }
    }
    if let Some(mut bridge) = bridge {
        bridge.finish();
        drain_text(&mut bridge, records)?;
    } else if target.is_none() {
        return Err("text track absent".into());
    }
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let is_text = args[0].starts_with("text-bridge-");
    let prefix = if is_text {
        "text-bridge-"
    } else {
        "subtitle-bridge-"
    };
    let invocation = Invocation::parse(
        args,
        prefix,
        " BRIDGE CONTAINER CODEC STREAM PROGRAM [opt:KEY=VALUE...]",
        5..=12,
        true,
    )?;
    let case = Case::parse(invocation.arguments, is_text)?;
    let file = invocation.input(MAX_INPUT)?;
    let pool = Budget::new(256 << 20)?;
    let raw = Bytes::copy_from_slice(&pool, &file)?;
    drop(file);
    measure(
        &invocation,
        if is_text {
            "text-subtitle-bridge"
        } else {
            "subtitle-bridge"
        },
        Some(&pool),
        || {
            let mut records = Records {
                totals: Totals {
                    packets: 0,
                    bytes: 0,
                    digest: 0,
                    refusal: None,
                },
                trace: invocation.mode == Mode::Trace,
            };
            let result = (|| {
                let mut player = player(&case, &pool, &raw)?;
                match case.codec {
                    Codec::Tx3g | Codec::Utf8 | Codec::Ass | Codec::Vtt => {
                        text_track(&case, &pool, &mut player, &mut records)
                    }
                    Codec::Cc => captions::closed(&case, &pool, &mut player, &mut records),
                    Codec::Teletext => captions::teletext(&case, &pool, &mut player, &mut records),
                    _ => bitmap::play(&case, &pool, &mut player, &mut records),
                }
            })();
            if records.trace
                && let Err(error) = &result
            {
                println!("# candidate: {error}");
            }
            records.record(9, &[u64::from(result.is_err())], &[]);
            Ok(records.totals)
        },
    )
}
