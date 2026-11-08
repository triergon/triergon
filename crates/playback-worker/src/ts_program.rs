//! One TS188 or M2TS192 program through the playback engine's native packet view:
//! open a preloaded seekable source, select explicit PIDs of program 1 with their
//! carriage, project native clocks and fold every selected PES packet through the
//! end of the source.
//!
//! TS188 uses `Engine::ts_native_program_v1` with every selection. M2TS192 starts
//! through `NativeStartup::new_m2ts_prefix_v2`, which admits one registered
//! carriage. Times are 90 kHz ticks of the native projection; the duration is the
//! demuxer's raw 90 kHz duration and the flags its packet flags.

use crate::packet_fold::{Packet, Work, drive};
use crate::runner::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{
    PacketFormat, ParseLimits,
    clock::SourceTimePolicy,
    compat::pes::{DemuxPacket, DemuxTimeView, PrivateAudioCarriage, RegisteredCarriage},
    psi::{DescriptorNamespace, DvbNamespaceEvidence},
};
use triergon_playback::{
    ByteSource, ClockChoice, Engine, Format, Player, PlayerPolicy, Progress, RawEvent, Selection,
    TsNativeCarriage, TsNativeSelection, TsNativeTrackSelection,
    io::{SelectedInput, SourceOwner},
    native_startup::{NativeStartup, StartupStep},
};

pub const PREFIX: &str = "ts-program-";
const OPERATION: &str = "ts-native-program";
const PROGRAM: u16 = 1;
const WINDOW: usize = 32 * 1024;
const WORK: u64 = 1 << 16;
const NAMESPACE: DescriptorNamespace =
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);

fn carriage(name: &str) -> Option<TsNativeCarriage> {
    Some(match name {
        "avc" => TsNativeCarriage::Registered(RegisteredCarriage::Avc),
        "hevc" => TsNativeCarriage::Registered(RegisteredCarriage::Hevc),
        "aac" => TsNativeCarriage::Registered(RegisteredCarriage::AacAdts),
        "latm" => TsNativeCarriage::Registered(RegisteredCarriage::AacLatm),
        "audio" => TsNativeCarriage::AudioFromPmt,
        "dvb-ac3" => TsNativeCarriage::PrivateAudio(PrivateAudioCarriage::DvbAc3Tag),
        "dvb-eac3" => TsNativeCarriage::PrivateAudio(PrivateAudioCarriage::DvbEac3Tag),
        "ac3" => TsNativeCarriage::PrivateAudio(PrivateAudioCarriage::PrivateType81Ac3),
        "eac3" => TsNativeCarriage::PrivateAudio(PrivateAudioCarriage::PrivateType87Eac3),
        "dvbsub" => TsNativeCarriage::DvbSubtitles,
        _ => return None,
    })
}

/// `FORMAT:PID=CARRIAGE,...`, for example `ts188:256=avc,257=audio`.
fn selections(argument: &str) -> Result<(PacketFormat, Vec<TsNativeTrackSelection>)> {
    let invalid = || format!("invalid program selection {argument:?}");
    let (format, list) = argument.split_once(':').ok_or_else(invalid)?;
    let format = match format {
        "ts188" => PacketFormat::Ts188,
        "m2ts192" => PacketFormat::M2ts192,
        _ => return Err(invalid().into()),
    };
    let mut selections = Vec::new();
    for item in list.split(',') {
        let (pid, name) = item.split_once('=').ok_or_else(invalid)?;
        selections.push(TsNativeTrackSelection {
            pid: pid.parse()?,
            carriage: carriage(name).ok_or_else(invalid)?,
        });
    }
    Ok((format, selections))
}

fn fold(work: &mut Work, packet: &DemuxPacket, clock: &DemuxTimeView) -> Result<()> {
    let time = |value: Option<i128>| value.map(i64::try_from).transpose();
    work.packet(Packet {
        track: u64::from(packet.pid),
        pts: time(clock.pts90)?,
        dts: time(clock.dts90)?,
        duration: u64::try_from(packet.duration90_raw).unwrap_or(0),
        flags: u64::from(packet.flags),
        size: packet.payload_len,
        payload: packet.payload(),
    })
}

fn player(
    pool: &Budget,
    raw: &Bytes,
    format: PacketFormat,
    selections: &[TsNativeTrackSelection],
) -> Result<Player<ByteSource>> {
    let cancel = AtomicBool::new(false);
    let id = SourceId(42);
    let caps = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: format == PacketFormat::Ts188,
    };
    let input = InputCursor::new(pool, id, caps, WINDOW)?;
    let source = SourceOwner::new(ByteSource::new(id, raw.clone(), WINDOW)?);
    let limits = ParseLimits::default();
    let policy = SourceTimePolicy::Unanchored;
    let mut player = if format == PacketFormat::Ts188 {
        let engine = Engine::ts_native_program_v1(
            input, format, PROGRAM, limits, policy, NAMESPACE, selections,
        )?;
        Player::new(engine, source, Selection::All, PlayerPolicy::default())?
    } else {
        let [
            TsNativeTrackSelection {
                pid,
                carriage: TsNativeCarriage::Registered(carriage),
            },
        ] = selections
        else {
            return Err("M2TS192 admits one registered carriage".into());
        };
        let mut startup = NativeStartup::new_m2ts_prefix_v2(SelectedInput {
            input,
            source,
            format: Format::Ts(format),
        })?;
        while startup.advance(&mut WorkBudget::new(WORK), &cancel)? != StartupStep::Ready {}
        let native = TsNativeSelection {
            pid: *pid,
            carriage: *carriage,
        };
        startup.finish()?.into_player(
            PROGRAM,
            limits,
            policy,
            native,
            Selection::All,
            PlayerPolicy::default(),
        )?
    };
    player.set_clock_view(ClockChoice::ExplicitNativeProjection)?;
    Ok(player)
}

/// One operation on a preloaded input with the case argument.
pub fn operate(pool: &Budget, raw: &Bytes, argument: &str, mut work: Work) -> Result<Work> {
    let (format, selections) = selections(argument)?;
    let cancel = AtomicBool::new(false);
    let mut player = player(pool, raw, format, &selections)?;
    let mut pending = None;
    loop {
        let progress = player.read_output(&mut pending, &mut WorkBudget::new(WORK), &cancel)?;
        if let Some(output) = pending.take() {
            match output.event() {
                RawEvent::NativeTs { packet, clock } => fold(&mut work, packet, clock)?,
                RawEvent::NativeRefusal { track, error, .. } => {
                    return Err(format!("native refusal on {track:?}: {error:?}").into());
                }
                _ => {}
            }
        }
        match progress {
            Progress::Ended => break,
            Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
            other => return Err(format!("unexpected player progress: {other:?}").into()),
        }
    }
    Ok(work)
}

/// `ts-program-fold FILE SELECTION`, `ts-program-bench FILE ITERATIONS SELECTION` or
/// `ts-program-memory FILE SELECTION`; SELECTION is `FORMAT:PID=CARRIAGE,...` with
/// FORMAT `ts188` or `m2ts192` and CARRIAGE `avc`, `hevc`, `aac`, `latm`, `audio`
/// (resolved from each PMT), `dvb-ac3`, `dvb-eac3`, `ac3`, `eac3` or `dvbsub`.
pub fn run(args: &[String]) -> Result<()> {
    drive(args, PREFIX, OPERATION, operate)
}
