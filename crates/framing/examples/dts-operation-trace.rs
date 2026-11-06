//! Complete functional observations. No case IDs, expected records or native golds.
#[path = "support/dts_operation_facts.rs"]
mod facts;
use std::{io::Read, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::compat::dts::{
    FactAssociationJob, ParserOperation, ParserOperationEvent, ParserOperationLimits,
    ParserOperationStep, ParserPacket,
};
use triergon_framing::{
    Dts14BoundaryEvidence, Dts14CarrierBoundary, Dts14CarrierLimits, Dts14Frame, Dts14SyncLimits,
    DtsWordOrder,
};
use triergon_harness_protocol::hex;

fn header(h: triergon_framing::DtsCoreHeader) -> String {
    let values = facts::values(h);
    let mut fields = Vec::new();
    let _ = facts::WIDTHS;
    for (name, value) in facts::HEADER_NAMES.into_iter().zip(values) {
        let text = if !value.present {
            "null".to_owned()
        } else if name == "open_bitrate" {
            (value.bits != 0).to_string()
        } else {
            format!("{}{}", if value.negative { "-" } else { "" }, value.bits)
        };
        fields.push(format!("\"{name}\":{text}"));
    }
    format!("{{{}}}", fields.join(","))
}
fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
fn source_record(f: &Dts14Frame) -> String {
    let a = f.facts();
    let end = a.identity.offset + f.physical_bytes().len() as u64;
    let boundary = match a.boundary {
        Dts14BoundaryEvidence::FollowingNormalSync { physical_offset } => {
            format!("{{\"kind\":\"following-normal-sync\",\"offset\":{physical_offset}}}")
        }
        Dts14BoundaryEvidence::ExplicitSourceEnd => {
            format!("{{\"kind\":\"entity-end\",\"offset\":{end}}}")
        }
    };
    format!(
        "{{\"event\":\"source-frame\",\"source_id\":{},\"generation\":{},\"physical_start\":{},\"physical_end\":{},\"skipped_interior_syncs\":{},\"boundary\":{},\"raw_hex\":\"{}\",\"canonical_hex\":\"{}\",\"header35_flat36\":{}}}",
        a.identity.source.0,
        a.identity.generation,
        a.identity.offset,
        end,
        a.skipped_interior_syncs,
        boundary,
        hex(f.physical_bytes().as_ref()),
        hex(f.canonical_bytes().as_ref()),
        header(a.header)
    )
}
fn parser_record(p: &ParserPacket) -> String {
    let range = p.physical_range();
    let boundary = match p.packet().boundary {
        Dts14CarrierBoundary::FollowingCarrierSync { offset } => {
            format!("{{\"kind\":\"following-carrier-sync\",\"offset\":{offset}}}")
        }
        Dts14CarrierBoundary::ExplicitSourceEnd => {
            format!("{{\"kind\":\"entity-end\",\"offset\":{}}}", range.end)
        }
    };
    let canonical = p
        .canonical_bytes()
        .map_or_else(|| "null".to_owned(), |b| format!("\"{}\"", hex(b.as_ref())));
    let source_header = p.source_header().map_or_else(|| "null".to_owned(), header);
    let clock = match p.clock() {
        Ok(c) => format!(
            "{{\"duration\":{},\"sample_rate\":{},\"profile\":{},\"disposition\":\"{:?}\"}}",
            c.duration, c.codec_sample_rate, c.profile, c.disposition
        ),
        Err(e) => format!("{{\"refusal\":{:?}}}", format!("{e:?}")),
    };
    let hints = match p.parser_hints() {
        Ok(h) => format!(
            "{{\"frame_offset\":{},\"consumed_offset\":{},\"opaque_next_offset\":{}}}",
            h.frame_offset, h.consumed_offset, h.opaque_next_offset
        ),
        Err(e) => format!("{{\"refusal\":{:?}}}", format!("{e:?}")),
    };
    format!(
        "{{\"event\":\"parser-packet\",\"source_id\":{},\"generation\":{},\"physical_start\":{},\"physical_end\":{},\"packet_boundary\":{},\"raw_hex\":\"{}\",\"canonical_hex\":{},\"source_header35\":{},\"source_syntax\":{:?},\"native_clock\":{},\"parser_hints\":{}}}",
        p.identity().source.0,
        p.identity().generation,
        range.start,
        range.end,
        boundary,
        hex(p.source_bytes().as_ref()),
        canonical,
        source_header,
        format!("{:?}", p.source_syntax()),
        clock,
        hints
    )
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 7 || args[1] != "dts-operation-trace" {
        return Err(
            "functional interface: operation FILE big|little CHUNK previous|all QUANTUM".into(),
        );
    }
    let order = match args[3].as_str() {
        "big" => DtsWordOrder::BigEndian,
        "little" => DtsWordOrder::LittleEndian,
        _ => return Err("order".into()),
    };
    let chunk: usize = args[4].parse()?;
    let all = match args[5].as_str() {
        "all" => true,
        "previous" => false,
        _ => return Err("retention".into()),
    };
    let quantum: u64 = args[6].parse()?;
    if chunk == 0 || chunk > 4096 || quantum == 0 || quantum > 1_000_000 {
        return Err("work/window policy".into());
    }
    let mut raw = Vec::new();
    std::fs::File::open(&args[2])?
        .take(60001)
        .read_to_end(&mut raw)?;
    if raw.len() > 60000 {
        return Err("source limit".into());
    }
    let pool = Budget::new(32 * 1024 * 1024)?;
    let baseline = Budget::control_storage_bytes();
    let limits = ParserOperationLimits {
        source: Dts14SyncLimits {
            maximum_physical_frame_bytes: 60000,
            maximum_canonical_frame_bytes: 60000,
            maximum_frames: 128,
            maximum_input_window: 4096,
        },
        parser: Dts14CarrierLimits {
            maximum_packet_bytes: 60000,
            maximum_packets: 128,
            maximum_input_window: 4096,
        },
    };
    let mut op = ParserOperation::new(
        &pool,
        SourceId(21186),
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        order,
        limits,
    )?;
    let context = op.context();
    let cancel = AtomicBool::new(false);
    let mut source_held: Vec<(Dts14Frame, String)> = Vec::new();
    let mut parser_held: Vec<(ParserPacket, String)> = Vec::new();
    let mut counts = [0usize; 4];
    let mut ended = false;
    println!(
        "{{\"event\":\"begin\",\"source_hex\":\"{}\",\"source_generation\":{},\"parser_generation\":{},\"retention_all\":{}}}",
        hex(&raw),
        context.source_generation(),
        context.parser_generation(),
        all
    );
    for _ in 0..20_000_000 {
        match op.step(&mut WorkBudget::new(quantum), &cancel)? {
            ParserOperationStep::Yield => {}
            ParserOperationStep::NeedInput { lane, request } => {
                let at = usize::try_from(request.offset)?;
                if at > raw.len() {
                    return Err("request beyond source".into());
                }
                if at == raw.len() {
                    op.finish(lane, request.source, request.token)?;
                    counts[2] += 1;
                } else {
                    let end = raw.len().min(at + chunk.min(request.maximum));
                    let window = InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        Bytes::copy_from_slice(&pool, &raw[at..end])?,
                    )?;
                    op.provide(lane, &window)?;
                }
            }
            ParserOperationStep::Event => match op.take_event()? {
                ParserOperationEvent::SourceFrame(f) => {
                    let record = source_record(&f);
                    println!("{record}");
                    if !all {
                        source_held.clear();
                    }
                    source_held.push((f, record));
                    counts[0] += 1;
                }
                ParserOperationEvent::ParserPacket(p) => {
                    let record = parser_record(&p);
                    println!("{record}");
                    if !all {
                        parser_held.clear();
                    }
                    parser_held.push((p, record));
                    counts[1] += 1;
                }
                ParserOperationEvent::ViewRefusal {
                    lane,
                    error,
                    request,
                } => {
                    counts[3] += 1;
                    println!(
                        "{{\"event\":\"view-refusal\",\"lane\":\"{lane:?}\",\"error\":{:?},\"offset\":{}}}",
                        format!("{error:?}"),
                        request.offset
                    );
                }
            },
            ParserOperationStep::End { source, parser } => {
                println!(
                    "{{\"event\":\"ends\",\"source\":{:?},\"parser\":{:?}}}",
                    format!("{source:?}"),
                    format!("{parser:?}")
                );
                ended = true;
                break;
            }
        }
    }
    if !ended {
        return Err("functional step bound".into());
    }
    drop(op);
    raw.fill(0xa5);
    for (f, record) in &source_held {
        if &source_record(f) != record {
            return Err("afterclose source changed".into());
        }
    }
    for (p, record) in &parser_held {
        if &parser_record(p) != record {
            return Err("afterclose parser changed".into());
        }
    }
    let mut associations = 0;
    for (f, _) in &source_held {
        for (p, _) in &parser_held {
            let r = p.physical_range();
            let a = f.facts().identity.offset;
            let b = a + f.physical_bytes().len() as u64;
            if a >= r.start && b <= r.end {
                let mut job = FactAssociationJob::new(&pool, context, f, p)?;
                loop {
                    match job.step(&mut WorkBudget::new(quantum), &cancel)? {
                        Step::Ready(a) => {
                            let _ = digest(a.source_frame().physical_bytes().as_ref());
                            associations += 1;
                            break;
                        }
                        Step::Yield => {}
                        _ => return Err("association state".into()),
                    }
                }
            }
        }
    }
    let held = [source_held.len(), parser_held.len()];
    drop(source_held);
    drop(parser_held);
    println!(
        "{{\"event\":\"summary\",\"source_frames\":{},\"parser_packets\":{},\"EOF_receipts\":{},\"view_refusals\":{},\"retained_source\":{},\"retained_parser\":{},\"byte_proved_associations\":{},\"afterclose_stable\":true,\"source_overwritten\":true,\"pool_final\":{},\"pool_baseline\":{},\"full35_native_parity\":false}}",
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        held[0],
        held[1],
        associations,
        pool.used(),
        baseline
    );
    if pool.used() != baseline {
        return Err(Error::Invalid("unreleased owners").into());
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(64);
    }
}
