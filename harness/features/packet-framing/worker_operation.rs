//! Complete selected initialization/packet/error/flush/retention operation.
use super::model::{Case, Digest, Event, Outcome};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, WorkBudget,
};
use triergon_framing::compat::nal::*;
use triergon_framing::*;

const QUANTUM: u64 = 4096;
static CANCEL: AtomicBool = AtomicBool::new(false);

#[inline(never)]
fn avc_into(
    pool: &Budget,
    raw: Bytes,
    offset: u64,
    update: bool,
    ready: &mut Option<ReceiverAvcConfiguration>,
) -> std::result::Result<(), (Error, Option<i32>)> {
    let source = AvcConfigSource {
        source: SourceId(1),
        generation: 1,
        offset,
    };
    let limits = AvcConfigLimits {
        maximum_record_bytes: 65536,
        maximum_nals: 65536,
        maximum_nal_bytes: 65535,
    };
    let mut parser = if update {
        ReceiverAvcConfigParser::new_update(pool, source, raw, limits)
    } else {
        ReceiverAvcConfigParser::new(pool, source, raw, limits)
    }
    .map_err(|e| (e, None))?;
    loop {
        match parser.step_into(&mut WorkBudget::new(QUANTUM), &CANCEL, ready) {
            Ok(ConfigProgress::Ready) => return Ok(()),
            Ok(ConfigProgress::Yield) => (),
            Ok(ConfigProgress::Complete) => {
                return Err((Error::Invalid("configuration missing"), None));
            }
            Err(e) => {
                return Err((
                    e,
                    parser
                        .take_update_rejection()
                        .map(|r| r.native_receive_result()),
                ));
            }
        }
    }
}

#[inline(never)]
fn configuration_into(case: &Case, ready: &mut Option<NalPacketConfiguration>) -> Result<()> {
    let raw = &case.configuration;
    if raw.len() < 4 || raw.starts_with(&[0, 0, 1]) || raw.starts_with(&[0, 0, 0, 1]) {
        *ready = Some(NalPacketConfiguration::Passthrough {
            codec: case.codec,
            raw: raw.clone(),
        });
        return Ok(());
    }
    match case.codec {
        Codec::Avc => {
            let mut record = None;
            avc_into(
                &case.pool,
                raw.clone(),
                case.config_offset,
                false,
                &mut record,
            )
            .map_err(|(e, _)| e)?;
            *ready = Some(NalPacketConfiguration::ReceiverAvc(
                record.ok_or(Error::Invalid("configuration missing"))?,
            ));
            Ok(())
        }
        Codec::Hevc => {
            let mut parser = ReceiverHevcConfigParser::new(
                &case.pool,
                HvcConfigSource {
                    source: SourceId(1),
                    generation: 1,
                    offset: case.config_offset,
                },
                raw.clone(),
                HvcConfigLimits {
                    maximum_record_bytes: 65536,
                    maximum_arrays: 255,
                    maximum_nals: 65536,
                    maximum_nal_bytes: 65535,
                },
            )?;
            let mut record = None;
            loop {
                match parser.step_into(&mut WorkBudget::new(QUANTUM), &CANCEL, &mut record)? {
                    ConfigProgress::Ready => {
                        *ready = Some(NalPacketConfiguration::ReceiverHevc(
                            record.ok_or(Error::Invalid("configuration missing"))?,
                        ));
                        return Ok(());
                    }
                    ConfigProgress::Yield => (),
                    ConfigProgress::Complete => {
                        return Err(Error::Invalid("configuration missing"));
                    }
                }
            }
        }
    }
}

#[inline]
fn packet_digest(hash: &mut Digest, kind: u64, input: i64, ordinal: u64, packet: &NalPacketOutput) {
    let fields = packet.source().fields;
    hash.words(&[
        kind,
        input as u64,
        ordinal,
        fields.pts as u64,
        fields.dts as u64,
        fields.duration as u64,
        fields.position as u64,
        i64::from(fields.flags as i32) as u64,
        i64::from(fields.stream_index) as u64,
        1,
        90000,
    ]);
    hash.blob(packet.bytes());
    let extra = packet
        .source()
        .new_extradata
        .as_ref()
        .filter(|_| packet.forward_new_extradata());
    hash.word(u64::from(extra.is_some()));
    if let Some(bytes) = extra {
        hash.word(1);
        hash.blob(bytes);
    }
}

fn receive_failure(hash: &mut Digest, input: usize, code: i32) {
    hash.words(&[3, input as u64, i64::from(code) as u64]);
}

fn retain_output<T>(kept: &mut BudgetVec<T>, value: T, following: &[Event]) -> Result<()> {
    if kept.is_empty() {
        // Each complete-packet action publishes at most one output. Read only
        // action tags after an actual Ready, never codec state or golds. One
        // reservation for every remaining packet action avoids a doubling
        // replacement, whose old and new buffers would both be live.
        let initial = 1 + following
            .iter()
            .filter(|event| matches!(event, Event::Packet { .. }))
            .count();
        // Keep the original minimum-slot admission when the larger bound
        // cannot fit. Allocation failure retains its original refusal.
        match kept.try_reserve_exact(initial) {
            Ok(()) | Err(Error::Backpressure { .. } | Error::Limit { .. } | Error::Overflow) => (),
            Err(error) => return Err(error),
        }
    }
    kept.try_push(value).map_err(|e| e.error)
}

fn operation(case: &Case) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    let config = {
        let mut ready = None;
        configuration_into(case, &mut ready)?;
        ready.ok_or(Error::Invalid("configuration missing"))?
    };
    let mut session = NalPacketSession::new(
        &case.pool,
        config,
        NalPacketLimits {
            maximum_packet_bytes: 16 * 1024 * 1024,
            maximum_output_bytes: 32 * 1024 * 1024,
            maximum_nals: 65536,
            maximum_segments: 131072,
        },
    )?;
    let configured = {
        let mut writer =
            NalPacketConfigWriter::new(&case.pool, session.configuration(), 32 * 1024 * 1024)?;
        loop {
            match writer.step(&mut WorkBudget::new(QUANTUM), &CANCEL) {
                Ok(BuildStep::Ready(v)) => break v,
                Ok(BuildStep::Yield) => (),
                Ok(BuildStep::Complete) => {
                    return Err(Error::Invalid("configuration publication missing"));
                }
                Err(error) => {
                    if let Some(rejection) = writer.take_receiver_rejection() {
                        outcome.digest.word(4);
                        outcome
                            .digest
                            .signed(i64::from(rejection.native_initialize_result()));
                        outcome.digest.word(6);
                        outcome.digest.word(0);
                        return Ok(outcome);
                    }
                    return Err(error);
                }
            }
        }
    };
    outcome.digest.word(1);
    outcome.digest.blob(configured.bytes());
    outcome.digest.signed(1);
    outcome.digest.signed(90000);
    let mut kept = BudgetVec::new(&case.pool);
    let mut drained = false;
    for (index, event) in case.events.iter().enumerate() {
        let Event::Packet {
            fields,
            bytes,
            extra,
            offset,
            extra_offset,
        } = event
        else {
            match event {
                Event::Flush => {
                    session.flush();
                    drained = false;
                }
                Event::Drain => {
                    session.drain();
                    drained = true;
                }
                Event::Packet { .. } => unreachable!(),
            }
            continue;
        };
        let mut update = None;
        if case.codec == Codec::Avc
            && let Some(raw) = extra.as_ref().filter(|b| !b.is_empty())
        {
            match avc_into(&case.pool, raw.clone(), *extra_offset, true, &mut update) {
                Ok(()) => (),
                Err((_, Some(code))) => {
                    receive_failure(&mut outcome.digest, index, code);
                    continue;
                }
                Err((error, None)) => return Err(error),
            }
        }
        let packet = NalPacket {
            bytes: bytes.clone(),
            fields: *fields,
            location: SourceLocation {
                source: SourceId(1),
                offset: *offset,
            },
            generation: 1,
            new_extradata: extra.clone(),
        };
        let mut job = match session.receiver_packet(packet, update) {
            Ok(job) => job,
            Err(Error::Invalid("packet after drain requires flush")) if drained => continue,
            Err(error) => return Err(error),
        };
        let mut ready = None;
        loop {
            match job.step_into(&mut WorkBudget::new(QUANTUM), &CANCEL, &mut ready) {
                Ok(NalPacketProgress::Yield) => (),
                Ok(NalPacketProgress::Complete) => {
                    return Err(Error::Invalid("packet publication missing"));
                }
                Ok(NalPacketProgress::Ready) => {
                    let packet = ready
                        .take()
                        .ok_or(Error::Invalid("missing published packet"))?;
                    packet_digest(
                        &mut outcome.digest,
                        2,
                        index as i64,
                        outcome.packets,
                        &packet,
                    );
                    outcome.bytes = outcome
                        .bytes
                        .checked_add(packet.bytes().len() as u64)
                        .ok_or(Error::Overflow)?;
                    outcome.packets = outcome.packets.checked_add(1).ok_or(Error::Overflow)?;
                    retain_output(&mut kept, packet, &case.events[index + 1..])?;
                    break;
                }
                Err(error) => {
                    if let Some(rejection) = job.take_receiver_rejection() {
                        receive_failure(
                            &mut outcome.digest,
                            index,
                            rejection.native_receive_result(),
                        );
                        break;
                    }
                    return Err(error);
                }
            }
        }
    }
    drop(session);
    outcome.digest.word(6);
    outcome.digest.word(outcome.packets);
    for (index, packet) in kept.as_slice().iter().enumerate() {
        packet_digest(&mut outcome.digest, 5, -1, index as u64, packet);
    }
    drop(configured);
    Ok(outcome)
}

pub fn run(case: &Case) -> Result<Outcome> {
    let result = operation(case);
    if case.pool.used() != case.baseline {
        return Err(Error::Invalid("retained charge after complete operation"));
    }
    result
}
