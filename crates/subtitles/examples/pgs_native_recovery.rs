//! Original correctness-only case adapter. No benchmark mode or dependencies.
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, TimeBase, WorkBudget};
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayLimits, DisplaySet, NATIVE_RECOVERY, NativeColourOrigin,
    NativeRecoveryDecoder, PacketClock, RecoveredEvent, RecoveryStatus,
};

const MAX_INPUT: usize = 16 * 1024 * 1024;

enum Operation {
    Packet { data: Vec<u8>, clock: PacketClock },
    Reset,
}
struct Case {
    operations: Vec<Operation>,
    forced_only: bool,
}
fn u32be(input: &mut impl Read) -> Result<u32, String> {
    let mut b = [0; 4];
    input.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(u32::from_be_bytes(b))
}
fn i64be(input: &mut impl Read) -> Result<i64, String> {
    let mut b = [0; 8];
    input.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(i64::from_be_bytes(b))
}
fn byte(input: &mut impl Read) -> Result<u8, String> {
    let mut b = [0];
    input.read_exact(&mut b).map_err(|e| e.to_string())?;
    Ok(b[0])
}
fn read_case(path: &Path) -> Result<Case, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > u64::try_from(MAX_INPUT + 4096).unwrap()
    {
        return Err("case input limit".into());
    }
    let mut input = BufReader::new(file);
    let mut magic = [0; 8];
    input.read_exact(&mut magic).map_err(|e| e.to_string())?;
    let extended = match &magic {
        b"SMPGS001" => false,
        b"SMPGS002" => true,
        _ => return Err("case magic".into()),
    };
    let count = u32be(&mut input)? as usize;
    if count == 0 || count > 64 {
        return Err("case record limit".into());
    }
    let timebase = TimeBase::new(u64::from(u32be(&mut input)?), u64::from(u32be(&mut input)?))
        .map_err(|e| e.to_string())?;
    let flags = if extended { u32be(&mut input)? } else { 0 };
    if flags & !1 != 0 {
        return Err("unknown case configuration".into());
    }
    let mut operations = Vec::new();
    let mut total = 0usize;
    for _ in 0..count {
        let operation = if extended { byte(&mut input)? } else { 0 };
        if operation == 1 {
            operations.push(Operation::Reset);
            continue;
        }
        if operation != 0 {
            return Err("unknown case operation".into());
        }
        let known = byte(&mut input)?;
        let pts = i64be(&mut input)?;
        if known > 1 {
            return Err("PTS presence flag".into());
        }
        let dts = if extended {
            let known = byte(&mut input)?;
            let value = i64be(&mut input)?;
            if known > 1 {
                return Err("DTS presence flag".into());
            }
            if known == 1 { Some(value) } else { None }
        } else {
            None
        };
        let length = u32be(&mut input)? as usize;
        total = total.checked_add(length).ok_or("case size overflow")?;
        if length == 0 || total > MAX_INPUT {
            return Err("case payload limit".into());
        }
        let mut data = vec![0; length];
        input.read_exact(&mut data).map_err(|e| e.to_string())?;
        operations.push(Operation::Packet {
            data,
            clock: PacketClock {
                pts: if known == 1 { Some(pts) } else { None },
                dts,
                timebase,
            },
        });
    }
    let mut extra = [0];
    if input.read(&mut extra).map_err(|e| e.to_string())? != 0 {
        return Err("trailing case bytes".into());
    }
    Ok(Case {
        operations,
        forced_only: flags & 1 != 0,
    })
}

fn hex(bytes: &[u8]) {
    print!("\"{}\"", triergon_harness_protocol::hex(bytes));
}
fn integer(value: Option<i64>) {
    match value {
        Some(v) => print!("{v}"),
        None => print!("null"),
    }
}
fn string(value: &str) {
    print!("\"");
    for ch in value.chars() {
        match ch {
            '"' => print!("\\\""),
            '\\' => print!("\\\\"),
            '\n' => print!("\\n"),
            '\r' => print!("\\r"),
            '\t' => print!("\\t"),
            c if c < ' ' => print!("\\u{:04x}", u32::from(c)),
            c => print!("{c}"),
        }
    }
    print!("\"");
}
fn emit_source(display: &DisplaySet, packet: usize, consumed: usize) {
    print!(
        "{{\"kind\":\"source-subtitle\",\"packet\":{packet},\"packet_offset\":0,\"consumed\":{consumed},\"flush\":false,\"format\":0,\"pts_microseconds\":"
    );
    integer(display.native_pts_microseconds);
    print!(
        ",\"start_display_ms\":0,\"end_display_ms\":4294967295,\"context_canvas\":[{},{}],\"rectangles\":[",
        display.presentation.width, display.presentation.height
    );
    for (order, object) in display.objects().iter().enumerate() {
        if order != 0 {
            print!(",");
        }
        print!(
            "{{\"order\":{order},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"flags\":{},\"native_stride\":{},\"indices_hex\":",
            object.native_x,
            object.native_y,
            object.object_width,
            object.object_height,
            u8::from(object.composition.forced()),
            object.object_width
        );
        hex(object.pixels());
        print!(",\"palette_entries\":256,\"raw_native_palette_hex\":\"");
        for index in 0..=u8::MAX {
            for b in object.native_argb(index).to_ne_bytes() {
                print!("{b:02x}");
            }
        }
        print!("\",\"palette_argb_u32\":[");
        for index in 0..=u8::MAX {
            if index != 0 {
                print!(",");
            }
            print!("{}", object.native_argb(index));
        }
        print!("],\"argb_hex\":");
        hex(object.native_argb_pixels());
        print!(
            ",\"source\":{{\"object_id\":{},\"object_version\":{},\"x\":{},\"y\":{},\"window_id\":{},\"window_known\":{},\"flags\":{},\"crop\":",
            object.composition.id,
            object.object_version,
            object.composition.x,
            object.composition.y,
            object.composition.window,
            object.visibility_window_known,
            object.composition.flags
        );
        match object.composition.crop {
            None => print!("null"),
            Some(c) => print!("[{},{},{},{}]", c.x, c.y, c.width, c.height),
        }
        print!(",\"window\":");
        match object.window {
            None => print!("null"),
            Some(w) => print!("[{},{},{},{},{}]", w.id, w.x, w.y, w.width, w.height),
        }
        print!(
            ",\"visible\":[{},{},{},{},{},{}],\"visible_indices_hex\":",
            object.visible.x,
            object.visible.y,
            object.visible.width,
            object.visible.height,
            object.visible.source_x,
            object.visible.source_y
        );
        hex(object.visible_pixels());
        print!(",\"colour_matrix_metadata\":null,\"palette_source\":[");
        for index in 0..=u8::MAX {
            if index != 0 {
                print!(",");
            }
            match object.palette().source(index) {
                None => print!("null"),
                Some(c) => {
                    print!(
                        "{{\"y\":{},\"cr\":{},\"cb\":{},\"alpha\":{},\"version\":{},\"epoch\":{},\"packet\":{},\"pts_ticks\":",
                        c.y,
                        c.cr,
                        c.cb,
                        c.alpha,
                        c.palette_version,
                        c.epoch,
                        c.stamp.packet_sequence
                    );
                    integer(c.stamp.packet_clock.pts);
                    print!(",\"dts_ticks\":");
                    integer(c.stamp.packet_clock.dts);
                    print!(
                        ",\"timebase\":[{},{}]",
                        c.stamp.packet_clock.timebase.numerator(),
                        c.stamp.packet_clock.timebase.denominator()
                    );
                    print!(",\"receiver_matrix\":");
                    string(c.receiver_matrix.identifier());
                    print!("}}");
                }
            }
        }
        print!("],\"native_colour_origin\":[");
        for index in 0..=u8::MAX {
            if index != 0 {
                print!(",");
            }
            string(match object.palette().native_origin(index) {
                NativeColourOrigin::ExplicitSource => "explicit-source",
                NativeColourOrigin::PreviousEpochSlot => "previous-epoch-physical-slot",
                NativeColourOrigin::ReceiverZeroInitialization => "receiver-zero-initialization",
            });
        }
        print!("]}}}}");
    }
    print!(
        "],\"source_display\":{{\"generation\":{},\"epoch\":{},\"composition_number\":{},\"state\":{},\"palette_update_flags\":{},\"source_end\":null,\"native_time_error\":",
        display.generation,
        display.epoch,
        display.presentation.number,
        display.presentation.raw_state,
        display.presentation.palette_update_flags
    );
    match display.native_time_error {
        None => print!("null"),
        Some(error) => string(&error.to_string()),
    }
    print!(",\"presentation_pts_ticks\":");
    integer(display.presentation.stamp.packet_clock.pts);
    print!(",\"presentation_dts_ticks\":");
    integer(display.presentation.stamp.packet_clock.dts);
    print!(
        ",\"presentation_timebase\":[{},{}],\"end_pts_ticks\":",
        display.presentation.stamp.packet_clock.timebase.numerator(),
        display
            .presentation
            .stamp
            .packet_clock
            .timebase
            .denominator()
    );
    integer(display.end_stamp.packet_clock.pts);
    print!(",\"segments\":[");
    for (index, segment) in display.segments().iter().enumerate() {
        if index != 0 {
            print!(",");
        }
        print!(
            "{{\"kind\":{},\"packet\":{},\"offset\":{},\"pts_ticks\":",
            segment.kind, segment.stamp.packet_sequence, segment.stamp.packet_offset
        );
        integer(segment.stamp.packet_clock.pts);
        print!(",\"dts_ticks\":");
        integer(segment.stamp.packet_clock.dts);
        print!(
            ",\"timebase\":[{},{}]",
            segment.stamp.packet_clock.timebase.numerator(),
            segment.stamp.packet_clock.timebase.denominator()
        );
        print!(",\"raw_hex\":");
        hex(&segment.raw);
        print!("}}");
    }
    println!("]}}}}");
}

fn emit_recovered(event: &RecoveredEvent, canvas: [u16; 2], packet: usize, consumed: usize) {
    print!(
        "{{\"kind\":\"subtitle\",\"packet\":{packet},\"packet_offset\":0,\"consumed\":{consumed},\"flush\":false,\"format\":0,\"pts_microseconds\":"
    );
    integer(event.pts_microseconds);
    print!(
        ",\"start_display_ms\":0,\"end_display_ms\":4294967295,\"context_canvas\":[{},{}],\"rectangles\":[",
        canvas[0], canvas[1]
    );
    for (i, r) in event.rectangles().iter().enumerate() {
        if i != 0 {
            print!(",");
        }
        print!(
            "{{\"order\":{i},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"flags\":{},\"native_stride\":{},\"indices_hex\":",
            r.x,
            r.y,
            r.width,
            r.height,
            u8::from(r.forced),
            r.stride
        );
        hex(r.pixels());
        print!(",\"palette_entries\":256,\"raw_native_palette_hex\":\"");
        for word in r.palette() {
            for b in word.to_le_bytes() {
                print!("{b:02x}");
            }
        }
        print!("\",\"palette_argb_u32\":[");
        for (j, word) in r.palette().iter().enumerate() {
            if j != 0 {
                print!(",");
            }
            print!("{word}");
        }
        print!("],\"argb_hex\":\"");
        for index in r.pixels().as_ref() {
            for b in r.palette()[usize::from(*index)].to_be_bytes() {
                print!("{b:02x}");
            }
        }
        print!("\"}}");
    }
    print!("],\"projection\":");
    string(NATIVE_RECOVERY);
    println!("}}");
}
fn run(path: &Path, quantum: u64) -> Result<(), String> {
    if quantum == 0 {
        return Err("positive work quantum required".into());
    }
    let case = read_case(path)?;
    let b = Budget::new(256 * 1024 * 1024).map_err(|e| e.to_string())?;
    let c = AtomicBool::new(false);
    let mut decoder = NativeRecoveryDecoder::new(
        &b,
        DisplayLimits::default(),
        DisplayConfig {
            native_forced_only: case.forced_only,
        },
    )
    .map_err(|e| e.to_string())?;
    println!(
        "{{\"kind\":\"candidate-identity\",\"operation\":\"explicit-NativeD32-recovery-with-original-Strict-sidecar-v1\",\"TIME\":false,\"MEM\":false,\"whole_PGS_parity\":false}}"
    );
    for (index, op) in case.operations.into_iter().enumerate() {
        match op {
            Operation::Reset => {
                decoder.reset(&c).map_err(|e| e.to_string())?;
                println!("{{\"kind\":\"hard-reset\",\"operation\":{index}}}");
            }
            Operation::Packet { data, clock } => {
                let size = data.len();
                print!("{{\"kind\":\"source-packet\",\"packet\":{index},\"pts_ticks\":");
                integer(clock.pts);
                print!(",\"dts_ticks\":");
                integer(clock.dts);
                print!(",\"payload_hex\":");
                hex(&data);
                println!("}}");
                let mut raw =
                    Some(Bytes::try_from_vec(&b, data).map_err(|e| e.error().to_string())?);
                decoder
                    .push(&mut raw, clock, &c)
                    .map_err(|e| e.to_string())?;
                let mut complete = false;
                for _ in 0..100_000_000 {
                    if decoder
                        .poll(&mut WorkBudget::new(quantum), &c)
                        .map_err(|e| e.to_string())?
                        == RecoveryStatus::OutputReady
                    {
                        let out = decoder.take_output(&c).map_err(|e| e.to_string())?;
                        for source in out.source_sets() {
                            emit_source(source, index, size);
                        }
                        if let Some(error) = out.source_error() {
                            print!("{{\"kind\":\"source-error\",\"packet\":{index},\"reason\":");
                            string(&error.to_string());
                            println!("}}");
                        }
                        if let Some(event) = out.receiver_output() {
                            emit_recovered(event, out.context_canvas, index, size);
                        }
                        println!(
                            "{{\"kind\":\"packet-projection\",\"packet\":{index},\"source_sets\":{},\"omitted_receiver_events\":{},\"receiver_segment_overread_bytes\":{},\"receiver_padding_bytes\":{},\"receiver_truncated_segment\":{}}}",
                            out.source_sets().len(),
                            out.omitted_receiver_events(),
                            out.receiver_observations.palette_cross_segment_bytes,
                            out.receiver_observations.palette_zero_padding_bytes,
                            out.receiver_observations.truncated_segment_tail
                        );
                        complete = true;
                        break;
                    }
                }
                if !complete {
                    return Err("receiver progress limit".into());
                }
            }
        }
    }
    decoder.finish_input(&c).map_err(|e| e.to_string())?;
    let mut complete = false;
    for _ in 0..100_000_000 {
        if decoder
            .poll(&mut WorkBudget::new(quantum), &c)
            .map_err(|e| e.to_string())?
            == RecoveryStatus::Ended
        {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err("receiver EOF progress limit".into());
    }
    if let Some(report) = decoder.eof_report() {
        print!("{{\"kind\":\"source-finalization\",\"error\":");
        match report.source_error {
            Some(e) => string(&e.to_string()),
            None => print!("null"),
        }
        println!(",\"native_delayed_events\":0}}");
    }
    println!("{{\"kind\":\"complete\",\"qualification_accepted\":false}}");
    Ok(())
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !(args.len() == 2 || args.len() == 3) || args[0] != "--case" {
        eprintln!("usage: pgs_native_recovery --case FILE [WORK_QUANTUM]");
        std::process::exit(2);
    }
    let q = if args.len() == 3 {
        args[2].parse::<u64>().unwrap_or(0)
    } else {
        4096
    };
    if let Err(error) = run(Path::new(&args[1]), q) {
        print!("{{\"kind\":\"error\",\"stage\":\"candidate\",\"code\":5,\"reason\":");
        string(&error);
        println!(",\"qualification_accepted\":false}}");
        std::process::exit(5);
    }
}
