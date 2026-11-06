//! Separate native packet/failure observation view; source refusals/raw facts
//! remain present.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, SourceLocation, WorkBudget};
use triergon_framing::*;
use triergon_harness_protocol::hex;
fn error(e: Error, at: i64) {
    let s = format!("{e:?}").replace('\\', "\\\\").replace('"', "\\\"");
    println!("{{\"event\":\"source_error\",\"input_event\":{at},\"error\":\"{s}\"}}");
}
struct Reader {
    b: Bytes,
    at: usize,
}
impl Reader {
    fn span(&mut self, n: usize) -> Result<Bytes, Error> {
        let end = self.at.checked_add(n).ok_or(Error::Overflow)?;
        let b = self.b.slice(self.at..end)?;
        self.at = end;
        Ok(b)
    }
    fn number(&mut self, n: usize) -> Result<u64, Error> {
        Ok(self
            .span(n)?
            .iter()
            .fold(0, |a, v| (a << 8) | u64::from(*v)))
    }
}
fn receiver_failure(phase: &str, at: i64, result: i32, raw: &Bytes) {
    println!(
        "{{\"event\":\"receiver_failure\",\"phase\":\"{phase}\",\"input_event\":{at},\"return\":{result},\"raw\":\"{}\"}}",
        hex(raw)
    );
}
fn source_refusal(kind: &str, at: i64, e: Error, raw: &Bytes) {
    let text = format!("{e:?}").replace('\\', "\\\\").replace('"', "\\\"");
    println!(
        "{{\"event\":\"{kind}\",\"input_event\":{at},\"error\":\"{text}\",\"raw\":\"{}\"}}",
        hex(raw)
    );
}
fn avc(
    pool: &Budget,
    b: Bytes,
    offset: u64,
    q: u64,
    update_at: Option<i64>,
) -> Result<NalObservedAvcConfiguration, Error> {
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
    let mut parser = if update_at.is_some() {
        NalObservedAvcConfigParser::new_update(pool, source, b, limits)?
    } else {
        NalObservedAvcConfigParser::new(pool, source, b, limits)?
    };
    for _ in 0..1_000_000 {
        match parser.step(&mut WorkBudget::new(q), &AtomicBool::new(false)) {
            Ok(BuildStep::Ready(v)) => return Ok(v),
            Ok(BuildStep::Yield) => (),
            Ok(BuildStep::Complete) => {
                return Err(Error::Invalid("missing observed AVC configuration"));
            }
            Err(e) => {
                if let (Some(at), Some(r)) = (update_at, parser.take_update_rejection()) {
                    receiver_failure("receive", at, r.native_receive_result(), r.source_bytes());
                }
                return Err(e);
            }
        }
    }
    Err(Error::Invalid("config stalled"))
}
fn hevc(
    pool: &Budget,
    b: Bytes,
    offset: u64,
    q: u64,
) -> Result<NalObservedHevcConfiguration, Error> {
    let mut parser = NalObservedHevcConfigParser::new(
        pool,
        HvcConfigSource {
            source: SourceId(1),
            generation: 1,
            offset,
        },
        b,
        HvcConfigLimits {
            maximum_record_bytes: 65536,
            maximum_arrays: 255,
            maximum_nals: 65536,
            maximum_nal_bytes: 65535,
        },
    )?;
    for _ in 0..1_000_000 {
        if let BuildStep::Ready(v) =
            parser.step(&mut WorkBudget::new(q), &AtomicBool::new(false))?
        {
            return Ok(v);
        }
    }
    Err(Error::Invalid("config stalled"))
}
fn main() -> Result<(), Error> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 3 {
        return Err(Error::Invalid("trace args"));
    }
    let q = args[2]
        .parse::<u64>()
        .map_err(|_| Error::Invalid("work quantum"))?;
    if q == 0 {
        return Err(Error::Invalid("work quantum"));
    }
    let pool = Budget::new(64 * 1024 * 1024)?;
    let raw = std::fs::read(&args[1]).map_err(|_| Error::Transport)?;
    let b = Bytes::try_from_vec(&pool, raw).map_err(|e| e.error())?;
    let mut r = Reader { b, at: 0 };
    if r.span(8)?.as_ref() != b"SMBSF001" {
        return Err(Error::Invalid("case magic"));
    }
    let codec = match r.number(1)? {
        1 => Codec::Avc,
        2 => Codec::Hevc,
        _ => return Err(Error::Invalid("case codec")),
    };
    let len = usize::try_from(r.number(4)?).map_err(|_| Error::Overflow)?;
    let off = r.at as u64;
    let config = r.span(len)?;
    let parsed = if config.len() < 4
        || config.starts_with(&[0, 0, 1])
        || config.starts_with(&[0, 0, 0, 1])
    {
        Ok(NalPacketConfiguration::Passthrough {
            codec,
            raw: config.clone(),
        })
    } else {
        match codec {
            Codec::Avc => {
                avc(&pool, config.clone(), off, q, None).map(NalPacketConfiguration::ObservedAvc)
            }
            Codec::Hevc => {
                hevc(&pool, config.clone(), off, q).map(NalPacketConfiguration::ObservedHevc)
            }
        }
    };
    let configuration = match parsed {
        Ok(v) => v,
        Err(e) => {
            error(e, -1);
            return Ok(());
        }
    };
    let mut session = NalPacketSession::new(
        &pool,
        configuration,
        NalPacketLimits {
            maximum_packet_bytes: 16 * 1024 * 1024,
            maximum_output_bytes: 32 * 1024 * 1024,
            maximum_nals: 65536,
            maximum_segments: 131072,
        },
    )?;
    if let Some(e) = session.configuration().strict_source_refusal() {
        source_refusal(
            "strict_record_refusal",
            -1,
            e,
            session.configuration().bytes(),
        );
    }
    let configured = {
        let mut job = NalPacketConfigWriter::new(&pool, session.configuration(), 32 * 1024 * 1024)?;
        loop {
            match job.step(&mut WorkBudget::new(q), &AtomicBool::new(false)) {
                Ok(BuildStep::Yield) => {}
                Ok(BuildStep::Ready(v)) => break v,
                Ok(BuildStep::Complete) => {
                    return Err(Error::Invalid("configuration not published"));
                }
                Err(e) => {
                    if let Some(r) = job.take_receiver_rejection() {
                        receiver_failure(
                            "initialize",
                            -1,
                            r.native_initialize_result(),
                            r.source_bytes(),
                        );
                    }
                    error(e, -1);
                    return Ok(());
                }
            }
        }
    };
    println!(
        "{{\"event\":\"initialize\",\"extradata\":\"{}\",\"time_base\":[1,90000]}}",
        hex(configured.bytes())
    );
    let mut kept = BudgetVec::new(&pool);
    let count = r.number(4)?;
    if count > 64 {
        return Err(Error::Limit {
            requested: count as usize,
            maximum: 64,
        });
    }
    for i in 0..count {
        let event = r.number(1)?;
        if event == 1 {
            session.flush();
            continue;
        }
        if event == 2 {
            session.drain();
            continue;
        }
        if event != 0 {
            return Err(Error::Invalid("case event"));
        }
        let fields = NalPacketFields {
            pts: r.number(8)? as i64,
            dts: r.number(8)? as i64,
            duration: r.number(8)? as i64,
            position: r.number(8)? as i64,
            flags: r.number(4)? as u32,
            stream_index: r.number(4)? as i32,
        };
        let len = r.number(4)? as usize;
        let en = r.number(4)? as u32;
        let pos = r.at;
        let data = r.span(len)?;
        let extra_pos = r.at;
        let extra = if en == u32::MAX {
            None
        } else {
            Some(r.span(en as usize)?)
        };
        println!(
            "{{\"event\":\"source_input\",\"input_event\":{i},\"raw\":\"{}\",\"pts\":{},\"dts\":{},\"duration\":{},\"pos\":{},\"flags\":{},\"stream\":{},\"new_extradata\":{}}}",
            hex(&data),
            fields.pts,
            fields.dts,
            fields.duration,
            fields.position,
            fields.flags,
            fields.stream_index,
            extra
                .as_ref()
                .map_or_else(|| "null".to_string(), |b| format!("\"{}\"", hex(b)))
        );
        let update = if codec == Codec::Avc {
            if let Some(b) = extra.as_ref().filter(|b| !b.is_empty()) {
                match avc(&pool, b.clone(), extra_pos as u64, q, Some(i as i64)) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        error(e, i as i64);
                        continue;
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let packet = NalPacket {
            bytes: data,
            fields,
            location: SourceLocation {
                source: SourceId(1),
                offset: pos as u64,
            },
            generation: 1,
            new_extradata: extra,
        };
        let mut job = match session.packet_observed(packet, update) {
            Ok(v) => v,
            Err(e) => {
                error(e, i as i64);
                continue;
            }
        };
        for _ in 0..1_000_000 {
            match job.step(&mut WorkBudget::new(q), &AtomicBool::new(false)) {
                Err(e) => {
                    if let Some(r) = job.take_receiver_rejection() {
                        if let Some(e) = r.packet_source_refusal() {
                            source_refusal("packet_source_refusal", i as i64, e, &r.source().bytes);
                        }
                        if let Some(e) = r.configuration_source_refusal() {
                            source_refusal(
                                "configuration_source_refusal",
                                i as i64,
                                e,
                                r.configuration_bytes(),
                            );
                        }
                        receiver_failure(
                            "receive",
                            i as i64,
                            r.native_receive_result(),
                            &r.source().bytes,
                        );
                    }
                    error(e, i as i64);
                    break;
                }
                Ok(BuildStep::Yield) => {}
                Ok(BuildStep::Complete) => return Err(Error::Invalid("missing packet")),
                Ok(BuildStep::Ready(v)) => {
                    if let Some(e) = v.packet_source_refusal() {
                        source_refusal("packet_source_refusal", i as i64, e, &v.source().bytes);
                    }
                    if let Some(e) = v.configuration_source_refusal() {
                        source_refusal(
                            "configuration_source_refusal",
                            i as i64,
                            e,
                            v.configuration_bytes(),
                        );
                    }
                    let fields = v.source().fields;
                    let side = if v.forward_new_extradata() {
                        v.source()
                            .new_extradata
                            .as_ref()
                            .map(|b| format!("{{\"type\":1,\"bytes\":\"{}\"}}", hex(b)))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    println!(
                        "{{\"event\":\"packet\",\"input_event\":{i},\"output\":{},\"pts\":{},\"dts\":{},\"duration\":{},\"pos\":{},\"flags\":{},\"stream\":{},\"time_base\":[1,90000],\"payload\":\"{}\",\"side_data\":[{side}]}}",
                        kept.len(),
                        fields.pts,
                        fields.dts,
                        fields.duration,
                        fields.position,
                        fields.flags as i32,
                        fields.stream_index,
                        hex(v.bytes())
                    );
                    println!(
                        "{{\"event\":\"source\",\"input_event\":{i},\"raw\":\"{}\",\"receiver_epoch\":{},\"configuration\":\"{}\",\"new_extradata\":{}}}",
                        hex(&v.source().bytes),
                        v.receiver_epoch(),
                        hex(v.configuration_bytes()),
                        v.source()
                            .new_extradata
                            .as_ref()
                            .map_or_else(|| "null".to_string(), |b| format!("\"{}\"", hex(b)))
                    );
                    kept.try_push(v).map_err(|e| e.error)?;
                    break;
                }
            }
        }
    }
    drop(session);
    for (i, p) in kept.as_slice().iter().enumerate() {
        println!(
            "{{\"event\":\"retained\",\"output\":{i},\"payload\":\"{}\",\"source\":\"{}\"}}",
            hex(p.bytes()),
            hex(&p.source().bytes)
        );
    }
    Ok(())
}
